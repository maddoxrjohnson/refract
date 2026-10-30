//! Git for the migration: the git snapshot of a repo (the commands
//! [`refract_core::migrate::gitstate`] documents), blob ids of working-tree files, and
//! normalising a copied repo into a Linux checkout (spec §3.3).
//!
//! Every command runs with `--no-optional-locks` and with `GIT_CEILING_DIRECTORIES` set
//! to the repo's parent, like [`crate::git`], so git never finds a repo around it.
//! `config` arguments are `key=value` pairs passed as `-c`, which beat every config
//! file.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use refract_core::migrate::gitconfig::{config_changes, parse_config, ConfigEntry};
use refract_core::migrate::gitstate::{
    parse_fsck, parse_head, parse_index_flags, parse_lines, parse_nul_records, parse_refs,
    parse_status, parse_submodules, GitSnapshot, FOR_EACH_REF_FORMAT,
};

use super::fsx;

/// Runs git in `dir` with `-c` `config`, feeding `input` on stdin when given.
pub fn run(
    dir: &Path,
    config: &[String],
    args: &[&str],
    input: Option<&[u8]>,
) -> io::Result<Output> {
    let mut command = command(dir, config);
    command.args(args);
    command.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdin = child.stdin.take();
    std::thread::scope(|scope| {
        if let (Some(mut stdin), Some(input)) = (stdin, input) {
            // Fed from a thread, so a large input never deadlocks against the output.
            scope.spawn(move || {
                let _ = stdin.write_all(input);
            });
        }
        child.wait_with_output()
    })
}

fn command(dir: &Path, config: &[String]) -> Command {
    let mut command = Command::new("git");
    // As the app runs git.exe: paths unquoted where `-z` is not available (`submodule
    // status`), and English messages, which `parse_fsck` filters.
    command
        .arg("--no-optional-locks")
        .args(["-c", "core.quotepath=false"]);
    for entry in config {
        command.arg("-c").arg(entry);
    }
    command
        .arg("-C")
        .arg(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .env("LANG", "C");
    if let Some(parent) = dir.parent() {
        command.env("GIT_CEILING_DIRECTORIES", parent);
    }
    command
}

/// Stdout of a command that must succeed, as text.
fn text(dir: &Path, config: &[String], args: &[&str]) -> Result<String, String> {
    let out = run(dir, config, args, None).map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    utf8(out.stdout, args)
}

fn utf8(bytes: Vec<u8>, args: &[&str]) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|_| {
        format!(
            "git {} wrote a path that is not UTF-8",
            args.first().unwrap_or(&"")
        )
    })
}

/// The git snapshot of the repo whose working tree is `dir`, with the commands and
/// parsers [`refract_core::migrate::gitstate`] lists.
pub fn snapshot(dir: &Path, config: &[String]) -> Result<GitSnapshot, String> {
    // An unborn HEAD fails, printing `HEAD`; parse_head reads that as None.
    let head = run(dir, config, &["rev-parse", "HEAD"], None)
        .map_err(|e| format!("cannot run git: {e}"))?;
    let format = format!("--format={FOR_EACH_REF_FORMAT}");
    let refs = text(dir, config, &["for-each-ref", &format])?;
    let stash = text(dir, config, &["stash", "list", "--format=%H"])?;
    // Exits 1 when the repo has no local config at all.
    let local = run(dir, config, &["config", "--local", "-l"], None)
        .map_err(|e| format!("cannot run git: {e}"))?;
    let status = text(
        dir,
        config,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    )?;
    let index = text(dir, config, &["ls-files", "-s", "-z"])?;
    let index_flags = text(dir, config, &["ls-files", "-v", "-z"])?;
    let submodules = text(dir, config, &["submodule", "status", "--recursive"])?;
    let fsck =
        run(dir, config, &["fsck", "--full"], None).map_err(|e| format!("cannot run git: {e}"))?;
    let mut fsck_text = fsck.stdout;
    fsck_text.extend_from_slice(&fsck.stderr);
    Ok(GitSnapshot {
        head: parse_head(&String::from_utf8_lossy(&head.stdout)),
        refs: parse_refs(&refs)?,
        stash: parse_lines(&stash),
        config: parse_config(&utf8(local.stdout, &["config"])?),
        status: parse_status(&status)?,
        index: parse_nul_records(&index),
        index_flags: parse_index_flags(&index_flags),
        submodules: parse_submodules(&submodules)?,
        fsck: parse_fsck(&String::from_utf8_lossy(&fsck_text)),
    })
}

