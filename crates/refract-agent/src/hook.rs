//! The optional status hook (spec §6.2) on disk: its script, installing it into and
//! removing it from Claude Code's `settings.json`, and reading the status files the
//! script writes. The edits themselves are [`refract_core::hook`]'s.
//!
//! Paths: the script at `$XDG_DATA_HOME/refract/status-hook.sh` (default
//! `~/.local/share`), the settings at `$CLAUDE_CONFIG_DIR/settings.json` (default
//! `~/.claude`), and in the state directory ([`crate::launch::state_dir`]) the status
//! files under `sessions/` and the original settings as `settings.backup.json`.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use refract_core::classify::HookReport;
use refract_core::diff::unified_diff;
use refract_core::hook::{self, Original, Uninstalled};
use refract_core::protocol::HookInfo;

use crate::launch;
use crate::state::path_str;

/// The script, written to [`Paths::script`] on install.
pub const SCRIPT: &str = include_str!("status-hook.sh");

/// Status files untouched this long are deleted when the directory is read in full.
pub const MAX_STATUS_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Where the hook's files are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// Claude Code's `settings.json`.
    pub settings: PathBuf,
    /// The installed script.
    pub script: PathBuf,
    /// The settings as they were before the install.
    pub backup: PathBuf,
    /// The status files.
    pub status_dir: PathBuf,
}

impl Paths {
    /// The paths for this environment. Relative `XDG_*` values are ignored.
    pub fn from_env() -> Paths {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()));
        let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".claude"));
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|v| v.is_absolute())
            .unwrap_or_else(|| home.join(".local/share"));
        let state = launch::state_dir();
        Paths {
            settings: claude.join("settings.json"),
            script: data.join("refract/status-hook.sh"),
            backup: state.join("settings.backup.json"),
            status_dir: status_dir(),
        }
    }
}

/// Where the script writes status files.
pub fn status_dir() -> PathBuf {
    launch::state_dir().join("sessions")
}

/// Whether the hook is installed, and what installing would change. A missing or
/// outdated script counts as not installed, so installing repairs it.
pub fn info(paths: &Paths) -> Result<HookInfo, String> {
    let current = read_settings(&paths.settings)?.unwrap_or_default();
    let (script, dir) = (path_str(&paths.script), path_str(&paths.status_dir));
    let installed = hook::is_installed(&current, &script, &dir)?
        && fs::read(&paths.script).is_ok_and(|bytes| bytes == SCRIPT.as_bytes());
    let diff = if installed {
        None
    } else {
        let next = hook::install(&current, &script, &dir)?;
        Some(unified_diff(
            &path_str(&paths.settings),
            &String::from_utf8_lossy(&current),
            &String::from_utf8_lossy(&next),
        ))
    };
    Ok(HookInfo {
        installed,
        settings_path: path_str(&paths.settings),
        diff,
    })
}

/// True when `settings.json` holds Refract entries, so status files are worth reading.
pub fn enabled(paths: &Paths) -> bool {
    read_settings(&paths.settings)
        .ok()
        .flatten()
        .is_some_and(|bytes| hook::has_entries(&bytes).unwrap_or(false))
}

/// Writes the script and adds it to `settings.json`, first saving the settings as they
/// are unless they already hold Refract entries. Nothing is written when the settings
/// cannot be read or edited.
pub fn install(paths: &Paths) -> Result<HookInfo, String> {
    let (script, dir) = (path_str(&paths.script), path_str(&paths.status_dir));
    let current = read_settings(&paths.settings)?;
    hook::install(current.as_deref().unwrap_or_default(), &script, &dir)?;
    write_file(&paths.script, SCRIPT.as_bytes(), 0o755)
        .map_err(|e| format!("cannot write {}: {e}", paths.script.display()))?;
    create_dir(&paths.status_dir)?;
    edit_settings(&paths.settings, |current| {
        let bytes = current.unwrap_or_default();
        let next = hook::install(bytes, &script, &dir)?;
        if next == bytes {
            return Ok(Edit::Keep);
        }
        if !hook::has_entries(bytes)? {
            save_backup(paths, current)?;
        }
        Ok(Edit::Write(next))
    })?;
    info(paths)
}

