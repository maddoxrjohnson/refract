//! Walking the source folder for the manifest S0 (spec §4.2): every file and folder
//! with its size, modification time and attributes, the build folders left out with
//! their sizes, links that leave the repo, and files that can't be read.
//!
//! The file system is behind [`SourceFs`], so the rules here are tested with an
//! in-memory folder; `win::NtfsFs` reads NTFS natively.
//!
//! Links are reparse points whose tag is a name surrogate (symlinks, junctions and the
//! like). Other reparse points, such as OneDrive placeholders, are read through as the
//! files and folders they stand for. A link is kept as a symlink entry only when its
//! target is relative and stays inside the repo; any other one is listed in
//! [`Walk::external_links`], which refuses the migration. That includes absolute
//! targets inside the repo, and so every junction: after the move they would point
//! at the deleted original.

use std::collections::BTreeSet;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use refract_core::migrate::build::{skip_rule, DirFacts, SkippedFolder};
use refract_core::migrate::manifest::{DroppedStream, Entry, EntryKind, Manifest, RelPath};
use serde::Serialize;

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// Set in the tag of reparse points that name another file or folder.
const NAME_SURROGATE: u32 = 0x2000_0000;

/// How often hashing reports progress.
const PROGRESS_EVERY: Duration = Duration::from_millis(200);
/// How often the thread waiting for the readers wakes up.
const POLL: Duration = Duration::from_millis(20);
/// Most threads hashing at once.
const MAX_THREADS: usize = 8;

/// One entry of a folder listing, as `FindFirstFileExW` gives it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RawEntry {
    /// The name, UTF-16 as Windows stores it.
    pub name: Vec<u16>,
    pub attributes: u32,
    pub size: u64,
    /// Last write time, nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// The reparse tag, when `attributes` has [`FILE_ATTRIBUTE_REPARSE_POINT`].
    pub reparse_tag: u32,
}

impl RawEntry {
    pub fn is_dir(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    /// A symlink, junction or other reparse point naming something else.
    pub fn is_link(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            && self.reparse_tag & NAME_SURROGATE != 0
    }
}

/// Why a file could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// Opening or reading failed: the file is locked, gone or not allowed.
    Unreadable(String),
    Cancelled,
}

/// The source folder, by paths relative to its root (`/`-separated, `""` for the root).
pub trait SourceFs: Sync {
    /// The folder's entries, without `.` and `..`.
    fn list(&self, rel: &str) -> io::Result<Vec<RawEntry>>;
    /// A link's target as stored.
    fn read_link(&self, rel: &str) -> io::Result<String>;
    /// Named alternate data streams of a file or folder: each name (without the leading
    /// `:` or the `:$DATA` suffix) and size.
    fn streams(&self, rel: &str) -> io::Result<Vec<(String, u64)>>;
    /// Reads the file through and returns its lowercase hex SHA-256, calling `read`
    /// with each chunk's length. Stops with [`ReadError::Cancelled`] once `cancel` is set.
    fn hash(&self, rel: &str, cancel: &AtomicBool, read: &dyn Fn(u64))
        -> Result<String, ReadError>;
    /// Opens the file and reads its first bytes, to tell whether it can be read.
    fn probe(&self, rel: &str) -> Result<(), ReadError>;
    /// The start of a small text file, up to `max` bytes, lossily decoded.
    fn read_text(&self, rel: &str, max: usize) -> io::Result<String>;
}

/// What [`walk`] found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Walk {
    /// Entries without hashes or blob ids yet, sorted by path; skipped folders and
    /// unrepresentable names filled in. Streams are added by [`read_files`] (files)
    /// and here (folders).
    pub manifest: Manifest,
    /// Links that point outside the repo, or whose target can't be read.
    pub external_links: Vec<String>,
    /// Folders that could not be listed.
    pub unreadable: Vec<String>,
}

/// The walk was stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

