//! The Windows side of the preflight (spec §4.1) and the source snapshot (spec §4.2),
//! built from [`SourceFs`] and [`GitRun`] so the whole of it runs against fakes in
//! tests.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;

use refract_core::migrate::build::{install_plan, InstallStep, RootFiles};
use refract_core::migrate::manifest::{EntryKind, Manifest};
use refract_core::migrate::preflight::{
    refusals, windows_tool_warnings, GitOperation, PreflightFacts, Refusal,
};
use refract_core::migrate::{RepoGit, SourceSnapshot, SOURCE_SNAPSHOT_VERSION};
use refract_core::sessions::is_windows_mount;
use serde::Serialize;

use super::git::{
    autocrlf, clean_blobs, git_snapshot, ignored_dirs, ignored_dirs_quick, link_blob,
    submodule_repos, tracked_paths, GitRun,
};
use super::walk::{read_files, walk, Cancelled, ReadMode, ReadProgress, SourceFs, Walk};

/// What the snapshot is doing, sent to the interface with its counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SnapshotPhase {
    /// Reading the git state of the repo and its submodules.
    Git,
    /// Listing every folder.
    Walking,
    /// Hashing every file; the counts are files and bytes.
    Hashing,
    /// Asking git for the clean blob of each tracked file.
    Blobs,
    /// Checking that every file can be read (the preflight).
    Probing,
}

/// One progress report: the phase, with file and byte counts while reading files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotProgress {
    pub phase: SnapshotPhase,
    #[serde(flatten)]
    pub read: ReadProgress,
}

/// Why a snapshot or preflight stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    Cancelled,
    /// git failed or printed something unreadable.
    Git(String),
    /// The source changed since the preflight in a way that refuses the move: a link
    /// leaves the repo, or a file or folder can't be read.
    Refused(Vec<Refusal>),
}

impl From<Cancelled> for SnapshotError {
    fn from(_: Cancelled) -> SnapshotError {
        SnapshotError::Cancelled
    }
}

/// Takes S0 and G0 (spec §4.2): G0 of the repo and each checked-out submodule, then
/// the walk with the folders git ignores, every file hashed on `threads` threads, and
/// the clean blob id of each tracked file present in the working tree.
pub fn take_snapshot(
    fs: &dyn SourceFs,
    git: &dyn GitRun,
    threads: usize,
    cancel: &AtomicBool,
    progress: &(dyn Fn(SnapshotProgress) + Sync),
) -> Result<SourceSnapshot, SnapshotError> {
    let phase = |phase| SnapshotProgress {
        phase,
        read: ReadProgress::default(),
    };
    progress(phase(SnapshotPhase::Git));
    let repo_git = |path: String| -> Result<RepoGit, SnapshotError> {
        Ok(RepoGit {
            snapshot: git_snapshot(git, &path).map_err(SnapshotError::Git)?,
            autocrlf: Some(autocrlf(git, &path).map_err(SnapshotError::Git)?),
            path,
        })
    };
    let mut repos = vec![repo_git(String::new())?];
    for path in submodule_repos(&repos[0].snapshot) {
        check(cancel)?;
        repos.push(repo_git(path)?);
    }
    let ignored: BTreeSet<String> = repos
        .iter()
        .flat_map(|repo| ignored_dirs(&repo.path, &repo.snapshot.status))
        .collect();

    progress(phase(SnapshotPhase::Walking));
    let Walk {
        mut manifest,
        external_links,
        unreadable,
    } = walk(fs, &ignored, cancel)?;
    refuse_links_or_unreadable(&external_links, &unreadable)?;

    let unreadable = read_files(
        fs,
        &mut manifest,
        ReadMode::Hash,
        threads,
        cancel,
        &|read| {
            progress(SnapshotProgress {
                phase: SnapshotPhase::Hashing,
                read,
            })
        },
    )?;
    refuse_links_or_unreadable(&[], &unreadable)?;

    progress(phase(SnapshotPhase::Blobs));
    for repo in &repos {
        check(cancel)?;
        set_blobs(
            git,
            &repo.path,
            &tracked_paths(&repo.snapshot.index),
            &mut manifest,
        )?;
    }
    Ok(SourceSnapshot {
        version: SOURCE_SNAPSHOT_VERSION,
        manifest,
        git: repos,
        linux_git: git.linux(),
    })
}