/// Removes Refract's entries from `settings.json`, giving back the saved bytes when
/// nothing else changed since the install. When there were entries, the backup and the
/// script go too. The status files always go.
pub fn uninstall(paths: &Paths) -> Result<HookInfo, String> {
    let saved = read_backup(paths)?;
    let original = saved.as_ref().map(|saved| match saved {
        Some(bytes) => Original::Bytes(bytes),
        None => Original::Missing,
    });
    let mut removed = false;
    edit_settings(&paths.settings, |current| {
        let Some(current) = current else {
            return Ok(Edit::Keep);
        };
        removed = hook::has_entries(current)?;
        Ok(match hook::uninstall(current, original)? {
            Uninstalled::Write(bytes) if bytes == current => Edit::Keep,
            Uninstalled::Write(bytes) => Edit::Write(bytes),
            Uninstalled::Remove => Edit::Remove,
        })
    })?;
    if removed {
        remove(&paths.backup)?;
        remove(&missing_marker(paths))?;
        remove(&paths.script)?;
    }
    remove_statuses(&paths.status_dir);
    info(paths)
}

/// The file saying there was no `settings.json` before the install, kept in place of
/// the backup.
fn missing_marker(paths: &Paths) -> PathBuf {
    paths.backup.with_extension("missing")
}

/// Saves the settings as they are before the install: their bytes, or the marker when
/// there is no file.
fn save_backup(paths: &Paths, settings: Option<&[u8]>) -> Result<(), String> {
    let (path, other, bytes) = match settings {
        Some(bytes) => (paths.backup.clone(), missing_marker(paths), bytes),
        None => (missing_marker(paths), paths.backup.clone(), &b""[..]),
    };
    write_file(&path, bytes, 0o600).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    remove(&other)
}