/// Walks the whole source. `ignored` holds the folders git ignores itself (paths
/// relative to the top repo, no trailing `/`), which [`skip_rule`] needs.
pub fn walk(
    fs: &dyn SourceFs,
    ignored: &BTreeSet<String>,
    cancel: &AtomicBool,
) -> Result<Walk, Cancelled> {
    let mut out = Walk::default();
    let root = match fs.list("") {
        Ok(listing) => listing,
        Err(err) => {
            out.unreadable.push(format!(". ({err})"));
            return Ok(out);
        }
    };
    // Folders to visit: path, inside a `.git`, listing.
    let mut stack: Vec<(String, bool, Vec<RawEntry>)> = vec![(String::new(), false, root)];
    while let Some((dir, in_git, listing)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled);
        }
        let has_cargo_toml = listing
            .iter()
            .any(|e| !e.is_dir() && name_is(e, "Cargo.toml"));
        for raw in &listing {
            let Some(name) = String::from_utf16(&raw.name)
                .ok()
                .filter(|n| !n.contains('/'))
            else {
                let lossy = String::from_utf16_lossy(&raw.name);
                out.manifest.unrepresentable.push(join(&dir, &lossy));
                continue;
            };
            let Ok(path) = RelPath::new(join(&dir, &name)) else {
                out.manifest.unrepresentable.push(join(&dir, &name));
                continue;
            };
            if raw.is_link() {
                match fs.read_link(path.as_str()) {
                    Ok(target) => match link_inside(&path, &target) {
                        Some(stored) => out.manifest.entries.push(Entry {
                            path,
                            kind: EntryKind::Symlink,
                            // As the helper's stat records it: sizes of links are not
                            // compared.
                            size: 0,
                            mtime_ns: raw.mtime_ns,
                            attributes: raw.attributes,
                            sha256: None,
                            blob: None,
                            link_target: Some(stored),
                        }),
                        None => out.external_links.push(path.to_string()),
                    },
                    Err(_) => out.external_links.push(path.to_string()),
                }
                continue;
            }
            if !raw.is_dir() {
                out.manifest.entries.push(entry(path, EntryKind::File, raw));
                continue;
            }
            let child = match fs.list(path.as_str()) {
                Ok(child) => child,
                Err(err) => {
                    out.unreadable.push(format!("{path} ({err})"));
                    continue;
                }
            };
            let child_in_git = in_git || name == ".git";
            if !child_in_git {
                let facts = DirFacts {
                    name: &name,
                    ignored: ignored.contains(path.as_str()),
                    has_pyvenv_cfg: child
                        .iter()
                        .any(|e| !e.is_dir() && name_is(e, "pyvenv.cfg")),
                    has_cachedir_tag: child
                        .iter()
                        .any(|e| !e.is_dir() && name_is(e, "CACHEDIR.TAG")),
                    sibling_cargo_toml: has_cargo_toml,
                };
                if let Some(rule) = skip_rule(&facts) {
                    let (bytes, files) = folder_size(fs, path.as_str(), child);
                    out.manifest.skipped.push(SkippedFolder {
                        path,
                        rule,
                        bytes,
                        files,
                    });
                    continue;
                }
            }
            if let Ok(streams) = fs.streams(path.as_str()) {
                add_streams(&mut out.manifest.streams, &path, streams);
            }
            out.manifest
                .entries
                .push(entry(path.clone(), EntryKind::Dir, raw));
            stack.push((path.to_string(), child_in_git, child));
        }
    }
    out.manifest.entries.sort_by(|a, b| a.path.cmp(&b.path));
    out.manifest.skipped.sort_by(|a, b| a.path.cmp(&b.path));
    out.manifest.unrepresentable.sort();
    out.external_links.sort();
    out.unreadable.sort();
    Ok(out)
}