/// The working-tree paths of submodules that are checked out, nested ones included,
/// relative to `dir`, from `git submodule status --recursive`.
pub fn submodule_paths(dir: &Path, config: &[String]) -> Result<Vec<String>, String> {
    let out = text(dir, config, &["submodule", "status", "--recursive"])?;
    Ok(parse_submodules(&out)?
        .into_iter()
        .filter(|s| s.state != "-")
        .map(|s| s.path)
        .collect())
}

/// One stage-0 index entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub mode: u32,
    pub object: String,
    pub path: String,
}

/// The index's entries at stage 0 (merged), keyed by path.
pub fn index(dir: &Path) -> Result<BTreeMap<String, IndexEntry>, String> {
    let out = text(dir, &[], &["ls-files", "-s", "-z"])?;
    let mut entries = BTreeMap::new();
    for record in parse_nul_records(&out) {
        let Some((meta, path)) = record.split_once('\t') else {
            continue;
        };
        let parts: Vec<&str> = meta.split(' ').collect();
        let [mode, object, stage] = parts.as_slice() else {
            continue;
        };
        let Ok(mode) = u32::from_str_radix(mode, 8) else {
            continue;
        };
        if *stage != "0" {
            continue;
        }
        entries.insert(
            path.to_owned(),
            IndexEntry {
                mode,
                object: (*object).to_owned(),
                path: path.to_owned(),
            },
        );
    }
    Ok(entries)
}

/// The paths of `dir`'s index entries marked skip-worktree, and of those marked
/// assume-unchanged (`git ls-files -v`).
fn flagged(dir: &Path) -> Result<(BTreeSet<String>, BTreeSet<String>), String> {
    let out = text(dir, &[], &["ls-files", "-v", "-z"])?;
    let (mut skip, mut assume) = (BTreeSet::new(), BTreeSet::new());
    for record in parse_index_flags(&out) {
        let Some((tag, path)) = record.split_once(' ') else {
            continue;
        };
        if tag.eq_ignore_ascii_case("S") {
            skip.insert(path.to_owned());
        }
        if tag.chars().all(|c| c.is_ascii_lowercase()) {
            assume.insert(path.to_owned());
        }
    }
    Ok((skip, assume))
}

/// The paths of `dir`'s intent-to-add entries (`git add -N`), which `diff-files` shows
/// as added.
fn intent_to_add(dir: &Path) -> Result<BTreeSet<String>, String> {
    let out = text(
        dir,
        &[],
        &["diff-files", "--name-only", "-z", "--diff-filter=A"],
    )?;
    Ok(parse_nul_records(&out).into_iter().collect())
}

/// Sets `flag` (an `update-index` option such as `--skip-worktree`) on `paths`.
fn set_flag(dir: &Path, flag: &str, paths: &[&String]) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    let out = run(
        dir,
        &[],
        &["update-index", "-z", flag, "--stdin"],
        Some(&input),
    )
    .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git update-index {flag} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// The paths git tracks in `dir`, any stage.
pub fn tracked(dir: &Path, config: &[String]) -> Result<BTreeSet<String>, String> {
    let out = text(dir, config, &["ls-files", "-z"])?;
    Ok(parse_nul_records(&out).into_iter().collect())
}