/// What [`save_backup`] saved: `Some(None)` when there was no `settings.json`, and
/// `None` when nothing was saved.
fn read_backup(paths: &Paths) -> Result<Option<Option<Vec<u8>>>, String> {
    match fs::read(&paths.backup) {
        Ok(bytes) => return Ok(Some(Some(bytes))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot read {}: {e}", paths.backup.display())),
    }
    Ok(missing_marker(paths).is_file().then_some(None))
}

/// The settings' bytes, or `None` when there is no file.
fn read_settings(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// What to do with `settings.json`.
enum Edit {
    Keep,
    Write(Vec<u8>),
    Remove,
}

/// Applies `edit` to `settings.json`, given its bytes (`None` when there is no file).
/// A symlinked file (kept in a dotfiles repo, say) has its target edited, so the link
/// stays, even a dangling one. The file keeps its mode. Right before the new file takes
/// the old one's place the settings are read again: when something else (Claude Code)
/// changed them meanwhile, `edit` runs once more on the new bytes, and a second change
/// is an error.
fn edit_settings(
    path: &Path,
    mut edit: impl FnMut(Option<&[u8]>) -> Result<Edit, String>,
) -> Result<(), String> {
    let target = resolve_links(path)?;
    let mut current = read_settings(&target)?;
    for _ in 0..2 {
        match edit(current.as_deref())? {
            Edit::Keep => return Ok(()),
            Edit::Write(bytes) => {
                let mode = fs::metadata(&target).map_or(0o600, |m| m.permissions().mode() & 0o7777);
                let tmp = write_tmp(&target, &bytes, mode)
                    .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                let now = read_settings(&target);
                if now.as_ref().is_ok_and(|now| *now == current) {
                    return fs::rename(&tmp, &target).map_err(|e| {
                        let _ = fs::remove_file(&tmp);
                        format!("cannot write {}: {e}", path.display())
                    });
                }
                let _ = fs::remove_file(&tmp);
                current = now?;
            }
            Edit::Remove => {
                let now = read_settings(&target)?;
                if now == current {
                    return remove(&target);
                }
                current = now;
            }
        }
    }
    Err(format!(
        "{} changed while Refract was editing it; try again",
        path.display()
    ))
}

/// `path` with every symlink at its end followed, relative links against the link's
/// directory. The target need not exist.
fn resolve_links(path: &Path) -> Result<PathBuf, String> {
    let mut path = path.to_owned();
    for _ in 0..40 {
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link = fs::read_link(&path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                path = path.parent().unwrap_or(Path::new("/")).join(link);
            }
            _ => return Ok(path),
        }
    }
    Err(format!("too many symlinks at {}", path.display()))
}

/// Writes `bytes` to `path` through a temporary file in the same directory, so a
/// reader never sees part of it, creating the directory when needed.
fn write_file(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let tmp = write_tmp(path, bytes, mode)?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// Writes `bytes` with `mode` to a temporary file next to `path` and returns its path.
fn write_tmp(path: &Path, bytes: &[u8], mode: u32) -> io::Result<PathBuf> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    fs::create_dir_all(dir)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.refract-{}.tmp", std::process::id()));
    let written = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .and_then(|mut file| {
            // The umask may have narrowed the mode at creation.
            file.set_permissions(fs::Permissions::from_mode(mode))?;
            file.write_all(bytes)?;
            file.sync_all()
        });
    match written {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn create_dir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

fn remove(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(format!("cannot remove {}: {e}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Deletes every status file in `dir`, and the script's leftover temporary files.
fn remove_statuses(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let temporary = name.starts_with('.') && name.ends_with(".tmp");
        if hook::status_file_session(&name).is_some() || temporary {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Every session's report, or none while the hook is not in `settings.json`.
pub fn statuses(paths: &Paths) -> HashMap<String, HookReport> {
    if enabled(paths) {
        read_statuses(&paths.status_dir)
    } else {
        HashMap::new()
    }
}

/// Every session's report in `dir`, by session id. Files untouched for
/// [`MAX_STATUS_AGE`] are deleted instead, since no running session is that quiet.
pub fn read_statuses(dir: &Path) -> HashMap<String, HookReport> {
    let mut statuses = HashMap::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return statuses;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = hook::status_file_session(&name) else {
            continue;
        };
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > MAX_STATUS_AGE));
        if old {
            let _ = fs::remove_file(entry.path());
            continue;
        }
        if let Some(report) = read_status(&entry.path()) {
            statuses.insert(id.to_owned(), report);
        }
    }
    statuses
}

/// Re-reads the status files named `names` in `dir` into `statuses`; a file that is
/// gone or unreadable drops its session's report.
pub fn update_statuses<'a>(
    dir: &Path,
    statuses: &mut HashMap<String, HookReport>,
    names: impl IntoIterator<Item = &'a String>,
) {
    for name in names {
        let Some(id) = hook::status_file_session(name) else {
            continue;
        };
        match read_status(&dir.join(name)) {
            Some(report) => statuses.insert(id.to_owned(), report),
            None => statuses.remove(id),
        };
    }
}

/// A status file's report. Only regular files are read (a FIFO would block), and only
/// their first 4 KiB: a status file is one short line, and anything longer is not one.
fn read_status(path: &Path) -> Option<HookReport> {
    if !fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(4096)
        .read_to_end(&mut bytes)
        .ok()?;
    let file = hook::parse_status_file(&bytes)?;
    Some(HookReport {
        status: file.status,
        at: file.at,
    })
}

#[cfg(test)]
mod tests {
    use refract_core::classify::HookStatus;

    use super::*;

    const ID: &str = "3f6c1a52-0000-4000-8000-000000000001";

    fn paths(root: &Path) -> Paths {
        Paths {
            settings: root.join("claude/settings.json"),
            script: root.join("share/refract/status-hook.sh"),
            backup: root.join("state/refract/settings.backup.json"),
            status_dir: root.join("state/refract/sessions"),
        }
    }

    #[test]
    fn install_and_uninstall_round_trip_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let original = b"{\n    \"theme\": \"dark\"\n}";
        fs::create_dir_all(p.settings.parent().unwrap()).unwrap();
        fs::write(&p.settings, original).unwrap();
        fs::set_permissions(&p.settings, fs::Permissions::from_mode(0o640)).unwrap();

        let before = info(&p).unwrap();
        assert!(!before.installed);
        let diff = before.diff.unwrap();
        assert!(diff.contains("+  \"hooks\": {"), "{diff}");
        assert!(diff.contains("-    \"theme\": \"dark\""), "{diff}");

        let after = install(&p).unwrap();
        assert!(after.installed);
        assert_eq!(after.diff, None);
        assert_eq!(fs::read(&p.backup).unwrap(), original);
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&p.script), 0o755);
        assert_eq!(mode(&p.settings), 0o640);
        assert_eq!(fs::read_to_string(&p.script).unwrap(), SCRIPT);
        assert!(p.status_dir.is_dir());
        // A second install changes nothing and keeps the first backup.
        let installed = fs::read(&p.settings).unwrap();
        assert!(install(&p).unwrap().installed);
        assert_eq!(fs::read(&p.settings).unwrap(), installed);
        assert_eq!(fs::read(&p.backup).unwrap(), original);

        assert!(!uninstall(&p).unwrap().installed);
        assert_eq!(fs::read(&p.settings).unwrap(), original);
        assert!(!p.backup.exists());
        assert!(!p.script.exists());
    }

    #[test]
    fn a_missing_settings_file_is_created_then_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        assert!(install(&p).unwrap().installed);
        assert!(p.settings.is_file());
        uninstall(&p).unwrap();
        assert!(!p.settings.exists());
    }

    #[test]
    fn broken_settings_are_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        fs::create_dir_all(p.settings.parent().unwrap()).unwrap();
        fs::write(&p.settings, "{ oops").unwrap();
        assert!(info(&p).is_err());
        assert!(install(&p).is_err());
        assert_eq!(fs::read_to_string(&p.settings).unwrap(), "{ oops");
        assert!(!p.script.exists() && !p.backup.exists());
    }

    #[test]
    fn a_symlinked_settings_file_stays_a_link() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        let real = tmp.path().join("dotfiles/settings.json");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::create_dir_all(p.settings.parent().unwrap()).unwrap();
        fs::write(&real, "{}\n").unwrap();
        std::os::unix::fs::symlink(&real, &p.settings).unwrap();
        install(&p).unwrap();
        assert!(fs::symlink_metadata(&p.settings).unwrap().is_symlink());
        assert!(fs::read_to_string(&real)
            .unwrap()
            .contains("status-hook.sh"));
        uninstall(&p).unwrap();
        assert_eq!(fs::read_to_string(&real).unwrap(), "{}\n");
    }

    #[test]
    fn status_files_are_read_and_updated() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(
            dir.join(format!("{ID}.json")),
            r#"{"status":"working","at":17}"#,
        )
        .unwrap();
        fs::write(dir.join(format!(".{ID}.42.tmp")), r#"{"status":"ended"}"#).unwrap();
        fs::write(dir.join("notes.json"), r#"{"status":"ended"}"#).unwrap();
        let mut statuses = read_statuses(dir);
        let report = |status, at| HookReport { status, at };
        assert_eq!(
            statuses,
            HashMap::from([(ID.to_owned(), report(HookStatus::Working, Some(17)))])
        );
        let name = format!("{ID}.json");
        fs::write(dir.join(&name), r#"{"status":"waitingOnUser"}"#).unwrap();
        update_statuses(dir, &mut statuses, [&name]);
        assert_eq!(statuses[ID], report(HookStatus::WaitingOnUser, None));
        fs::remove_file(dir.join(&name)).unwrap();
        update_statuses(dir, &mut statuses, [&name]);
        assert!(statuses.is_empty());
    }

    #[test]
    fn odd_status_files_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let other = "3f6c1a52-0000-4000-8000-000000000002";
        // A FIFO would block a reader; a long file is cut at 4 KiB and so is no status.
        let fifo = std::process::Command::new("mkfifo")
            .arg(dir.join(format!("{ID}.json")))
            .status()
            .unwrap();
        assert!(fifo.success());
        let long = format!(r#"{{"status":"working","event":"{}"}}"#, "x".repeat(5000));
        fs::write(dir.join(format!("{other}.json")), long).unwrap();
        assert!(read_statuses(dir).is_empty());
    }

    fn installed_paths(tmp: &Path, settings: &str) -> Paths {
        let p = paths(tmp);
        fs::create_dir_all(p.settings.parent().unwrap()).unwrap();
        fs::write(&p.settings, settings).unwrap();
        install(&p).unwrap();
        p
    }

    #[test]
    fn a_missing_script_needs_repair() {
        let tmp = tempfile::tempdir().unwrap();
        let p = installed_paths(tmp.path(), "{}\n");
        let settings = fs::read(&p.settings).unwrap();
        fs::remove_file(&p.script).unwrap();
        assert!(!info(&p).unwrap().installed);
        fs::write(&p.script, "echo old").unwrap();
        assert!(!info(&p).unwrap().installed);
        assert!(install(&p).unwrap().installed);
        assert_eq!(fs::read_to_string(&p.script).unwrap(), SCRIPT);
        assert_eq!(fs::read(&p.settings).unwrap(), settings);
    }

    #[test]
    fn uninstall_keeps_the_script_without_entries_to_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let p = installed_paths(tmp.path(), "{}\n");
        // The entries were removed by hand.
        fs::write(&p.settings, "{}\n").unwrap();
        uninstall(&p).unwrap();
        assert!(p.script.is_file());
        assert!(p.backup.is_file());
    }

    #[test]
    fn uninstall_removes_status_files() {
        let tmp = tempfile::tempdir().unwrap();
        let p = installed_paths(tmp.path(), "{}\n");
        fs::write(
            p.status_dir.join(format!("{ID}.json")),
            r#"{"status":"working"}"#,
        )
        .unwrap();
        fs::write(p.status_dir.join(format!(".{ID}.7.tmp")), "").unwrap();
        assert!(enabled(&p));
        assert_eq!(statuses(&p).len(), 1);
        uninstall(&p).unwrap();
        assert!(!enabled(&p));
        assert_eq!(fs::read_dir(&p.status_dir).unwrap().count(), 0);
        // A status written without the hook installed is ignored.
        fs::write(
            p.status_dir.join(format!("{ID}.json")),
            r#"{"status":"working"}"#,
        )
        .unwrap();
        assert!(statuses(&p).is_empty());
    }

    #[test]
    fn an_empty_settings_file_is_restored_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let p = installed_paths(tmp.path(), "");
        assert!(info(&p).unwrap().installed);
        uninstall(&p).unwrap();
        assert_eq!(fs::read(&p.settings).unwrap(), b"");
        assert!(!missing_marker(&p).exists());
    }

    #[test]
    fn a_dangling_settings_link_stays_a_link() {
        let tmp = tempfile::tempdir().unwrap();
        let p = paths(tmp.path());
        fs::create_dir_all(p.settings.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("../dotfiles/claude.json", &p.settings).unwrap();
        let real = tmp.path().join("dotfiles/claude.json");
        assert!(install(&p).unwrap().installed);
        assert!(fs::symlink_metadata(&p.settings).unwrap().is_symlink());
        assert!(fs::read_to_string(&real)
            .unwrap()
            .contains("status-hook.sh"));
        uninstall(&p).unwrap();
        assert!(fs::symlink_metadata(&p.settings).unwrap().is_symlink());
        assert!(!real.exists());
    }

    #[test]
    fn a_concurrent_settings_change_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        fs::write(&path, "a").unwrap();
        // Claude Code writes the file while the first edit is being made.
        let mut seen = Vec::new();
        edit_settings(&path, |current| {
            let current = current.unwrap().to_vec();
            if seen.is_empty() {
                fs::write(&path, "b").unwrap();
            }
            seen.push(current.clone());
            Ok(Edit::Write([current, b"+".to_vec()].concat()))
        })
        .unwrap();
        assert_eq!(seen, [b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(fs::read(&path).unwrap(), b"b+");
        // A file that keeps changing is left alone, with an error.
        let mut n = 0;
        let err = edit_settings(&path, |_| {
            n += 1;
            fs::write(&path, format!("c{n}")).unwrap();
            Ok(Edit::Write(b"mine".to_vec()))
        })
        .unwrap_err();
        assert!(
            err.contains("changed while Refract was editing it"),
            "{err}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"c2");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn old_status_files_are_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(format!("{ID}.json"));
        fs::write(&path, r#"{"status":"finished"}"#).unwrap();
        let old = SystemTime::now() - MAX_STATUS_AGE - Duration::from_secs(60);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(read_statuses(tmp.path()).is_empty());
        assert!(!path.exists());
    }
}