fn check(cancel: &AtomicBool) -> Result<(), SnapshotError> {
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        Err(SnapshotError::Cancelled)
    } else {
        Ok(())
    }
}

fn refuse_links_or_unreadable(
    external_links: &[String],
    unreadable: &[String],
) -> Result<(), SnapshotError> {
    let mut refused = Vec::new();
    if !external_links.is_empty() {
        refused.push(Refusal::ExternalLinks {
            paths: external_links.to_vec(),
        });
    }
    if !unreadable.is_empty() {
        refused.push(Refusal::Unreadable {
            paths: unreadable.to_vec(),
        });
    }
    if refused.is_empty() {
        Ok(())
    } else {
        Err(SnapshotError::Refused(refused))
    }
}

/// Sets the clean blob id on the manifest entry of each tracked path of the repo at
/// `repo` that is a file or symlink in the working tree: a file's from its content
/// through the repo's filters, a symlink's from its target. Tracked paths deleted from
/// the working tree have no entry and get none.
fn set_blobs(
    git: &dyn GitRun,
    repo: &str,
    tracked: &[String],
    manifest: &mut Manifest,
) -> Result<(), SnapshotError> {
    let index: BTreeMap<String, usize> = manifest
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e.kind, EntryKind::File | EntryKind::Symlink))
        .map(|(i, e)| (e.path.to_string(), i))
        .collect();
    let (mut present, mut slots) = (Vec::new(), Vec::new());
    for path in tracked {
        let full = if repo.is_empty() {
            path.clone()
        } else {
            format!("{repo}/{path}")
        };
        let Some(&slot) = index.get(&full) else {
            continue;
        };
        match &manifest.entries[slot].link_target {
            Some(target) if manifest.entries[slot].kind == EntryKind::Symlink => {
                let blob = link_blob(git, repo, target).map_err(SnapshotError::Git)?;
                manifest.entries[slot].blob = Some(blob);
            }
            _ => {
                present.push(path.clone());
                slots.push(slot);
            }
        }
    }
    let blobs = clean_blobs(git, repo, &present).map_err(SnapshotError::Git)?;
    for (slot, blob) in slots.into_iter().zip(blobs) {
        manifest.entries[slot].blob = Some(blob);
    }
    Ok(())
}

/// What the source's `.git` says about the repo, for the preflight.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepoShape {
    pub is_git_repo: bool,
    pub is_linked_worktree: bool,
    pub has_linked_worktrees: bool,
    pub operation: Option<GitOperation>,
    pub uses_lfs: bool,
}

/// Reads the shape from the source's `.git`: a `.git` file is a linked worktree (or a
/// repo whose git directory lives elsewhere, which is refused the same way), entries
/// in `.git/worktrees` are linked worktrees, the marker files of an operation in
/// progress, and LFS from `.git/lfs` or `filter=lfs` in `.gitattributes`.
pub fn repo_shape(fs: &dyn SourceFs) -> RepoShape {
    let Ok(root) = fs.list("") else {
        return RepoShape::default();
    };
    let named = |entries: &[super::walk::RawEntry], name: &str| {
        entries
            .iter()
            .find(|e| String::from_utf16_lossy(&e.name) == name)
            .cloned()
    };
    let Some(dot_git) = named(&root, ".git") else {
        return RepoShape::default();
    };
    if !dot_git.is_dir() {
        return RepoShape {
            is_git_repo: true,
            is_linked_worktree: true,
            ..RepoShape::default()
        };
    }
    let git = fs.list(".git").unwrap_or_default();
    let has = |name: &str| named(&git, name).is_some();
    let operation = if has("rebase-merge") || has("rebase-apply") {
        Some(GitOperation::Rebase)
    } else if has("MERGE_HEAD") {
        Some(GitOperation::Merge)
    } else if has("CHERRY_PICK_HEAD") {
        Some(GitOperation::CherryPick)
    } else if has("REVERT_HEAD") {
        Some(GitOperation::Revert)
    } else if has("BISECT_LOG") {
        Some(GitOperation::Bisect)
    } else {
        None
    };
    let attributes = fs.read_text(".gitattributes", 1 << 20).unwrap_or_default();
    RepoShape {
        is_git_repo: true,
        is_linked_worktree: false,
        has_linked_worktrees: fs
            .list(".git/worktrees")
            .is_ok_and(|entries| !entries.is_empty()),
        operation,
        uses_lfs: has("lfs") || attributes.contains("filter=lfs"),
    }
}