/// The blob ids git would store for working-tree paths of the repo at `dir`
/// (`/`-separated, relative to it), computed with `config`. Regular files go through
/// `git hash-object --stdin-paths`, which applies the path's filters (line endings,
/// LFS); a symlink's blob is its target. With `write` the blobs are written to the
/// object store. A path that cannot be hashed is left out of the result.
pub fn blobs(
    dir: &Path,
    config: &[String],
    paths: &[String],
    write: bool,
) -> Result<BTreeMap<String, String>, String> {
    let mut result = BTreeMap::new();
    let mut files = Vec::new();
    for path in paths {
        let abs = dir.join(path);
        match fs::symlink_metadata(&abs) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let Ok(target) = fs::read_link(&abs) else {
                    continue;
                };
                if let Some(id) =
                    hash_bytes(dir, config, target.as_os_str().as_bytes(), None, write)?
                {
                    result.insert(path.clone(), id);
                }
            }
            Ok(meta) if meta.is_file() => files.push(path.clone()),
            _ => {}
        }
    }
    // --stdin-paths reads one path per line, so paths holding a newline go one by one.
    let (plain, odd): (Vec<String>, Vec<String>) =
        files.into_iter().partition(|p| !p.contains('\n'));
    for path in odd {
        if let Ok(bytes) = fs::read(dir.join(&path)) {
            if let Some(id) = hash_bytes(dir, config, &bytes, Some(&path), write)? {
                result.insert(path, id);
            }
        }
    }
    // Batched so that one unreadable file costs only its own batch a retry.
    for chunk in plain.chunks(2000) {
        let mut input = Vec::new();
        for path in chunk {
            input.extend_from_slice(path.as_bytes());
            input.push(b'\n');
        }
        let mut args = vec!["hash-object", "--stdin-paths"];
        if write {
            args.insert(1, "-w");
        }
        let out =
            run(dir, config, &args, Some(&input)).map_err(|e| format!("cannot run git: {e}"))?;
        let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect();
        if out.status.success() && ids.len() == chunk.len() {
            result.extend(chunk.iter().cloned().zip(ids));
            continue;
        }
        // Some file failed (unreadable, vanished): hash the batch one by one.
        for path in chunk {
            let Ok(bytes) = fs::read(dir.join(path)) else {
                continue;
            };
            if let Some(id) = hash_bytes(dir, config, &bytes, Some(path), write)? {
                result.insert(path.clone(), id);
            }
        }
    }
    Ok(result)
}

/// The blob id of `bytes`: filtered as `path` when given, else as they are.
fn hash_bytes(
    dir: &Path,
    config: &[String],
    bytes: &[u8],
    path: Option<&str>,
    write: bool,
) -> Result<Option<String>, String> {
    let path_arg;
    let mut args = vec!["hash-object"];
    if write {
        args.push("-w");
    }
    match path {
        Some(path) => {
            path_arg = format!("--path={path}");
            args.push(&path_arg);
        }
        None => args.push("--no-filters"),
    }
    args.push("--stdin");
    let out = run(dir, config, &args, Some(bytes)).map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned()))
}

/// The `-c` settings under which Linux git computes the blob Windows git would store
/// for a working-tree file: `core.autocrlf` as `recorded` says (the value the app's
/// git saw, from [`refract_core::migrate::RepoGit::autocrlf`], global and system
/// config included), else as the repo's local config sets it, else `true` (Git for
/// Windows' default, from its system config); and `core.safecrlf=false` so a file with
/// mixed line endings is hashed rather than refused.
pub fn windows_config(local: &[ConfigEntry], recorded: Option<&str>) -> Vec<String> {
    let autocrlf = match recorded {
        Some(value) => value.to_owned(),
        None => local
            .iter()
            .rev()
            .find(|e| e.key.eq_ignore_ascii_case("core.autocrlf"))
            .map(|e| e.value.clone().unwrap_or_else(|| "true".into()))
            .unwrap_or_else(|| "true".into()),
    };
    vec![
        format!("core.autocrlf={autocrlf}"),
        "core.filemode=false".into(),
        "core.safecrlf=false".into(),
    ]
}

/// What [`normalise`] did besides rewriting files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Normalised {
    /// Per-path failures.
    pub problems: Vec<(String, String)>,
    /// Tracked files left byte for byte as copied, because Windows git checking out
    /// their clean blob would not give back their bytes (a clean filter that drops
    /// part of them, mixed line endings): rewriting them would lose what the blob
    /// does not hold. Check 1 then fails on them unless Linux git stores the same blob.
    pub kept: Vec<String>,
}