fn name_is(entry: &RawEntry, name: &str) -> bool {
    entry.name.iter().copied().eq(name.encode_utf16())
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn entry(path: RelPath, kind: EntryKind, raw: &RawEntry) -> Entry {
    Entry {
        path,
        kind,
        size: if kind == EntryKind::File { raw.size } else { 0 },
        mtime_ns: raw.mtime_ns,
        attributes: raw.attributes,
        sha256: None,
        blob: None,
        link_target: None,
    }
}

fn add_streams(out: &mut Vec<DroppedStream>, path: &RelPath, streams: Vec<(String, u64)>) {
    out.extend(streams.into_iter().map(|(stream, size)| DroppedStream {
        path: path.clone(),
        stream,
        size,
    }));
}

/// Bytes and files inside a skipped folder, not following links. Folders that can't
/// be listed count as empty: the folder is not copied either way.
fn folder_size(fs: &dyn SourceFs, dir: &str, listing: Vec<RawEntry>) -> (u64, u64) {
    let (mut bytes, mut files) = (0, 0);
    let mut stack = vec![(dir.to_owned(), listing)];
    while let Some((dir, listing)) = stack.pop() {
        for raw in listing {
            if raw.is_dir() && !raw.is_link() {
                let path = join(&dir, &String::from_utf16_lossy(&raw.name));
                if let Ok(child) = fs.list(&path) {
                    stack.push((path, child));
                }
            } else {
                bytes += raw.size;
                files += 1;
            }
        }
    }
    (bytes, files)
}

/// The target to record for a link at `link`, `/`-separated, when it stays inside the
/// repo; `None` when it leaves it, is absolute (a drive, a share, a rooted path or a
/// verbatim `\\?\` one), or is empty. A relative target is resolved from the link's
/// folder, and passing above the repo root at any point counts as leaving it.
pub fn link_inside(link: &RelPath, target: &str) -> Option<String> {
    let bytes = target.as_bytes();
    let absolute = target.is_empty()
        || target.starts_with(['\\', '/'])
        || (bytes.len() >= 2 && bytes[1] == b':');
    if absolute {
        return None;
    }
    let mut parts: Vec<&str> = link.parent().map_or(Vec::new(), |p| p.split('/').collect());
    for part in target.split(['\\', '/']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(target.replace('\\', "/"))
}

/// What [`read_files`] does with each file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    /// SHA-256 of the whole file (the snapshot).
    Hash,
    /// Only check that it opens and reads (the preflight).
    Probe,
}

/// Progress of [`read_files`], also sent to the interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadProgress {
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// Reads every file entry of `manifest` on up to `threads` threads (at most 8): its
/// alternate data streams are added to `manifest.streams`, and in [`ReadMode::Hash`]
/// its SHA-256 is set. Returns the files that could not be read, sorted. `progress`
/// is called about every 200 ms and once at the end.
pub fn read_files(
    fs: &dyn SourceFs,
    manifest: &mut Manifest,
    mode: ReadMode,
    threads: usize,
    cancel: &AtomicBool,
    progress: &(dyn Fn(ReadProgress) + Sync),
) -> Result<Vec<String>, Cancelled> {
    let files: Vec<usize> = (0..manifest.entries.len())
        .filter(|i| manifest.entries[*i].kind == EntryKind::File)
        .collect();
    let total = ReadProgress {
        files_total: files.len() as u64,
        bytes_total: files.iter().map(|i| manifest.entries[*i].size).sum(),
        ..ReadProgress::default()
    };
    let next = AtomicUsize::new(0);
    let (files_done, bytes_done) = (AtomicU64::new(0), AtomicU64::new(0));
    let finished = AtomicUsize::new(0);
    let threads = threads.clamp(1, MAX_THREADS);
    type Outcome = (usize, Result<Option<String>, ReadError>, Vec<(String, u64)>);
    let results: Mutex<Vec<Outcome>> = Mutex::new(Vec::with_capacity(files.len()));
    let entries = &manifest.entries;
    let snapshot = || ReadProgress {
        files_done: files_done.load(Ordering::Relaxed),
        bytes_done: bytes_done.load(Ordering::Relaxed),
        ..total
    };
    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut mine = Vec::new();
                loop {
                    let n = next.fetch_add(1, Ordering::Relaxed);
                    if n >= files.len() || cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let index = files[n];
                    let path = entries[index].path.as_str();
                    let streams = fs.streams(path).unwrap_or_default();
                    let result = match mode {
                        ReadMode::Hash => fs
                            .hash(path, cancel, &|len| {
                                bytes_done.fetch_add(len, Ordering::Relaxed);
                            })
                            .map(Some),
                        ReadMode::Probe => fs.probe(path).map(|()| {
                            bytes_done.fetch_add(entries[index].size, Ordering::Relaxed);
                            None
                        }),
                    };
                    files_done.fetch_add(1, Ordering::Relaxed);
                    mine.push((index, result, streams));
                }
                results
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(mine);
                finished.fetch_add(1, Ordering::Relaxed);
            });
        }
        let mut last = Instant::now();
        while finished.load(Ordering::Relaxed) < threads {
            thread::sleep(POLL);
            if last.elapsed() >= PROGRESS_EVERY {
                progress(snapshot());
                last = Instant::now();
            }
        }
    });
    progress(snapshot());
    if cancel.load(Ordering::Relaxed) {
        return Err(Cancelled);
    }
    let mut unreadable = Vec::new();
    let mut results = results.into_inner().unwrap_or_else(|e| e.into_inner());
    results.sort_by_key(|(index, ..)| *index);
    for (index, result, streams) in results {
        let path = manifest.entries[index].path.clone();
        add_streams(&mut manifest.streams, &path, streams);
        match result {
            Ok(hash) => manifest.entries[index].sha256 = hash,
            Err(ReadError::Unreadable(why)) => unreadable.push(format!("{path} ({why})")),
            Err(ReadError::Cancelled) => return Err(Cancelled),
        }
    }
    manifest
        .streams
        .sort_by(|a, b| (&a.path, &a.stream).cmp(&(&b.path, &b.stream)));
    Ok(unreadable)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use refract_core::migrate::build::BuildFolder;
    use std::collections::BTreeMap;

    /// An in-memory folder: paths to nodes.
    #[derive(Default)]
    pub struct FakeFs {
        pub nodes: BTreeMap<String, FakeNode>,
    }

    #[derive(Clone)]
    pub enum FakeNode {
        Dir,
        File(Vec<u8>),
        Link(String),
        Locked(u64),
        /// A name that is not valid UTF-16 (an unpaired surrogate) inside this folder.
        BadName,
    }

    impl FakeFs {
        pub fn with(nodes: &[(&str, FakeNode)]) -> FakeFs {
            let mut fs = FakeFs::default();
            for (path, node) in nodes {
                // Parents exist implicitly.
                let mut parent = String::new();
                for part in path.split('/').collect::<Vec<_>>().split_last().unwrap().1 {
                    parent = join(&parent, part);
                    fs.nodes.entry(parent.clone()).or_insert(FakeNode::Dir);
                }
                fs.nodes.insert((*path).to_owned(), node.clone());
            }
            fs
        }
    }

    impl SourceFs for FakeFs {
        fn list(&self, rel: &str) -> io::Result<Vec<RawEntry>> {
            let prefix = if rel.is_empty() {
                String::new()
            } else {
                format!("{rel}/")
            };
            let mut out = Vec::new();
            for (path, node) in &self.nodes {
                let Some(name) = path.strip_prefix(&prefix) else {
                    continue;
                };
                if name.contains('/') || name.is_empty() {
                    continue;
                }
                let mut raw = RawEntry {
                    name: name.encode_utf16().collect(),
                    mtime_ns: 1_700_000_000_000_000_000,
                    ..RawEntry::default()
                };
                match node {
                    FakeNode::Dir => raw.attributes = FILE_ATTRIBUTE_DIRECTORY,
                    FakeNode::File(bytes) => {
                        raw.attributes = 0x20;
                        raw.size = bytes.len() as u64;
                    }
                    FakeNode::Locked(size) => raw.size = *size,
                    FakeNode::Link(_) => {
                        raw.attributes = FILE_ATTRIBUTE_REPARSE_POINT;
                        raw.reparse_tag = 0xA000_000C;
                    }
                    FakeNode::BadName => raw.name = vec![0xD800, b'x' as u16],
                }
                out.push(raw);
            }
            Ok(out)
        }

        fn read_link(&self, rel: &str) -> io::Result<String> {
            match self.nodes.get(rel) {
                Some(FakeNode::Link(target)) => Ok(target.clone()),
                _ => Err(io::Error::other("not a link")),
            }
        }

        fn streams(&self, rel: &str) -> io::Result<Vec<(String, u64)>> {
            Ok(if rel.ends_with(".exe") {
                vec![("Zone.Identifier".into(), 26)]
            } else {
                Vec::new()
            })
        }

        fn hash(
            &self,
            rel: &str,
            _cancel: &AtomicBool,
            read: &dyn Fn(u64),
        ) -> Result<String, ReadError> {
            match self.nodes.get(rel) {
                Some(FakeNode::File(bytes)) => {
                    read(bytes.len() as u64);
                    Ok(format!("sha:{}", String::from_utf8_lossy(bytes)))
                }
                _ => Err(ReadError::Unreadable("locked".into())),
            }
        }

        fn probe(&self, rel: &str) -> Result<(), ReadError> {
            self.hash(rel, &AtomicBool::new(false), &|_| {}).map(|_| ())
        }

        fn read_text(&self, rel: &str, max: usize) -> io::Result<String> {
            match self.nodes.get(rel) {
                Some(FakeNode::File(bytes)) => {
                    Ok(String::from_utf8_lossy(&bytes[..bytes.len().min(max)]).into_owned())
                }
                _ => Err(io::Error::from(io::ErrorKind::NotFound)),
            }
        }
    }

    fn file(text: &str) -> FakeNode {
        FakeNode::File(text.as_bytes().to_vec())
    }

    fn paths(walk: &Walk) -> Vec<&str> {
        walk.manifest
            .entries
            .iter()
            .map(|e| e.path.as_str())
            .collect()
    }

    fn ignored(dirs: &[&str]) -> BTreeSet<String> {
        dirs.iter().map(|d| (*d).to_owned()).collect()
    }

    pub fn sample() -> FakeFs {
        FakeFs::with(&[
            (".git/HEAD", file("ref: refs/heads/main\n")),
            (".git/objects/ab/cdef", file("blob")),
            // Inside .git no rule applies, even to a folder named like a build folder.
            (".git/node_modules/x", file("x")),
            ("src/main.rs", file("fn main() {}")),
            ("Cargo.toml", file("[package]")),
            ("target/debug/app.exe", file("MZ")),
            ("target/CACHEDIR.TAG", file("Signature")),
            ("web/node_modules/left-pad/index.js", file("module.exports")),
            ("web/node_modules/left-pad/package.json", file("{}")),
            // Not ignored by git: copied even though the name matches.
            ("web/dist/app.js", file("app")),
            ("env/settings.toml", file("x = 1")),
            ("tools/setup.exe", file("MZ")),
            ("docs/latest", FakeNode::Link("v2".into())),
            ("docs/v2/index.md", file("# Docs")),
            ("docs/escape", FakeNode::Link("..\\..\\outside".into())),
            (
                "docs/junction",
                FakeNode::Link("C:\\Users\\dev\\elsewhere".into()),
            ),
            ("data/big.bin", FakeNode::Locked(5)),
            ("odd", FakeNode::Dir),
            ("odd/bad", FakeNode::BadName),
        ])
    }

    #[test]
    fn walks_everything_but_ignored_build_folders() {
        let fs = sample();
        let walk = walk(
            &fs,
            &ignored(&["target", "web/node_modules", "env"]),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            paths(&walk),
            [
                ".git",
                ".git/HEAD",
                ".git/node_modules",
                ".git/node_modules/x",
                ".git/objects",
                ".git/objects/ab",
                ".git/objects/ab/cdef",
                "Cargo.toml",
                "data",
                "data/big.bin",
                "docs",
                "docs/latest",
                "docs/v2",
                "docs/v2/index.md",
                "env",
                "env/settings.toml",
                "odd",
                "src",
                "src/main.rs",
                "tools",
                "tools/setup.exe",
                "web",
                "web/dist",
                "web/dist/app.js",
            ]
        );
        let skipped: Vec<(&str, BuildFolder, u64, u64)> = walk
            .manifest
            .skipped
            .iter()
            .map(|s| (s.path.as_str(), s.rule, s.bytes, s.files))
            .collect();
        assert_eq!(
            skipped,
            [
                ("target", BuildFolder::CargoTarget, 11, 2),
                ("web/node_modules", BuildFolder::NodeModules, 16, 2),
            ]
        );
        assert_eq!(walk.external_links, ["docs/escape", "docs/junction"]);
        assert_eq!(walk.manifest.unrepresentable.len(), 1);
        assert!(walk.manifest.unrepresentable[0].starts_with("odd/"));
        let link = walk
            .manifest
            .entries
            .iter()
            .find(|e| e.path.as_str() == "docs/latest")
            .unwrap();
        assert_eq!(link.kind, EntryKind::Symlink);
        assert_eq!(link.link_target.as_deref(), Some("v2"));
        assert_eq!(link.size, 0);
    }

    #[test]
    fn hashing_fills_hashes_streams_and_finds_locked_files() {
        let fs = sample();
        let mut walk = walk(&fs, &ignored(&["target"]), &AtomicBool::new(false)).unwrap();
        let seen = Mutex::new(Vec::new());
        let unreadable = read_files(
            &fs,
            &mut walk.manifest,
            ReadMode::Hash,
            3,
            &AtomicBool::new(false),
            &|p| seen.lock().unwrap().push(p),
        )
        .unwrap();
        assert_eq!(unreadable, ["data/big.bin (locked)"]);
        let main = walk
            .manifest
            .entries
            .iter()
            .find(|e| e.path.as_str() == "src/main.rs")
            .unwrap();
        assert_eq!(main.sha256.as_deref(), Some("sha:fn main() {}"));
        let streams: Vec<&str> = walk
            .manifest
            .streams
            .iter()
            .map(|s| s.path.as_str())
            .collect();
        assert_eq!(streams, ["tools/setup.exe"]);
        let last = *seen.lock().unwrap().last().unwrap();
        assert_eq!(last.files_done, last.files_total);
        assert_eq!(last.files_total, 12);
        // The locked file counts as done but its bytes were never read.
        assert_eq!(last.bytes_done, last.bytes_total - 5);
    }

    #[test]
    fn probing_reads_nothing_into_the_manifest() {
        let fs = sample();
        let mut walk = walk(&fs, &ignored(&[]), &AtomicBool::new(false)).unwrap();
        let unreadable = read_files(
            &fs,
            &mut walk.manifest,
            ReadMode::Probe,
            2,
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap();
        assert_eq!(unreadable, ["data/big.bin (locked)"]);
        assert!(walk.manifest.entries.iter().all(|e| e.sha256.is_none()));
    }

    #[test]
    fn a_cancelled_walk_or_read_stops() {
        let fs = sample();
        let cancel = AtomicBool::new(true);
        assert_eq!(walk(&fs, &ignored(&[]), &cancel), Err(Cancelled));
        let mut walk = walk(&fs, &ignored(&[]), &AtomicBool::new(false)).unwrap();
        assert_eq!(
            read_files(&fs, &mut walk.manifest, ReadMode::Hash, 2, &cancel, &|_| {}),
            Err(Cancelled)
        );
    }

    #[test]
    fn links_stay_inside_only_when_relative_and_below_the_root() {
        let at = |p: &str| RelPath::new(p).unwrap();
        assert_eq!(link_inside(&at("docs/latest"), "v2").as_deref(), Some("v2"));
        assert_eq!(
            link_inside(&at("a/b/link"), "..\\c\\d").as_deref(),
            Some("../c/d")
        );
        assert_eq!(link_inside(&at("a/link"), "../x").as_deref(), Some("../x"));
        assert_eq!(
            link_inside(&at("link"), "./x/./y").as_deref(),
            Some("./x/./y")
        );
        assert_eq!(link_inside(&at("link"), "../x"), None);
        // Leaving and coming back in still counts as leaving.
        assert_eq!(link_inside(&at("a/link"), "../../repo/a/x"), None);
        for absolute in [
            "C:\\Users\\dev\\repo\\x",
            "c:x",
            "\\\\?\\C:\\x",
            "\\\\server\\share",
            "\\rooted",
            "/mnt/c/x",
            "",
        ] {
            assert_eq!(link_inside(&at("a/link"), absolute), None, "{absolute}");
        }
    }
}