/// The Linux side's facts, from one `wsl.exe` script ([`LINUX_FACTS_SCRIPT`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LinuxFacts {
    pub home: Option<String>,
    pub has_git_lfs: bool,
    /// Something exists at the destination (only asked when one was given).
    pub dest_exists: bool,
    /// Free bytes on the filesystem the destination would be on.
    pub free_bytes: u64,
    /// Install tools asked about that a login shell does not find.
    pub missing_tools: BTreeSet<String>,
    /// Transcripts in the source's project folder under `~/.claude/projects`.
    pub sessions: u32,
}

/// Prints `key=value` lines: `home`, `lfs` (1 or 0), `exists` (1 or 0, only with a
/// destination in `$1`), `freekb`, the free KiB of the nearest existing folder above
/// the destination, or of `$HOME` without one; `missing` for each tool in `$3`
/// (space-separated) that `command -v` does not find in a login shell, as the
/// helper's install looks for it; and `sessions`, the `*.jsonl` transcripts in the
/// project folder named `$2` (the source's encoded path, matched ignoring ASCII case,
/// as for any `/mnt` path).
pub const LINUX_FACTS_SCRIPT: &str = r#"d="$1"; printf 'home=%s\n' "$HOME"
if command -v git-lfs >/dev/null 2>&1; then echo lfs=1; else echo lfs=0; fi
for t in $3; do bash -lc "command -v $t" >/dev/null 2>&1 </dev/null || echo "missing=$t"; done
n=0
if [ -n "$2" ]; then
  want=$(printf '%s' "$2" | tr 'A-Z' 'a-z')
  for p in "${CLAUDE_CONFIG_DIR:-$HOME/.claude}"/projects/*/; do
    [ "$(basename "$p" | tr 'A-Z' 'a-z')" = "$want" ] || continue
    for f in "$p"*.jsonl; do [ -f "$f" ] && n=$((n+1)); done
  done
fi
echo "sessions=$n"
if [ -n "$d" ]; then
  if [ -e "$d" ] || [ -L "$d" ]; then echo exists=1; else echo exists=0; fi
  p="$d"
else
  p="$HOME"
fi
while [ ! -d "$p" ] && [ "$p" != / ] && [ -n "$p" ]; do p=$(dirname "$p"); done
printf 'freekb=%s\n' "$(df -Pk "$p" 2>/dev/null | awk 'NR==2 {print $4}')""#;

/// Reads [`LINUX_FACTS_SCRIPT`]'s output. Missing or unreadable lines leave the
/// defaults: no home, no LFS, nothing at the destination, no free space, no tool
/// missing, no sessions.
pub fn parse_linux_facts(stdout: &str) -> LinuxFacts {
    let mut facts = LinuxFacts::default();
    for line in stdout.lines() {
        match line.trim_end().split_once('=') {
            Some(("home", home)) if home.starts_with('/') => facts.home = Some(home.into()),
            Some(("lfs", value)) => facts.has_git_lfs = value == "1",
            Some(("exists", value)) => facts.dest_exists = value == "1",
            Some(("freekb", kb)) => {
                facts.free_bytes = kb.trim().parse::<u64>().unwrap_or(0).saturating_mul(1024)
            }
            Some(("missing", tool)) if !tool.is_empty() => {
                facts.missing_tools.insert(tool.to_owned());
            }
            Some(("sessions", n)) => facts.sessions = n.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    facts
}

/// The Linux scan roots the default destination is chosen from. The app does not
/// know the helper's configured roots yet, so this is `~/projects`.
// TODO: take the helper's roots once it reports them.
pub fn default_roots(home: &str) -> Vec<String> {
    vec![format!("{}/projects", home.trim_end_matches('/'))]
}

/// The tools of `steps` to ask the Linux side about, once each. Only plain names are
/// asked, since the script runs `command -v` on them through a shell.
pub fn install_tools(steps: &[InstallStep]) -> Vec<String> {
    let plain = |t: &str| {
        !t.is_empty()
            && t.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    let tools: BTreeSet<String> = steps
        .iter()
        .map(|s| s.tool.clone())
        .filter(|t| plain(t))
        .collect();
    tools.into_iter().collect()
}

/// Builds the facts [`refusals`] decides on from both sides.
pub fn preflight_facts(
    shape: &RepoShape,
    on_windows_drive: bool,
    walk: &Walk,
    unreadable: &[String],
    linux: &LinuxFacts,
    dest: Option<&str>,
) -> PreflightFacts {
    let mut all_unreadable = walk.unreadable.clone();
    all_unreadable.extend(unreadable.iter().cloned());
    PreflightFacts {
        is_git_repo: shape.is_git_repo,
        on_windows_drive,
        is_linked_worktree: shape.is_linked_worktree,
        has_linked_worktrees: shape.has_linked_worktrees,
        operation: shape.operation,
        external_links: walk.external_links.clone(),
        unreadable: all_unreadable,
        uses_lfs: shape.uses_lfs,
        wsl_has_git_lfs: linux.has_git_lfs,
        dest_on_linux: dest.is_some_and(|d| d.starts_with('/') && !is_windows_mount(d)),
        dest_exists: linux.dest_exists,
        copy_bytes: walk.manifest.totals().bytes,
        free_bytes: linux.free_bytes,
    }
}

/// A refusal with the sentence the dialog shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefusalView {
    #[serde(flatten)]
    pub refusal: Refusal,
    pub message: String,
}

impl From<Refusal> for RefusalView {
    fn from(refusal: Refusal) -> RefusalView {
        RefusalView {
            message: refusal.message(),
            refusal,
        }
    }
}

/// An install step, and whether its tool is missing in WSL (the dialog says how to
/// get it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedStep {
    #[serde(flatten)]
    pub step: InstallStep,
    pub missing: bool,
}

/// What the dialog shows before **Start** (spec §2.2), apart from busy windows and
/// agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreflightSummary {
    pub refusals: Vec<RefusalView>,
    pub warnings: Vec<String>,
    /// Files, folders and bytes that will be copied.
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub skipped: Vec<refract_core::migrate::build::SkippedFolder>,
    pub install: Vec<PlannedStep>,
    /// Alternate data streams that will be dropped.
    pub dropped_streams: usize,
    /// Claude sessions whose history moves with it.
    pub history_sessions: u32,
}

/// The install steps for the walked source: `package_json` and `yarn_lock` are the
/// start of those files when present.
pub fn install_steps(
    walk: &Walk,
    package_json: Option<&str>,
    yarn_lock: Option<&str>,
) -> Vec<InstallStep> {
    let root_names = walk
        .manifest
        .entries
        .iter()
        .filter(|e| e.kind != EntryKind::Dir && e.path.parent().is_none())
        .map(|e| e.path.to_string())
        .collect();
    let root = RootFiles::new(root_names, package_json, yarn_lock);
    install_plan(&walk.manifest.skipped, &root)
}

/// The dialog's summary from the facts, the walk, its [`install_steps`] and the
/// Linux side's facts.
pub fn summarize(
    facts: &PreflightFacts,
    walk: &Walk,
    install: Vec<InstallStep>,
    linux: &LinuxFacts,
) -> PreflightSummary {
    let totals = walk.manifest.totals();
    PreflightSummary {
        refusals: refusals(facts).into_iter().map(RefusalView::from).collect(),
        warnings: windows_tool_warnings(
            walk.manifest
                .entries
                .iter()
                .map(|e| e.path.as_str())
                .chain(walk.manifest.skipped.iter().map(|s| s.path.as_str())),
        ),
        files: totals.files,
        dirs: totals.dirs,
        bytes: totals.bytes,
        skipped: walk.manifest.skipped.clone(),
        install: install
            .into_iter()
            .map(|step| PlannedStep {
                missing: linux.missing_tools.contains(&step.tool),
                step,
            })
            .collect(),
        dropped_streams: walk.manifest.streams.len(),
        history_sessions: linux.sessions,
    }
}

/// The preflight's walk: the folders git ignores (cheaply), the walk, and every file
/// opened and read a little, to find locked ones. Returns the walk and the files that
/// could not be read.
pub fn preflight_walk(
    fs: &dyn SourceFs,
    git: &dyn GitRun,
    threads: usize,
    cancel: &AtomicBool,
    progress: &(dyn Fn(SnapshotProgress) + Sync),
) -> Result<(Walk, Vec<String>), SnapshotError> {
    progress(SnapshotProgress {
        phase: SnapshotPhase::Git,
        read: ReadProgress::default(),
    });
    let ignored = ignored_dirs_quick(git).map_err(SnapshotError::Git)?;
    progress(SnapshotProgress {
        phase: SnapshotPhase::Walking,
        read: ReadProgress::default(),
    });
    let mut walked = walk(fs, &ignored, cancel)?;
    let unreadable = read_files(
        fs,
        &mut walked.manifest,
        ReadMode::Probe,
        threads,
        cancel,
        &|read| {
            progress(SnapshotProgress {
                phase: SnapshotPhase::Probing,
                read,
            })
        },
    )?;
    Ok((walked, unreadable))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::git::tests::{repo_with_submodule, FakeGit};
    use crate::migrate::walk::tests::{FakeFs, FakeNode};
    use std::sync::Mutex;

    fn file(text: &str) -> FakeNode {
        FakeNode::File(text.as_bytes().to_vec())
    }

    fn source() -> FakeFs {
        FakeFs::with(&[
            (".git/HEAD", file("ref: refs/heads/main\n")),
            ("README.md", file("# Notes\r\n")),
            ("notes.txt", file("draft")),
            (".env", file("TOKEN=x")),
            ("node_modules/x/index.js", file("x")),
            ("web/dist/app.js", file("app")),
            ("lib/sub/.git", file("gitdir: ../../.git/modules/lib/sub\n")),
            ("lib/sub/src.rs", file("fn x() {}")),
            ("lib/sub/target/debug/x", file("bin")),
            ("lib/sub/target/CACHEDIR.TAG", file("Signature")),
            ("package.json", file(r#"{"packageManager": "pnpm@9.1.0"}"#)),
            ("App.sln", file("")),
            ("latest", FakeNode::Link("README.md".into())),
        ])
    }

    /// The submodule repo of `repo_with_submodule`, with its own tracked file.
    fn git() -> FakeGit {
        let mut git = repo_with_submodule();
        git.answer(
            "",
            "ls-files -s -z",
            "100644 1111111111111111111111111111111111111111 0\tREADME.md\0\
             160000 1111111111111111111111111111111111111111 0\tlib/sub\0\
             120000 1111111111111111111111111111111111111111 0\tlatest\0",
        );
        git.answer("", "config core.autocrlf", "true\n");
        git.answer(
            "",
            "-c core.safecrlf=false hash-object --stdin-paths",
            "aaaa\n",
        );
        git.answer("", "hash-object --no-filters --stdin", "cccc\n");
        git.answer(
            "lib/sub",
            "ls-files -s -z",
            "100644 1111111111111111111111111111111111111111 0\tsrc.rs\0",
        );
        git.answer(
            "lib/sub",
            "-c core.safecrlf=false hash-object --stdin-paths",
            "bbbb\n",
        );
        git
    }

    #[test]
    fn snapshot_has_hashes_blobs_and_every_repo() {
        let phases = Mutex::new(Vec::new());
        let snapshot = take_snapshot(&source(), &git(), 2, &AtomicBool::new(false), &|p| {
            let mut phases = phases.lock().unwrap();
            if phases.last() != Some(&p.phase) {
                phases.push(p.phase);
            }
        })
        .unwrap();
        assert_eq!(
            *phases.lock().unwrap(),
            [
                SnapshotPhase::Git,
                SnapshotPhase::Walking,
                SnapshotPhase::Hashing,
                SnapshotPhase::Blobs
            ]
        );
        let repos: Vec<&str> = snapshot.git.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(repos, ["", "lib/sub"]);
        // The autocrlf git applied, `false` where unset.
        let autocrlf: Vec<Option<&str>> =
            snapshot.git.iter().map(|r| r.autocrlf.as_deref()).collect();
        assert_eq!(autocrlf, [Some("true"), Some("false")]);
        let skipped: Vec<&str> = snapshot
            .manifest
            .skipped
            .iter()
            .map(|s| s.path.as_str())
            .collect();
        assert_eq!(skipped, ["lib/sub/target", "node_modules", "web/dist"]);
        let blob = |path: &str| {
            snapshot
                .manifest
                .entries
                .iter()
                .find(|e| e.path.as_str() == path)
                .unwrap()
                .blob
                .clone()
        };
        assert_eq!(blob("README.md").as_deref(), Some("aaaa"));
        assert_eq!(blob("lib/sub/src.rs").as_deref(), Some("bbbb"));
        // A link's blob is its target's, not the file it points to.
        assert_eq!(blob("latest").as_deref(), Some("cccc"));
        assert_eq!(blob("notes.txt"), None);
        assert!(snapshot
            .manifest
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::File)
            .all(|e| e.sha256.is_some()));
        assert!(!snapshot.linux_git);
    }

    #[test]
    fn locked_files_and_external_links_refuse_the_snapshot() {
        let mut fs = source();
        fs.nodes.insert("data.db".into(), FakeNode::Locked(10));
        fs.nodes
            .insert("shared".into(), FakeNode::Link("D:\\shared".into()));
        let err = take_snapshot(&fs, &git(), 2, &AtomicBool::new(false), &|_| {}).unwrap_err();
        assert_eq!(
            err,
            SnapshotError::Refused(vec![Refusal::ExternalLinks {
                paths: vec!["shared".into()]
            }])
        );
        fs.nodes.remove("shared");
        let err = take_snapshot(&fs, &git(), 2, &AtomicBool::new(false), &|_| {}).unwrap_err();
        assert_eq!(
            err,
            SnapshotError::Refused(vec![Refusal::Unreadable {
                paths: vec!["data.db (locked)".into()]
            }])
        );
    }

    #[test]
    fn preflight_facts_come_from_the_walk_and_both_sides() {
        let mut fs = source();
        fs.nodes.insert("data.db".into(), FakeNode::Locked(10));
        fs.nodes
            .insert("shared".into(), FakeNode::Link("..\\elsewhere".into()));
        fs.nodes.insert(".git/MERGE_HEAD".into(), file("abc"));
        fs.nodes.insert(".git/lfs".into(), FakeNode::Dir);
        let (walk, unreadable) =
            preflight_walk(&fs, &git(), 2, &AtomicBool::new(false), &|_| {}).unwrap();
        let shape = repo_shape(&fs);
        assert_eq!(
            shape,
            RepoShape {
                is_git_repo: true,
                is_linked_worktree: false,
                has_linked_worktrees: false,
                operation: Some(GitOperation::Merge),
                uses_lfs: true,
            }
        );
        let linux = parse_linux_facts(
            "home=/home/dev\nlfs=0\nmissing=pnpm\nsessions=3\nexists=1\nfreekb=1000\n",
        );
        let facts = preflight_facts(
            &shape,
            true,
            &walk,
            &unreadable,
            &linux,
            Some("/home/dev/projects/aurora-notes"),
        );
        assert_eq!(facts.external_links, ["shared"]);
        assert_eq!(facts.unreadable, ["data.db (locked)"]);
        assert!(facts.dest_on_linux && facts.dest_exists);
        assert_eq!(facts.free_bytes, 1_024_000);
        let steps = install_steps(&walk, Some(r#"{"packageManager": "pnpm@9.1.0"}"#), None);
        assert_eq!(install_tools(&steps), ["pnpm"]);
        let summary = summarize(&facts, &walk, steps, &linux);
        let kinds: Vec<String> = summary
            .refusals
            .iter()
            .map(|r| {
                serde_json::to_value(r).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "operationInProgress",
                "externalLinks",
                "unreadable",
                "lfsMissing",
                "destExists",
                "notEnoughSpace"
            ]
        );
        assert!(summary.refusals[0].message.contains("merge"));
        assert_eq!(summary.warnings.len(), 1);
        assert_eq!(summary.install.len(), 1);
        assert_eq!(
            summary.install[0].step.command,
            "pnpm install --frozen-lockfile"
        );
        assert!(summary.install[0].missing);
        assert_eq!(summary.history_sessions, 3);
        let value = serde_json::to_value(&summary).unwrap();
        assert_eq!(value["install"][0]["tool"], "pnpm");
        assert_eq!(value["install"][0]["missing"], true);
        assert_eq!(value["historySessions"], 3);
    }

    #[test]
    fn a_dot_git_file_is_a_linked_worktree() {
        let fs = FakeFs::with(&[(".git", file("gitdir: C:/dev/main/.git/worktrees/x"))]);
        assert!(repo_shape(&fs).is_linked_worktree);
        let fs = FakeFs::with(&[("README.md", file("x"))]);
        assert!(!repo_shape(&fs).is_git_repo);
        let fs = FakeFs::with(&[
            (".git/HEAD", file("x")),
            (".git/worktrees/feature/HEAD", file("x")),
            (
                ".gitattributes",
                file("*.psd filter=lfs diff=lfs merge=lfs -text\n"),
            ),
        ]);
        let shape = repo_shape(&fs);
        assert!(shape.has_linked_worktrees && shape.uses_lfs);
    }

    #[test]
    fn linux_facts_are_read_leniently() {
        assert_eq!(
            parse_linux_facts("home=/home/dev\r\nlfs=1\nfreekb=12\n"),
            LinuxFacts {
                home: Some("/home/dev".into()),
                has_git_lfs: true,
                dest_exists: false,
                free_bytes: 12 * 1024,
                ..LinuxFacts::default()
            }
        );
        let facts = parse_linux_facts("missing=uv\nmissing=\nmissing=npm\nsessions=x\n");
        assert_eq!(facts.missing_tools.len(), 2);
        assert_eq!(facts.sessions, 0);
        let step = |tool: &str| InstallStep {
            folder: refract_core::migrate::manifest::RelPath::new("node_modules").unwrap(),
            tool: tool.into(),
            command: String::new(),
            install_hint: String::new(),
        };
        assert_eq!(
            install_tools(&[step("npm"), step("npm"), step("uv; rm -rf /"), step("uv")]),
            ["npm", "uv"]
        );
        assert_eq!(
            parse_linux_facts("garbage\nfreekb=\n"),
            LinuxFacts::default()
        );
        assert_eq!(default_roots("/home/dev/"), ["/home/dev/projects"]);
    }
}