/// Normalises the copied repo whose working tree is `dir` (spec §3.3): for each tracked
/// file (only those in `only`, when given; paths relative to `dir`) the blob Windows
/// git would store is written to the copy's object store with `windows`, the
/// Windows-only local config keys are replaced ([`config_changes`]), and each file
/// whose bytes are exactly what Windows git checks that blob out as (`git cat-file
/// --batch --filters` with `windows`) is rewritten as Linux git checks it out (the
/// same without them), with its mode from the index (a symlink entry becomes a
/// symlink). A file already as Linux git checks it out only gets its mode; any other
/// file is kept byte for byte ([`Normalised::kept`]), since its bytes cannot be rebuilt
/// from the blob. A file whose bytes do not change is left alone; a rewritten one keeps
/// its modification time. Then the index is refreshed.
///
/// The copied index still holds each file's Windows size, and git takes a file whose
/// size differs from the recorded one for modified without reading it. So the entries
/// of rewritten files (of every stage-0 file, with `restat_all`, for an index copied
/// again from the source) are first written again with the same mode and object
/// (`update-index --index-info`), which clears their recorded stat, and the refresh
/// then compares their content. Writing an entry again drops its skip-worktree and
/// assume-unchanged flags, so they are read first and set again after the refresh;
/// intent-to-add entries are not written again at all, since that would stage them as
/// empty files (their recorded stat is empty anyway).
/// `progress` is called with each file done.
pub fn normalise(
    dir: &Path,
    windows: &[String],
    only: Option<&BTreeSet<String>>,
    restat_all: bool,
    progress: &mut dyn FnMut(),
) -> Result<Normalised, String> {
    let index = index(dir)?;
    let mut result = Normalised::default();
    let candidates: Vec<&IndexEntry> = index
        .values()
        .filter(|e| matches!(e.mode, 0o100644 | 0o100755 | 0o120000))
        .filter(|e| only.is_none_or(|only| only.contains(&e.path)))
        .collect();
    // Only regular files are rewritten: a symlink in the copy is already what Linux git
    // checks out.
    let files: Vec<String> = candidates
        .iter()
        .filter(|e| fs::symlink_metadata(dir.join(&e.path)).is_ok_and(|m| m.is_file()))
        .map(|e| e.path.clone())
        .collect();
    let clean = blobs(dir, windows, &files, true)?;

    let local = run(dir, &[], &["config", "--local", "-l"], None)
        .map_err(|e| format!("cannot run git: {e}"))?;
    let changes = config_changes(&parse_config(&String::from_utf8_lossy(&local.stdout)));
    for key in &changes.unset {
        // Exits 5 when the key is not there.
        let _ = run(dir, &[], &["config", "--local", "--unset-all", key], None);
    }
    for (key, value) in &changes.set {
        text(
            dir,
            &[],
            &["config", "--local", "--replace-all", key, value],
        )?;
    }

    let mut want: Vec<(&String, &String)> = clean.iter().collect();
    want.sort();
    for path in files.iter().filter(|p| !clean.contains_key(*p)) {
        result.problems.push((
            path.clone(),
            "Git could not read it to normalise it.".into(),
        ));
    }
    let rewritten = smudge(dir, windows, &want, &index, &mut result, progress)?;
    let (skip, assume) = flagged(dir)?;
    let restat: Vec<String> = if restat_all {
        index
            .values()
            .filter(|e| matches!(e.mode, 0o100644 | 0o100755 | 0o120000))
            .map(|e| e.path.clone())
            .collect()
    } else {
        rewritten
    };
    let intent = if restat.is_empty() {
        BTreeSet::new()
    } else {
        intent_to_add(dir)?
    };
    let restat: Vec<String> = restat.into_iter().filter(|p| !intent.contains(p)).collect();
    if !restat.is_empty() {
        let mut input = Vec::new();
        for entry in restat.iter().filter_map(|p| index.get(p)) {
            input.extend_from_slice(
                format!("{:o} {}\t{}\0", entry.mode, entry.object, entry.path).as_bytes(),
            );
        }
        let out = run(
            dir,
            &[],
            &["update-index", "-z", "--index-info"],
            Some(&input),
        )
        .map_err(|e| format!("cannot run git: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git update-index --index-info failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    // Exits 1 when some file differs from the index, as unstaged edits do.
    let _ = run(dir, &[], &["update-index", "-q", "--refresh"], None);
    let lost = |flags: &BTreeSet<String>| -> Vec<&String> {
        restat.iter().filter(|p| flags.contains(*p)).collect()
    };
    set_flag(dir, "--skip-worktree", &lost(&skip))?;
    set_flag(dir, "--assume-unchanged", &lost(&assume))?;
    Ok(result)
}

/// A `git cat-file --batch --filters` child reading the copy's objects.
struct CatFile {
    child: std::process::Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl CatFile {
    fn spawn(dir: &Path, config: &[String]) -> Result<(CatFile, std::process::ChildStdin), String> {
        let mut child = command(dir, config)
            .args(["cat-file", "--batch", "--filters"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot run git: {e}"))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let reader = BufReader::new(child.stdout.take().expect("piped stdout"));
        Ok((CatFile { child, reader }, stdin))
    }

    /// The next object's content; `None` when git could not check it out.
    fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
        let mut header = String::new();
        if self
            .reader
            .read_line(&mut header)
            .map_err(|e| e.to_string())?
            == 0
        {
            return Err("git cat-file stopped early".to_string());
        }
        let parts: Vec<&str> = header.trim_end().split(' ').collect();
        let size = match parts.as_slice() {
            [_, "blob", size] => size.parse::<usize>().map_err(|e| e.to_string())?,
            _ => return Ok(None),
        };
        let mut content = vec![0; size];
        self.reader
            .read_exact(&mut content)
            .map_err(|e| e.to_string())?;
        let mut newline = [0u8; 1];
        self.reader
            .read_exact(&mut newline)
            .map_err(|e| e.to_string())?;
        Ok(Some(content))
    }
}

/// How many files [`windows_checkout`] checks out at once.
const CHECKOUT_CHUNK: usize = 500;

/// What Windows git (`windows`) checks each (path, blob) of `want` out as, by path, in
/// the repo whose working tree is `dir`: `git checkout-index --temp` from a scratch
/// index holding just those entries, so filters, line endings and attributes apply as
/// git applies them (`cat-file --batch --filters` cannot be used: its header gives the
/// unfiltered size). A path git could not check out is left out.
fn windows_checkout(
    dir: &Path,
    windows: &[String],
    want: &[&(&String, &String)],
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let git_dir = text(dir, &[], &["rev-parse", "--absolute-git-dir"])?;
    let scratch = Path::new(git_dir.trim()).join("refract-normalise-index");
    let _ = fs::remove_file(&scratch);
    let mut input = Vec::new();
    let mut paths = Vec::new();
    for (path, blob) in want {
        input.extend_from_slice(format!("100644 {blob}\t{path}\0").as_bytes());
        paths.extend_from_slice(path.as_bytes());
        paths.push(0);
    }
    let with_index = |config: &[String], args: &[&str], stdin: &[u8]| -> Result<Output, String> {
        let mut command = command(dir, config);
        command
            .args(args)
            .env("GIT_INDEX_FILE", &scratch)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| format!("cannot run git: {e}"))?;
        let mut pipe = child.stdin.take().expect("piped stdin");
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let _ = pipe.write_all(stdin);
            });
            child.wait_with_output()
        })
        .map_err(|e| format!("cannot run git: {e}"))
    };
    let result = (|| {
        let made = with_index(&[], &["update-index", "-z", "--index-info"], &input)?;
        if !made.status.success() {
            return Err(format!(
                "git update-index --index-info failed: {}",
                String::from_utf8_lossy(&made.stderr).trim()
            ));
        }
        let out = with_index(
            windows,
            &["checkout-index", "--temp", "-z", "--stdin"],
            &paths,
        )?;
        let mut found = BTreeMap::new();
        for record in out.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let Some(tab) = record.iter().position(|b| *b == b'\t') else {
                continue;
            };
            let temp = dir.join(std::ffi::OsStr::from_bytes(&record[..tab]));
            let path = String::from_utf8_lossy(&record[tab + 1..]).into_owned();
            let bytes = fs::read(&temp);
            let _ = fs::remove_file(&temp);
            if let Ok(bytes) = bytes {
                found.insert(path, bytes);
            }
        }
        Ok(found)
    })();
    let _ = fs::remove_file(&scratch);
    result
}

