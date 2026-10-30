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

/// Normalises the copied repo whose working tree is `dir` (spec §3.3): for each tracked
/// file (only those in `only`, when given; paths relative to `dir`) the blob Windows
/// git would store is written to the copy's object store with `windows`, the Windows-only
/// local config keys are replaced ([`config_changes`]), each file is rewritten as Linux
/// git checks that blob out (`git cat-file --batch --filters`), with its mode from the
/// index (a symlink entry becomes a symlink), and the index is refreshed. A file whose
/// bytes do not change is left alone; a rewritten one keeps its modification time.
///
/// The copied index still holds each file's Windows size, and git takes a file whose
/// size differs from the recorded one for modified without reading it. So the entries
/// of rewritten files are first written again with the same mode and object
/// (`update-index --index-info`), which clears their recorded stat, and the refresh
/// then compares their content. Writing an entry again drops its skip-worktree and
/// assume-unchanged flags, so they are read first and set again after the refresh;
/// intent-to-add entries are not written again at all, since that would stage them as
/// empty files (their recorded stat is empty anyway).
/// `progress` is called with each file done. Returns per-path failures.
pub fn normalise(
    dir: &Path,
    windows: &[String],
    only: Option<&BTreeSet<String>>,
    progress: &mut dyn FnMut(),
) -> Result<Vec<(String, String)>, String> {
    let index = index(dir)?;
    let mut problems = Vec::new();
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
        problems.push((
            path.clone(),
            "Git could not read it to normalise it.".into(),
        ));
    }
    let rewritten = smudge(dir, &want, &index, &mut problems, progress)?;
    let (skip, assume) = flagged(dir)?;
    let intent = if rewritten.is_empty() {
        BTreeSet::new()
    } else {
        intent_to_add(dir)?
    };
    let rewritten: Vec<String> = rewritten
        .into_iter()
        .filter(|p| !intent.contains(p))
        .collect();
    if !rewritten.is_empty() {
        let mut input = Vec::new();
        for entry in rewritten.iter().filter_map(|p| index.get(p)) {
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
        rewritten.iter().filter(|p| flags.contains(*p)).collect()
    };
    set_flag(dir, "--skip-worktree", &lost(&skip))?;
    set_flag(dir, "--assume-unchanged", &lost(&assume))?;
    Ok(problems)
}

/// Rewrites each (path, blob) as Linux git checks the blob out. Returns the paths whose
/// bytes or kind changed.
fn smudge(
    dir: &Path,
    want: &[(&String, &String)],
    index: &BTreeMap<String, IndexEntry>,
    problems: &mut Vec<(String, String)>,
    progress: &mut dyn FnMut(),
) -> Result<Vec<String>, String> {
    let usable: Vec<&(&String, &String)> = want.iter().filter(|(p, _)| !p.contains('\n')).collect();
    for (path, _) in want.iter().filter(|(p, _)| p.contains('\n')) {
        problems.push(((*path).clone(), "Its name holds a line break.".into()));
    }
    let mut rewritten = Vec::new();
    if usable.is_empty() {
        return Ok(rewritten);
    }
    let mut child = command(dir, &[])
        .args(["cat-file", "--batch", "--filters"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run git: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let input: Vec<u8> = usable
        .iter()
        .flat_map(|(path, blob)| format!("{blob} {path}\n").into_bytes())
        .collect();
    let result = std::thread::scope(|scope| {
        scope.spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let mut reader = BufReader::new(stdout);
        for (path, _) in &usable {
            let mut header = String::new();
            if reader.read_line(&mut header).map_err(|e| e.to_string())? == 0 {
                return Err("git cat-file stopped early".to_string());
            }
            let parts: Vec<&str> = header.trim_end().split(' ').collect();
            let size = match parts.as_slice() {
                [_, "blob", size] => size.parse::<usize>().map_err(|e| e.to_string())?,
                _ => {
                    problems.push(((*path).clone(), "Git could not check it out.".into()));
                    continue;
                }
            };
            let mut content = vec![0; size];
            reader.read_exact(&mut content).map_err(|e| e.to_string())?;
            let mut newline = [0u8; 1];
            reader.read_exact(&mut newline).map_err(|e| e.to_string())?;
            let mode = index.get(*path).map_or(0o100644, |e| e.mode);
            match rewrite(&dir.join(path), &content, mode) {
                Ok(true) => rewritten.push((*path).clone()),
                Ok(false) => {}
                Err(e) => problems.push(((*path).clone(), format!("Could not rewrite it: {e}"))),
            }
            progress();
        }
        Ok(())
    });
    let _ = child.wait();
    result.map(|()| rewritten)
}

/// Writes `content` to `path` as a file of git `mode` (or a symlink to it, for
/// `120000`), keeping the old modification time. Returns whether its bytes or kind
/// changed.
fn rewrite(path: &Path, content: &[u8], mode: u32) -> io::Result<bool> {
    let meta = fs::symlink_metadata(path)?;
    let mtime = fsx::mtime_ns(&meta);
    if mode == 0o120000 {
        let target = std::ffi::OsStr::from_bytes(content);
        fs::remove_file(path)?;
        std::os::unix::fs::symlink(target, path)?;
        fsx::set_mtime(path, mtime)?;
        return Ok(true);
    }
    let perms = if mode == 0o100755 { 0o755 } else { 0o644 };
    let changed = fs::read(path)? != content;
    if changed {
        // Truncated in place, so the folder's modification time stays.
        fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)?
            .write_all(content)?;
    }
    if meta.permissions().mode() & 0o777 != perms {
        fs::set_permissions(path, fs::Permissions::from_mode(perms))?;
    }
    fsx::set_mtime(path, mtime)?;
    Ok(changed)
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