/// Rewrites each (path, blob) as Linux git checks the blob out, when the file holds
/// what Windows git (`windows`) checks it out as (see [`normalise`]). Returns the paths
/// whose bytes or kind changed; kept files go to `result.kept`.
fn smudge(
    dir: &Path,
    windows: &[String],
    want: &[(&String, &String)],
    index: &BTreeMap<String, IndexEntry>,
    result: &mut Normalised,
    progress: &mut dyn FnMut(),
) -> Result<Vec<String>, String> {
    let usable: Vec<&(&String, &String)> = want.iter().filter(|(p, _)| !p.contains('\n')).collect();
    for (path, _) in want.iter().filter(|(p, _)| p.contains('\n')) {
        result
            .problems
            .push(((*path).clone(), "Its name holds a line break.".into()));
    }
    let mut rewritten = Vec::new();
    if usable.is_empty() {
        return Ok(rewritten);
    }
    let (mut linux, linux_in) = CatFile::spawn(dir, &[])?;
    let input: Vec<u8> = usable
        .iter()
        .flat_map(|(path, blob)| format!("{blob} {path}\n").into_bytes())
        .collect();
    let outcome = std::thread::scope(|scope| {
        let mut stdin = linux_in;
        let input = &input;
        scope.spawn(move || {
            let _ = stdin.write_all(input);
        });
        for chunk in usable.chunks(CHECKOUT_CHUNK) {
            let on_windows = windows_checkout(dir, windows, chunk)?;
            for (path, _) in chunk {
                let Some(on_linux) = linux.next()? else {
                    result
                        .problems
                        .push(((*path).clone(), "Git could not check it out.".into()));
                    progress();
                    continue;
                };
                let mode = index.get(*path).map_or(0o100644, |e| e.mode);
                let windows = on_windows.get(*path).map(Vec::as_slice);
                match rewrite(&dir.join(path), &on_linux, windows, mode) {
                    Ok(Rewrite::Changed) => rewritten.push((*path).clone()),
                    Ok(Rewrite::Same) => {}
                    Ok(Rewrite::Kept) => result.kept.push((*path).clone()),
                    Err(e) => result
                        .problems
                        .push(((*path).clone(), format!("Could not rewrite it: {e}"))),
                }
                progress();
            }
        }
        Ok(())
    });
    if outcome.is_err() {
        let _ = linux.child.kill();
    }
    let _ = linux.child.wait();
    outcome.map(|()| rewritten)
}

/// What [`rewrite`] did.
enum Rewrite {
    /// The bytes or the kind changed.
    Changed,
    /// Already as Linux git checks it out (its mode may have changed).
    Same,
    /// Left as it was: it is not what Windows git checks the blob out as.
    Kept,
}

/// Writes `linux` to `path` as a file of git `mode` (or a symlink to it, for `120000`),
/// keeping the old modification time, when the file holds `linux` or `windows` (the
/// blob as Linux and as Windows git check it out; `None` when Windows git could not);
/// otherwise leaves its bytes alone.
fn rewrite(path: &Path, linux: &[u8], windows: Option<&[u8]>, mode: u32) -> io::Result<Rewrite> {
    let meta = fs::symlink_metadata(path)?;
    let mtime = fsx::mtime_ns(&meta);
    let current = fs::read(path)?;
    let perms = if mode == 0o100755 { 0o755 } else { 0o644 };
    let outcome = if current == linux {
        Rewrite::Same
    } else if windows == Some(current.as_slice()) {
        Rewrite::Changed
    } else {
        Rewrite::Kept
    };
    if mode == 0o120000 {
        if matches!(outcome, Rewrite::Kept) {
            return Ok(Rewrite::Kept);
        }
        let target = std::ffi::OsStr::from_bytes(linux);
        fs::remove_file(path)?;
        std::os::unix::fs::symlink(target, path)?;
        fsx::set_mtime(path, mtime)?;
        return Ok(Rewrite::Changed);
    }
    if matches!(outcome, Rewrite::Changed) {
        // Truncated in place, so the folder's modification time stays.
        fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)?
            .write_all(linux)?;
    }
    if meta.permissions().mode() & 0o777 != perms {
        fs::set_permissions(path, fs::Permissions::from_mode(perms))?;
    }
    fsx::set_mtime(path, mtime)?;
    Ok(outcome)
}

/// The git directory of the repo whose working tree is `repo` (relative to `root`,
/// `""` for the top), resolved: its `.git` folder, or where its `.git` file points.
/// Fails when it lies outside `root`, since git would then read and write a folder
/// that is not the copy's (a submodule pointing back at the original, say).
pub fn git_dir(root: &Path, repo: &str) -> Result<std::path::PathBuf, String> {
    let work = if repo.is_empty() {
        root.to_owned()
    } else {
        root.join(repo)
    };
    let dot_git = work.join(".git");
    let shown = if repo.is_empty() { "." } else { repo };
    let meta = fs::symlink_metadata(&dot_git).map_err(|e| format!("{shown} has no .git: {e}"))?;
    let target = if meta.is_dir() {
        dot_git
    } else if meta.is_file() {
        let text = fs::read_to_string(&dot_git)
            .map_err(|e| format!("cannot read {}: {e}", dot_git.display()))?;
        let pointed = text
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("gitdir:"))
            .map(str::trim)
            .ok_or_else(|| format!("{} is not a gitdir file", dot_git.display()))?;
        work.join(pointed)
    } else {
        return Err(format!("{shown}/.git is a link; refusing to follow it"));
    };
    let canonical_root =
        fs::canonicalize(root).map_err(|e| format!("cannot resolve {}: {e}", root.display()))?;
    let canonical = fs::canonicalize(&target)
        .map_err(|e| format!("cannot resolve the git folder of {shown}: {e}"))?;
    if canonical != canonical_root && canonical.starts_with(&canonical_root) {
        Ok(canonical)
    } else {
        Err(format!(
            "The git folder of {shown} is {}, outside the repo; Refract won't touch it.",
            canonical.display()
        ))
    }
}

/// The path of the index of the repo at `repo` relative to `root`, when its git
/// directory lies inside `root` (see [`git_dir`]).
pub fn index_path(root: &Path, repo: &str) -> Option<String> {
    let dir = git_dir(root, repo).ok()?;
    let canonical_root = fs::canonicalize(root).ok()?;
    let rel = dir.join("index");
    let rel = rel.strip_prefix(&canonical_root).ok()?;
    rel.to_str().map(str::to_owned)
}

/// A SHA-256 of the index entries of the repo at `dir` (`ls-files -s` and `ls-files
/// -v`), not of the file: a refresh that only rewrites the recorded stat keeps it.
pub fn index_digest(dir: &Path) -> Result<String, String> {
    let mut all = text(dir, &[], &["ls-files", "-s", "-z"])?.into_bytes();
    all.push(0);
    all.extend_from_slice(text(dir, &[], &["ls-files", "-v", "-z"])?.as_bytes());
    Ok(fsx::sha256_bytes(&all))
}

/// True when git can run `git lfs`.
pub fn has_git_lfs() -> bool {
    Command::new("git")
        .args(["lfs", "version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `git lfs fsck` in `dir`: the problem, if it fails.
pub fn lfs_fsck(dir: &Path) -> Option<String> {
    match run(dir, &[], &["lfs", "fsck"], None) {
        Ok(out) if out.status.success() => None,
        Ok(out) => Some(format!(
            "git lfs fsck failed: {}",
            String::from_utf8_lossy(&out.stdout).trim()
        )),
        Err(e) => Some(format!("cannot run git lfs fsck: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_config_follows_the_local_autocrlf() {
        let entry = |key: &str, value: Option<&str>| ConfigEntry {
            key: key.into(),
            value: value.map(str::to_owned),
        };
        assert_eq!(windows_config(&[], None)[0], "core.autocrlf=true");
        assert_eq!(
            windows_config(&[entry("core.autocrlf", Some("input"))], None)[0],
            "core.autocrlf=input"
        );
        assert_eq!(
            windows_config(&[entry("core.autocrlf", Some("false"))], None)[0],
            "core.autocrlf=false"
        );
        // What the app's git saw, global config included, wins.
        assert_eq!(
            windows_config(&[entry("core.autocrlf", Some("true"))], Some("false"))[0],
            "core.autocrlf=false"
        );
    }
}
