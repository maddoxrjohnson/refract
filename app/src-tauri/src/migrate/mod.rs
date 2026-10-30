//! The Windows side of **Move to WSL** (`.plans/wsl-migration-spec.md`): the
//! preflight, the source snapshot taken natively with `git.exe`, the helper requests,
//! and **Delete original** with the Recycle Bin, the `.lnk` and the VS Code windows.
//! The rules themselves are in `refract_core::migrate`.
//!
//! # Commands
//!
//! Paths are Linux paths as the helper reports repos (`/mnt/c/...` for the source).
//! Every command fails with a [`MigrateError`] `{kind, message, refusals?}`.
//!
//! | Command | Args | Answer |
//! |---|---|---|
//! | `migrate_preflight` | `path`, `dest?` | [`Preflight`] |
//! | `migrate_start` | `path`, `dest` | `{id}` once the snapshot is taken and the helper started |
//! | `migrate_cancel_snapshot` | `path` | `true` when a preflight or snapshot of `path` was stopped |
//! | `migrate_cancel` | `id` | `null`: the helper deletes the partial copy |
//! | `migrate_windows` | `path` | [`VsCodeWindow`]`[]` that have the folder open |
//! | `migrate_close_window` | `path`, `id` | `true` when asked to close, `false` when gone |
//! | `migrate_stop_agent` | `path`, `sessionId` | `null` |
//! | `migrate_resync` | `id` | `null`; progress and a new `migrateReady` follow |
//! | `migrate_delete` | `id`, `permanent` | [`DeleteOutcome`] |
//! | `migrate_finish` | `id`, `action` (`keep` or `discard`) | `null` |
//! | `migrate_pending` | | [`PendingMigration`]`[]`, after a restart |
//! | `migrate_source_exists` | `id` | `false` once the original folder is gone |
//!
//! `migrate_preflight` and `migrate_start` walk and read the whole source, so they
//! can take a while; both send `migrate://snapshot` events and stop on
//! `migrate_cancel_snapshot` (failing with `cancelled`). `migrate_start` takes the
//! snapshot again rather than trusting the preflight's, and refuses (`busy`) while a
//! VS Code window has the folder open or a migration of it runs. Agents that are busy
//! block **Start** in the dialog; the interface knows them from the repo's sessions.
//!
//! **Delete original** is `migrate_delete {id, permanent: false}`. It checks the VS
//! Code windows and the Recycle Bin, then sends `migrateBeginDelete`, which rechecks
//! the original and puts the helper in its *deleting* state, then removes the folder
//! and finishes (see `flow`). Its outcome, each but `deleted` and `unfinished` with
//! `deleting` (the helper is in the deleting state):
//! - `changed {changed, removed, unreadable, copyChanged, busyAgents, sourceMissing}`:
//!   the recheck failed and nothing was deleted. Not deleting: offer **Re-sync**
//!   (`migrate_resync`) for changed files, **Keep both** for a changed copy, **Stop**
//!   for idle agents, then try again. Deleting: only **Finish deleting** once fixed;
//! - `windowsOpen {windows}`: offer **Close** for each, then try again;
//! - `wontFit {reason}`: ask "Delete permanently?" (default **No**, which is
//!   `migrate_finish keep`, or, while deleting, leaving it), and on **Yes** call
//!   `migrate_delete {id, permanent: true}`;
//! - `driveHidden`: the original is there on Windows but not over `/mnt` (its drive
//!   isn't mounted in WSL), so it could not be checked and nothing was deleted. Make
//!   the drive visible in WSL, then try again (or finish deleting);
//! - `unfinished {reason}`: the helper is in the deleting state but the original is not
//!   all gone (a file was in use) or the helper did not finish. Offer only **Finish
//!   deleting** (`migrate_delete` again), never Re-sync, Keep or Discard;
//! - `deleted {permanent, alreadyGone, shortcut?, shortcutError?}`: done; the gem moves
//!   to `dest`. `alreadyGone` when the original had been deleted by hand.
//!
//! The deleting state is the helper's and survives restarts: `migrate_pending` marks it
//! `deleting`. While in it, `migrate_resync`, `migrate_finish` and `migrate_cancel`
//! refuse here too. **Keep both** is `migrate_finish keep` (the interface then hides
//! the old gem); **Discard the copy** is `migrate_finish discard`, refused once the
//! original is gone, as is `migrate_resync`. `migrate_cancel` works only while the copy
//! runs. One command at a time runs per migration (`busy` otherwise).
//!
//! # Events
//!
//! - `migrate://snapshot`: `{path, phase, filesDone, filesTotal, bytesDone,
//!   bytesTotal}` while a preflight or snapshot runs; `phase` is `git`, `walking`,
//!   `probing` (preflight), `hashing` or `blobs` (snapshot).
//! - The helper's `migrateProgress {id, source, dest, step, done, total}` and
//!   `migrateReady {id, report}` reach the interface on `agent://event` like every
//!   helper event. The app also reads `migrateReady` to know which migrations may be
//!   deleted.
//!
//! Helper errors keep their meaning ([`MigrateError::agent`]): an unknown migration or
//! one not at that step is `notReady`, a working agent `busy`; a `migrate` the helper's
//! preflight refuses is `refused` (without `refusals`, the message says why).
//!
//! A migration that becomes ready, or fails, while the window is hidden shows a
//! Windows notification ([`notify`], through `tauri-plugin-notification`).
//!
//! The snapshot file goes to `%LOCALAPPDATA%\<identifier>\migrate`, written under a
//! temporary name and renamed into place before `migrate` is sent, and is deleted when
//! the migration is cancelled, finished or fails to start. Files no migration of this
//! run uses are swept at each start, since the helper reads its file only once; a
//! start registers its file's name before writing it, under the same lock as the
//! sweep, so one start never sweeps another's file.
//!
//! A source whose root is a link (symlink or junction), that has names ending in a dot
//! or a space, or files that are only in the cloud (OneDrive placeholders, which
//! reading would download) is refused. A source inside a OneDrive folder gets a warning
//! in the preflight: deleting the original deletes it from OneDrive too.

mod flow;
mod git;
mod snapshot;
mod vscode;
mod walk;
mod win;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use refract_client::AgentStatus;
use refract_core::launch::wsl_path;
use refract_core::migrate::preflight::{default_destination, Refusal};
use refract_core::migrate::report::Report;
use refract_core::migrate::SourceSnapshot;
use refract_core::protocol::{Event, FinishAction, MigrateParams, MigrationTicket};
use refract_core::sessions::{encode_project_dir, is_windows_mount};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::agent::{distro_args, wsl_command, Agent};
use crate::open::windows_folder;
use crate::process::output_with_deadline;
use flow::{delete_original, AgentHelper, DeleteTarget, MigrateHelper, WindowsOps};
pub use flow::{DeleteOutcome, MigrateError, MigrateErrorKind, PendingMigration};
use git::{find_git_exe, GitExe, GitRun, WslGit};
use snapshot::{
    default_roots, install_steps, install_tools, parse_linux_facts, preflight_facts,
    preflight_walk, repo_shape, summarize, take_snapshot, LinuxFacts, PreflightSummary,
    SnapshotError, SnapshotProgress, WindowsRefusal, LINUX_FACTS_SCRIPT,
};
pub use vscode::VsCodeWindow;
use walk::{SourceFs as _, Walk};

/// Emitted with each [`SnapshotEvent`].
pub const SNAPSHOT_EVENT: &str = "migrate://snapshot";

/// How long the Linux facts script gets. WSL may still be starting.
const LINUX_FACTS_TIMEOUT: Duration = Duration::from_secs(30);

/// Deletes run one at a time.
static DELETES: Mutex<()> = Mutex::new(());
/// Held while the snapshot folder is swept and a new file's name registered.
static SWEEP: Mutex<()> = Mutex::new(());

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The migrations this run knows of, managed as Tauri state.
#[derive(Default)]
pub struct Migrations {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    by_id: HashMap<String, Migration>,
    /// Cancel flags of the preflights and snapshots running, by source path.
    running: HashMap<String, Arc<AtomicBool>>,
    /// Snapshot files written and not yet handed over.
    files: HashSet<PathBuf>,
    /// Migrations a command is acting on.
    busy: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Migration {
    /// The source's Linux path.
    source: String,
    dest: String,
    snapshot_file: Option<PathBuf>,
    /// Files and bytes of the source, skipped folders included.
    files: u64,
    bytes: u64,
    phase: Phase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Copying and checking, until `migrateReady`.
    Running,
    Ready {
        can_delete: bool,
    },
    /// The helper is in the deleting state (`migrateBeginDelete` passed): only finishing
    /// the delete is left.
    Deleting,
}

impl Migrations {
    /// Records a report from `migrateReady` or `migratePending`; `deleting` when the
    /// helper says it is in the deleting state.
    fn ready(&self, id: &str, report: &Report, deleting: bool) {
        let files =
            report.source_totals.files + report.skipped.iter().map(|s| s.files).sum::<u64>();
        let bytes =
            report.source_totals.bytes + report.skipped.iter().map(|s| s.bytes).sum::<u64>();
        let mut inner = lock(&self.inner);
        let migration = inner
            .by_id
            .entry(id.to_owned())
            .or_insert_with(|| Migration {
                source: report.source.clone(),
                dest: report.dest.clone(),
                snapshot_file: None,
                files,
                bytes,
                phase: Phase::Running,
            });
        migration.files = files;
        migration.bytes = bytes;
        if deleting {
            migration.phase = Phase::Deleting;
        } else if migration.phase != Phase::Deleting {
            migration.phase = Phase::Ready {
                can_delete: report.can_delete(),
            };
        }
    }

    /// The helper stopped: the copies it was making are gone with it (it deletes them
    /// when it starts again), so they no longer block **Start**.
    fn helper_stopped(&self) {
        let stale: Vec<Migration> = {
            let mut inner = lock(&self.inner);
            let ids: Vec<String> = inner
                .by_id
                .iter()
                .filter(|(_, m)| m.phase == Phase::Running)
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| inner.by_id.remove(id)).collect()
        };
        for file in stale.into_iter().filter_map(|m| m.snapshot_file) {
            remove_snapshot_file(&file);
        }
    }

    /// Marks `id` busy until the guard drops; `busy` when another command acts on it.
    fn claim(&self, id: &str) -> Result<Claim<'_>, MigrateError> {
        if !lock(&self.inner).busy.insert(id.to_owned()) {
            return Err(MigrateError::new(
                MigrateErrorKind::Busy,
                "Another answer to this move is still being carried out.",
            ));
        }
        Ok(Claim {
            migrations: self,
            id: id.to_owned(),
        })
    }

    fn get(&self, id: &str) -> Result<Migration, MigrateError> {
        lock(&self.inner).by_id.get(id).cloned().ok_or_else(|| {
            MigrateError::new(
                MigrateErrorKind::NotReady,
                format!("No migration {id} is known."),
            )
        })
    }

    fn set_phase(&self, id: &str, phase: Phase) {
        if let Some(m) = lock(&self.inner).by_id.get_mut(id) {
            m.phase = phase;
        }
    }

    /// Forgets a migration and deletes its snapshot file.
    fn forget(&self, id: &str) {
        let removed = lock(&self.inner).by_id.remove(id);
        if let Some(file) = removed.and_then(|m| m.snapshot_file) {
            remove_snapshot_file(&file);
        }
    }
}

/// A migration a command acts on; see [`Migrations::claim`].
struct Claim<'a> {
    migrations: &'a Migrations,
    id: String,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        lock(&self.migrations.inner).busy.remove(&self.id);
    }
}

/// Called with every helper status change.
pub fn on_helper_status(app: &AppHandle, status: &AgentStatus) {
    if *status != AgentStatus::Running {
        if let Some(migrations) = app.try_state::<Migrations>() {
            migrations.helper_stopped();
        }
    }
}

/// Called with every helper event before it is forwarded to the interface.
pub fn on_helper_event(app: &AppHandle, event: &Event) {
    let Event::MigrateReady { id, report } = event else {
        return;
    };
    if let Some(migrations) = app.try_state::<Migrations>() {
        migrations.ready(id, report, false);
    }
    if !crate::window::window_visible(app) {
        let name = report.source.rsplit('/').next().unwrap_or(&report.source);
        if report.can_delete() {
            notify(
                app,
                &format!("{name} is ready to move"),
                "Open Refract to confirm.",
            );
        } else {
            notify(
                app,
                &format!("Moving {name} failed"),
                "Open Refract to see why.",
            );
        }
    }
}

/// Shows a Windows notification; a failure is only logged.
fn notify(app: &AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    if let Err(err) = app.notification().builder().title(title).body(body).show() {
        eprintln!("refract: cannot show the notification {title:?}: {err}");
    }
}

/// One progress report of a preflight or snapshot, sent as `migrate://snapshot`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotEvent {
    pub path: String,
    #[serde(flatten)]
    pub progress: SnapshotProgress,
}

/// What `migrate_preflight` answers: what the dialog shows (spec §2.2).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preflight {
    /// The source's Linux and Windows paths, and its folder name.
    pub source: String,
    pub windows_path: String,
    pub name: String,
    /// The destination asked for, else the default one; `None` when none could be
    /// found (WSL's home unknown).
    pub dest: Option<String>,
    #[serde(flatten)]
    pub summary: PreflightSummary,
    /// VS Code windows that have the folder open; each blocks **Start**.
    pub vscode_windows: Vec<VsCodeWindow>,
    /// `git.exe` is missing, so Linux git reads the source.
    pub linux_git: bool,
    /// The source is in a folder OneDrive (or another cloud engine) syncs: deleting the
    /// original deletes it from the cloud and the other devices too.
    pub cloud_synced: bool,
}

/// The source's Windows path (`C:\dev\aurora-notes`) and folder name from its Linux
/// path, which must be a folder on a Windows drive, not the drive itself.
fn source_paths(path: &str) -> Result<(String, String), MigrateError> {
    let invalid =
        |why: &str| MigrateError::new(MigrateErrorKind::Invalid, format!("{why}: {path}"));
    if !is_windows_mount(path) {
        return Err(MigrateError::refused(vec![Refusal::NotOnWindowsDrive]));
    }
    let windows = windows_folder(path, None).map_err(|err| invalid(&err.message))?;
    // Win32 paths drop a trailing dot or space, so the folder Windows would act on is
    // another one.
    if path
        .trim_end_matches('/')
        .split('/')
        .any(walk::bad_win32_name)
    {
        return Err(invalid(
            "Windows can't safely handle folder names that end in a dot or a space",
        ));
    }
    let name = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    if windows.ends_with('\\') || name.is_empty() {
        return Err(invalid("Cannot move a whole drive"));
    }
    Ok((windows, name))
}

/// Refuses a source whose folder is itself a link: recycling it would remove only the
/// link and leave the files it points to.
fn check_root(windows: &str) -> Result<(), MigrateError> {
    if win::root_facts(windows).is_link {
        return Err(MigrateError::refused_views(vec![
            WindowsRefusal::SourceIsLink.into(),
        ]));
    }
    Ok(())
}

/// The source is in a folder OneDrive or another cloud engine syncs.
fn cloud_synced(windows: &str) -> bool {
    win::root_facts(windows).cloud || win::inside_any(windows, &win::onedrive_roots())
}

/// Threads that hash at once.
fn threads() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// `git.exe`, else Linux git through `wsl.exe`.
fn git_runner(
    path: &str,
    windows: &str,
    distro: Option<String>,
    cancel: Arc<AtomicBool>,
) -> Box<dyn GitRun> {
    let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from);
    let path_var = std::env::var("PATH").ok();
    match find_git_exe(path_var.as_deref(), program_files.as_deref(), Path::is_file) {
        Some(exe) => Box::new(GitExe {
            exe,
            root: PathBuf::from(windows),
            cancel,
        }),
        None => {
            eprintln!("refract: git.exe not found; Linux git reads {path}");
            Box::new(WslGit {
                distro,
                root: path.to_owned(),
                cancel,
            })
        }
    }
}

/// Registers a cancel flag for `path`'s preflight or snapshot, removed on drop.
struct Running<'a> {
    migrations: &'a Migrations,
    path: String,
    cancel: Arc<AtomicBool>,
}

impl<'a> Running<'a> {
    fn start(migrations: &'a Migrations, path: &str) -> Result<Running<'a>, MigrateError> {
        let mut inner = lock(&migrations.inner);
        if inner.running.contains_key(path) {
            return Err(MigrateError::new(
                MigrateErrorKind::Busy,
                "This folder is already being checked.",
            ));
        }
        let cancel = Arc::new(AtomicBool::new(false));
        inner.running.insert(path.to_owned(), cancel.clone());
        Ok(Running {
            migrations,
            path: path.to_owned(),
            cancel,
        })
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        lock(&self.migrations.inner).running.remove(&self.path);
    }
}

fn snapshot_error(err: SnapshotError) -> MigrateError {
    match err {
        SnapshotError::Cancelled => MigrateError::new(MigrateErrorKind::Cancelled, "Cancelled."),
        SnapshotError::Git(message) => MigrateError::new(MigrateErrorKind::Git, message),
        SnapshotError::Refused(refusals) => MigrateError::refused_views(refusals),
    }
}

fn emitter(app: &AppHandle, path: &str) -> impl Fn(SnapshotProgress) + Sync {
    let (app, path) = (app.clone(), path.to_owned());
    move |progress| {
        let event = SnapshotEvent {
            path: path.clone(),
            progress,
        };
        if let Err(err) = app.emit(SNAPSHOT_EVENT, event) {
            eprintln!("refract: sending snapshot progress: {err}");
        }
    }
}

/// Runs [`LINUX_FACTS_SCRIPT`] in the helper's distro, for the source at `source` (its
/// Linux path) and the install `tools`.
fn linux_facts(
    distro: Option<&str>,
    dest: Option<&str>,
    source: &str,
    tools: &[String],
) -> LinuxFacts {
    let mut command = wsl_command();
    command
        .args(distro_args(distro))
        .args([
            "--exec",
            "sh",
            "-c",
            LINUX_FACTS_SCRIPT,
            "sh",
            dest.unwrap_or(""),
            &encode_project_dir(source),
            &tools.join(" "),
        ])
        .stdin(std::process::Stdio::null());
    match output_with_deadline(&mut command, LINUX_FACTS_TIMEOUT) {
        Ok(out) => parse_linux_facts(&String::from_utf8_lossy(&out.stdout)),
        Err(err) => {
            eprintln!("refract: cannot read WSL facts for the move: {err}");
            LinuxFacts::default()
        }
    }
}

/// Whether something exists at the Linux path `path`, looked up from Windows through
/// `\\wsl.localhost`. False when the distro is unknown; the helper refuses an existing
/// destination anyway.
fn exists_in_wsl(distro: Option<&str>, path: &str) -> bool {
    let Ok(windows) = windows_folder(path, distro) else {
        return false;
    };
    match std::fs::symlink_metadata(windows) {
        Ok(_) => true,
        Err(err) => err.kind() != std::io::ErrorKind::NotFound,
    }
}

fn internal(err: impl std::fmt::Display) -> MigrateError {
    MigrateError::new(
        MigrateErrorKind::Internal,
        format!("The task failed: {err}"),
    )
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, MigrateError> + Send + 'static,
) -> Result<T, MigrateError> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(internal)?
}

/// The preflight (see the module docs).
#[tauri::command]
pub async fn migrate_preflight(
    app: AppHandle,
    path: String,
    dest: Option<String>,
) -> Result<Preflight, MigrateError> {
    blocking(move || {
        let (windows, name) = source_paths(&path)?;
        check_root(&windows)?;
        let migrations = app.state::<Migrations>();
        let running = Running::start(&migrations, &path)?;
        let distro = app.state::<Agent>().distro();
        let fs = win::NtfsFs::new(&windows);
        let shape = repo_shape(&fs);
        let git = git_runner(&path, &windows, distro.clone(), running.cancel.clone());
        let (walked, unreadable) = if shape.is_git_repo && !shape.is_linked_worktree {
            preflight_walk(
                &fs,
                git.as_ref(),
                threads(),
                &running.cancel,
                &emitter(&app, &path),
            )
            .map_err(snapshot_error)?
        } else {
            (Walk::default(), Vec::new())
        };
        let package_json = fs.read_text("package.json", 1 << 20).ok();
        let yarn_lock = fs.read_text("yarn.lock", 4096).ok();
        let steps = install_steps(&walked, package_json.as_deref(), yarn_lock.as_deref());
        let mut linux = linux_facts(
            distro.as_deref(),
            dest.as_deref(),
            &path,
            &install_tools(&steps),
        );
        let dest = match dest {
            Some(dest) => Some(dest),
            None => {
                linux.dest_exists = false;
                linux.home.as_deref().and_then(|home| {
                    default_destination(&default_roots(home), &name, |p| {
                        exists_in_wsl(distro.as_deref(), p)
                    })
                })
            }
        };
        let facts = preflight_facts(&shape, true, &walked, &unreadable, &linux, dest.as_deref());
        let summary = summarize(&facts, &walked, steps, &linux);
        let cloud_synced = cloud_synced(&windows);
        Ok(Preflight {
            source: path,
            windows_path: windows,
            vscode_windows: win::vscode_windows(&name),
            name,
            dest,
            summary,
            linux_git: git.linux(),
            cloud_synced,
        })
    })
    .await
}

/// A new snapshot file's path in `dir`, and the temporary name it is written under.
fn new_snapshot_path(dir: &Path) -> (PathBuf, PathBuf) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let file = dir.join(format!("snapshot-{}-{stamp}.json", std::process::id()));
    let tmp = file.with_extension("json.tmp");
    (file, tmp)
}

/// Writes `snapshot` to `tmp` and renames it to `file`.
fn write_snapshot(file: &Path, tmp: &Path, snapshot: &SourceSnapshot) -> Result<(), MigrateError> {
    let io = |err: std::io::Error| {
        MigrateError::new(
            MigrateErrorKind::Io,
            format!("Cannot write the snapshot file: {err}"),
        )
    };
    let bytes = serde_json::to_vec(snapshot).map_err(|err| io(std::io::Error::other(err)))?;
    std::fs::write(tmp, bytes).map_err(io)?;
    std::fs::rename(tmp, file).map_err(|err| {
        let _ = std::fs::remove_file(tmp);
        io(err)
    })
}

fn remove_snapshot_file(file: &Path) {
    if let Err(err) = std::fs::remove_file(file) {
        if err.kind() != std::io::ErrorKind::NotFound {
            eprintln!("refract: cannot delete {}: {err}", file.display());
        }
    }
}

/// Deletes the files in `dir` that `keep` does not hold.
fn sweep_snapshots(dir: &Path, keep: &HashSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !keep.contains(&path) {
            remove_snapshot_file(&path);
        }
    }
}

fn snapshot_dir(app: &AppHandle) -> Result<PathBuf, MigrateError> {
    app.path()
        .app_local_data_dir()
        .map(|dir| dir.join("migrate"))
        .map_err(|err| {
            MigrateError::new(
                MigrateErrorKind::Io,
                format!("Cannot find the app's data folder: {err}"),
            )
        })
}

/// Takes the snapshot and starts the migration (see the module docs).
#[tauri::command]
pub async fn migrate_start(
    app: AppHandle,
    path: String,
    dest: String,
) -> Result<MigrationTicket, MigrateError> {
    blocking(move || {
        let (windows, name) = source_paths(&path)?;
        check_root(&windows)?;
        let migrations = app.state::<Migrations>();
        if lock(&migrations.inner)
            .by_id
            .values()
            .any(|m| m.source == path)
        {
            return Err(MigrateError::new(
                MigrateErrorKind::Busy,
                "This folder is already being moved.",
            ));
        }
        if !win::vscode_windows(&name).is_empty() {
            return Err(MigrateError::new(
                MigrateErrorKind::Busy,
                "Close the VS Code windows that have this folder open first.",
            ));
        }
        let running = Running::start(&migrations, &path)?;
        let agent = app.state::<Agent>();
        let git = git_runner(&path, &windows, agent.distro(), running.cancel.clone());
        let fs = win::NtfsFs::new(&windows);
        let snapshot = take_snapshot(
            &fs,
            git.as_ref(),
            threads(),
            &running.cancel,
            &emitter(&app, &path),
        )
        .map_err(snapshot_error)?;

        let dir = snapshot_dir(&app)?;
        std::fs::create_dir_all(&dir).map_err(|err| {
            MigrateError::new(
                MigrateErrorKind::Io,
                format!("Cannot create the snapshot folder: {err}"),
            )
        })?;
        // The sweep and this file's registration happen under one lock, so no other
        // start sweeps it away before it is written.
        let (file, tmp) = {
            let _sweeping = lock(&SWEEP);
            let mut keep = {
                let inner = lock(&migrations.inner);
                let mut keep = inner.files.clone();
                keep.extend(inner.by_id.values().filter_map(|m| m.snapshot_file.clone()));
                keep
            };
            keep.extend(keep.clone().iter().map(|f| f.with_extension("json.tmp")));
            sweep_snapshots(&dir, &keep);
            let (file, tmp) = new_snapshot_path(&dir);
            lock(&migrations.inner).files.insert(file.clone());
            (file, tmp)
        };
        let forget_file = || {
            lock(&migrations.inner).files.remove(&file);
            remove_snapshot_file(&file);
        };
        if let Err(err) = write_snapshot(&file, &tmp, &snapshot) {
            forget_file();
            return Err(err);
        }
        let Some(linux_file) = wsl_path(&file.to_string_lossy()) else {
            forget_file();
            return Err(MigrateError::new(
                MigrateErrorKind::Io,
                format!("{} has no Linux path.", file.display()),
            ));
        };
        let params = MigrateParams {
            path: path.clone(),
            dest: dest.clone(),
            snapshot: linux_file,
        };
        let id = match AgentHelper(&agent).start(params) {
            Ok(id) => id,
            Err(err) => {
                forget_file();
                return Err(MigrateError::start_failed(err));
            }
        };
        let totals = snapshot.manifest.totals();
        let skipped = &snapshot.manifest.skipped;
        let mut inner = lock(&migrations.inner);
        inner.files.remove(&file);
        // A `migrateReady` may already have arrived and created the entry.
        let migration = inner.by_id.entry(id.clone()).or_insert(Migration {
            source: path,
            dest,
            snapshot_file: None,
            files: 0,
            bytes: 0,
            phase: Phase::Running,
        });
        migration.snapshot_file = Some(file);
        migration.files = totals.files + skipped.iter().map(|s| s.files).sum::<u64>();
        migration.bytes = totals.bytes + skipped.iter().map(|s| s.bytes).sum::<u64>();
        Ok(MigrationTicket { id })
    })
    .await
}

/// Stops the preflight or snapshot running for `path`; `false` when none runs.
#[tauri::command]
pub fn migrate_cancel_snapshot(migrations: State<'_, Migrations>, path: String) -> bool {
    match lock(&migrations.inner).running.get(&path) {
        Some(cancel) => {
            cancel.store(true, Ordering::SeqCst);
            true
        }
        None => false,
    }
}

/// Cancels a migration before its report: the helper deletes the partial copy. Only
/// while the copy runs; a ready one is answered with Keep both or Discard.
#[tauri::command]
pub async fn migrate_cancel(app: AppHandle, id: String) -> Result<(), MigrateError> {
    blocking(move || {
        let migrations = app.state::<Migrations>();
        let _busy = migrations.claim(&id)?;
        if migrations.get(&id)?.phase != Phase::Running {
            return Err(MigrateError::new(
                MigrateErrorKind::NotReady,
                "The copy is no longer running; answer its report instead.",
            ));
        }
        let agent = app.state::<Agent>();
        AgentHelper(&agent)
            .cancel(&id)
            .map_err(|err| MigrateError::agent("Cannot cancel the move", err))?;
        migrations.forget(&id);
        Ok(())
    })
    .await
}

/// The VS Code windows that have the folder at `path` open.
#[tauri::command]
pub async fn migrate_windows(path: String) -> Result<Vec<VsCodeWindow>, MigrateError> {
    blocking(move || {
        let (_, name) = source_paths(&path)?;
        Ok(win::vscode_windows(&name))
    })
    .await
}

/// Asks VS Code window `id` to close, if it still has the folder at `path` open. Its
/// unsaved-file prompt still shows.
#[tauri::command]
pub async fn migrate_close_window(path: String, id: u64) -> Result<bool, MigrateError> {
    blocking(move || {
        let (_, name) = source_paths(&path)?;
        win::close_window(id, &name)
            .map_err(|message| MigrateError::new(MigrateErrorKind::Io, message))
    })
    .await
}

/// Stops an agent on the repo that is finished or waiting on the user.
#[tauri::command]
pub async fn migrate_stop_agent(
    app: AppHandle,
    path: String,
    session_id: String,
) -> Result<(), MigrateError> {
    blocking(move || {
        AgentHelper(&app.state::<Agent>())
            .stop_agent(&path, &session_id)
            .map_err(|err| MigrateError::agent("Cannot stop the agent", err))
    })
    .await
}

/// Whether the original folder of migration `id` is still there (when unsure, true).
fn source_present(migration: &Migration) -> Result<bool, MigrateError> {
    let (windows, _) = source_paths(&migration.source)?;
    Ok(win::Os.exists(Path::new(&windows)))
}

fn deleting_refused() -> MigrateError {
    MigrateError::new(
        MigrateErrorKind::NotReady,
        "The original is being deleted; finish deleting it.",
    )
}

fn source_gone_refused() -> MigrateError {
    MigrateError::new(
        MigrateErrorKind::NotReady,
        "The original folder is gone; finish the move instead.",
    )
}

/// Re-copies what changed since the report; progress and a new report follow. The
/// migration counts as running from before the request, and goes back to its report
/// if the helper refuses.
#[tauri::command]
pub async fn migrate_resync(app: AppHandle, id: String) -> Result<(), MigrateError> {
    blocking(move || {
        let migrations = app.state::<Migrations>();
        let _busy = migrations.claim(&id)?;
        let migration = migrations.get(&id)?;
        let before = migration.phase;
        match before {
            Phase::Ready { .. } => {}
            Phase::Deleting => return Err(deleting_refused()),
            Phase::Running => {
                return Err(MigrateError::new(
                    MigrateErrorKind::NotReady,
                    "The move has no report to re-sync yet.",
                ))
            }
        }
        if !source_present(&migration)? {
            return Err(source_gone_refused());
        }
        migrations.set_phase(&id, Phase::Running);
        if let Err(err) = AgentHelper(&app.state::<Agent>()).resync(&id) {
            migrations.set_phase(&id, before);
            return Err(MigrateError::agent("Cannot re-sync", err));
        }
        Ok(())
    })
    .await
}

/// **Delete original** (see the module docs).
#[tauri::command]
pub async fn migrate_delete(
    app: AppHandle,
    id: String,
    permanent: bool,
) -> Result<DeleteOutcome, MigrateError> {
    blocking(move || {
        let _one_at_a_time = lock(&DELETES);
        let migrations = app.state::<Migrations>();
        let _busy = migrations.claim(&id)?;
        let migration = migrations.get(&id)?;
        let deleting = match migration.phase {
            Phase::Ready { can_delete: true } => false,
            Phase::Deleting => true,
            Phase::Ready { can_delete: false } => {
                return Err(MigrateError::new(
                    MigrateErrorKind::NotReady,
                    "A check failed, so the original can't be deleted.",
                ))
            }
            Phase::Running => {
                return Err(MigrateError::new(
                    MigrateErrorKind::NotReady,
                    "The move hasn't finished its checks yet.",
                ))
            }
        };
        let (windows, name) = source_paths(&migration.source)?;
        check_root(&windows)?;
        let agent = app.state::<Agent>();
        let distro = agent.distro();
        let target = DeleteTarget {
            id: id.clone(),
            source: PathBuf::from(windows),
            name,
            dest: windows_folder(&migration.dest, distro.as_deref()).ok(),
            files: migration.files,
            bytes: migration.bytes,
            deleting,
        };
        let outcome = delete_original(&AgentHelper(&agent), &win::Os, &target, permanent)?;
        if matches!(outcome, DeleteOutcome::Deleted { .. }) {
            migrations.forget(&id);
        } else if outcome.deleting() {
            migrations.set_phase(&id, Phase::Deleting);
        }
        Ok(outcome)
    })
    .await
}

/// **Keep both** (`keep`) or **Discard the copy** (`discard`). Neither once the delete
/// has begun; discarding the copy also not once the original is gone.
#[tauri::command]
pub async fn migrate_finish(
    app: AppHandle,
    id: String,
    action: FinishAction,
) -> Result<(), MigrateError> {
    blocking(move || {
        if action == FinishAction::Delete {
            return Err(MigrateError::new(
                MigrateErrorKind::Invalid,
                "Deleting the original goes through migrate_delete.",
            ));
        }
        let migrations = app.state::<Migrations>();
        let _busy = migrations.claim(&id)?;
        let migration = migrations.get(&id)?;
        match migration.phase {
            Phase::Ready { .. } => {}
            Phase::Deleting => return Err(deleting_refused()),
            Phase::Running => {
                return Err(MigrateError::new(
                    MigrateErrorKind::NotReady,
                    "The move hasn't finished its checks yet; cancel it instead.",
                ))
            }
        }
        if action == FinishAction::Discard && !source_present(&migration)? {
            return Err(source_gone_refused());
        }
        AgentHelper(&app.state::<Agent>())
            .finish(&id, action)
            .map_err(|err| MigrateError::agent("Cannot finish the move", err))?;
        migrations.forget(&id);
        Ok(())
    })
    .await
}

/// Whether the original folder of migration `id` is still there; `false` once it is
/// gone, deleted by hand or by an earlier **Delete original**.
#[tauri::command]
pub async fn migrate_source_exists(app: AppHandle, id: String) -> Result<bool, MigrateError> {
    blocking(move || source_present(&app.state::<Migrations>().get(&id)?)).await
}

/// Reports still waiting for an answer, from an earlier run of the app or helper.
#[tauri::command]
pub async fn migrate_pending(app: AppHandle) -> Result<Vec<PendingMigration>, MigrateError> {
    blocking(move || {
        let pending = AgentHelper(&app.state::<Agent>())
            .pending()
            .map_err(|err| MigrateError::agent("Cannot list pending moves", err))?;
        let migrations = app.state::<Migrations>();
        for p in &pending {
            migrations.ready(&p.id, &p.report, p.deleting);
        }
        Ok(pending)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use refract_core::migrate::build::{BuildFolder, SkippedFolder};
    use refract_core::migrate::manifest::{RelPath, Totals};
    use refract_core::migrate::report::{Check, CheckResult, REPORT_VERSION};

    #[test]
    fn sources_are_folders_on_a_windows_drive() {
        assert_eq!(
            source_paths("/mnt/c/Users/dev/Desktop/aurora-notes").unwrap(),
            (
                r"C:\Users\dev\Desktop\aurora-notes".to_owned(),
                "aurora-notes".to_owned()
            )
        );
        assert_eq!(
            source_paths("/mnt/d/aurora-notes/").unwrap().1,
            "aurora-notes"
        );
        let refused = source_paths("/home/dev/projects/aurora-notes").unwrap_err();
        assert_eq!(refused.kind, MigrateErrorKind::Refused);
        assert_eq!(
            source_paths("/mnt/c").unwrap_err().kind,
            MigrateErrorKind::Invalid
        );
        assert_eq!(
            source_paths("/mnt/c/../etc").unwrap_err().kind,
            MigrateErrorKind::Invalid
        );
        for odd in [
            "/mnt/c/dev/aurora-notes.",
            "/mnt/c/dev /aurora-notes",
            "/mnt/c/dev/notes /",
        ] {
            assert_eq!(
                source_paths(odd).unwrap_err().kind,
                MigrateErrorKind::Invalid,
                "{odd}"
            );
        }
    }

    fn report(id: &str, pass: bool) -> Report {
        let status = |check| {
            if pass {
                CheckResult::from_problems(check, vec![])
            } else {
                CheckResult::not_run(check)
            }
        };
        Report {
            version: REPORT_VERSION,
            id: id.into(),
            source: "/mnt/c/dev/aurora-notes".into(),
            dest: "/home/dev/projects/aurora-notes".into(),
            started_at: serde_json::from_value(serde_json::json!(NOW)).unwrap(),
            finished_at: serde_json::from_value(serde_json::json!(NOW)).unwrap(),
            checks: [
                Check::Files,
                Check::GitState,
                Check::NothingSkipped,
                Check::Unchanged,
                Check::History,
            ]
            .into_iter()
            .map(status)
            .collect(),
            skipped: vec![SkippedFolder {
                path: RelPath::new("node_modules").unwrap(),
                rule: BuildFolder::NodeModules,
                bytes: 900,
                files: 9,
            }],
            dropped_streams: vec![],
            new_objects: 0,
            source_totals: Totals {
                files: 10,
                dirs: 2,
                bytes: 100,
            },
            copy_totals: Totals::default(),
            install: vec![],
            install_changes: Default::default(),
            install_backup: None,
            history: Default::default(),
            linux_git: false,
            warnings: vec![],
        }
    }

    const NOW: &str = "2026-10-30T12:00:00Z";

    #[test]
    fn reports_mark_migrations_ready_with_their_full_size() {
        let migrations = Migrations::default();
        migrations.ready("m-1", &report("m-1", true), false);
        let m = migrations.get("m-1").unwrap();
        assert_eq!(m.phase, Phase::Ready { can_delete: true });
        assert_eq!((m.files, m.bytes), (19, 1000));
        assert_eq!(m.source, "/mnt/c/dev/aurora-notes");
        migrations.ready("m-2", &report("m-2", false), false);
        assert_eq!(
            migrations.get("m-2").unwrap().phase,
            Phase::Ready { can_delete: false }
        );
        // A delete that has begun stays begun whatever the helper says again, and the
        // pending list brings it back after a restart.
        migrations.set_phase("m-1", Phase::Deleting);
        migrations.ready("m-1", &report("m-1", true), false);
        assert_eq!(migrations.get("m-1").unwrap().phase, Phase::Deleting);
        migrations.ready("m-3", &report("m-3", true), true);
        assert_eq!(migrations.get("m-3").unwrap().phase, Phase::Deleting);
        migrations.forget("m-1");
        assert_eq!(
            migrations.get("m-1").unwrap_err().kind,
            MigrateErrorKind::NotReady
        );
    }

    #[test]
    fn a_stopped_helper_drops_running_copies_and_claims_are_exclusive() {
        let migrations = Migrations::default();
        migrations.ready("m-1", &report("m-1", true), false);
        lock(&migrations.inner).by_id.insert(
            "m-2".into(),
            Migration {
                source: "/mnt/c/dev/reef-notes".into(),
                dest: "/home/dev/projects/reef-notes".into(),
                snapshot_file: None,
                files: 0,
                bytes: 0,
                phase: Phase::Running,
            },
        );
        migrations.helper_stopped();
        assert!(migrations.get("m-1").is_ok());
        assert_eq!(
            migrations.get("m-2").unwrap_err().kind,
            MigrateErrorKind::NotReady
        );

        let claim = migrations.claim("m-1").unwrap();
        assert_eq!(
            migrations.claim("m-1").err().unwrap().kind,
            MigrateErrorKind::Busy
        );
        assert!(migrations.claim("m-3").is_ok());
        drop(claim);
        assert!(migrations.claim("m-1").is_ok());
    }

    #[test]
    fn snapshot_files_are_renamed_into_place_and_swept() {
        let dir = std::env::temp_dir().join(format!("refract-test-snap-{}", std::process::id()));
        let snapshot = SourceSnapshot {
            version: refract_core::migrate::SOURCE_SNAPSHOT_VERSION,
            manifest: Default::default(),
            git: vec![],
            linux_git: false,
        };
        std::fs::create_dir_all(&dir).unwrap();
        let write = || {
            let (file, tmp) = new_snapshot_path(&dir);
            write_snapshot(&file, &tmp, &snapshot).unwrap();
            assert!(!tmp.exists());
            file
        };
        let file = write();
        assert!(file.extension().is_some_and(|e| e == "json"));
        let back: SourceSnapshot = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(back, snapshot);
        let other = write();
        sweep_snapshots(&dir, &HashSet::from([file.clone()]));
        assert!(file.exists());
        assert!(!other.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End to end with the real `git.exe`, run by hand (`--ignored`): builds a small
    /// repo at `%TEMP%\refract-e2e\marigold-site`, takes its snapshot as **Start**
    /// does, and writes it to `%TEMP%\refract-e2e` for the WSL helper to migrate. Prints
    /// the repo's and the file's Linux paths. The repo is left for the helper; delete
    /// `%TEMP%\refract-e2e` afterwards.
    #[test]
    #[ignore]
    fn real_git_exe_snapshot_for_the_helper() {
        let base = std::env::temp_dir().join("refract-e2e");
        let repo = base.join("marigold-site");
        let _ = std::fs::remove_dir_all(&base);
        let files = [
            ("README.md", "# Marigold\r\n\r\nA small site.\r\n"),
            ("index.html", "<!doctype html>\n<h1>Marigold</h1>\n"),
            ("src/app.js", "console.log('marigold');\r\n"),
            ("src/mixed.txt", "one\r\ntwo\nthree\r\n"),
            (".gitignore", "node_modules/\n"),
            (
                "package.json",
                "{\"name\": \"marigold-site\", \"private\": true}\n",
            ),
            ("node_modules/left-pad/index.js", "module.exports = 1;\n"),
        ];
        for (path, text) in files {
            let file = repo.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, text).unwrap();
        }
        let path_var = std::env::var("PATH").ok();
        let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from);
        let exe = find_git_exe(path_var.as_deref(), program_files.as_deref(), Path::is_file)
            .expect("git.exe");
        let git = |args: &[&str]| {
            let out = std::process::Command::new(&exe)
                .args([
                    "-c",
                    "user.name=Test Dev",
                    "-c",
                    "user.email=dev@example.com",
                ])
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "First page"]);
        git(&["branch", "feature"]);
        git(&["update-index", "--skip-worktree", "index.html"]);
        std::fs::write(repo.join("notes.txt"), "draft\r\n").unwrap();
        std::fs::write(
            repo.join("README.md"),
            "# Marigold\r\n\r\nA small site, now with notes.\r\n",
        )
        .unwrap();

        let fs = win::NtfsFs::new(&repo.to_string_lossy());
        let runner = GitExe {
            exe,
            root: repo.clone(),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let snapshot = take_snapshot(&fs, &runner, 4, &AtomicBool::new(false), &|_| {}).unwrap();
        assert_eq!(snapshot.git[0].snapshot.index_flags, ["S index.html"]);
        assert!(snapshot.git[0].autocrlf.is_some());
        assert_eq!(snapshot.manifest.skipped[0].path.as_str(), "node_modules");
        let (file, tmp) = new_snapshot_path(&base);
        write_snapshot(&file, &tmp, &snapshot).unwrap();
        println!("autocrlf={}", snapshot.git[0].autocrlf.as_deref().unwrap());
        println!("source={}", wsl_path(&repo.to_string_lossy()).unwrap());
        println!("snapshot={}", wsl_path(&file.to_string_lossy()).unwrap());
    }

    #[test]
    fn snapshot_events_have_the_wire_shape() {
        let event = SnapshotEvent {
            path: "/mnt/c/dev/aurora-notes".into(),
            progress: SnapshotProgress {
                phase: snapshot::SnapshotPhase::Hashing,
                read: walk::ReadProgress {
                    files_done: 1,
                    files_total: 2,
                    bytes_done: 3,
                    bytes_total: 4,
                },
            },
        };
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            serde_json::json!({"path": "/mnt/c/dev/aurora-notes", "phase": "hashing",
                "filesDone": 1, "filesTotal": 2, "bytesDone": 3, "bytesTotal": 4})
        );
    }
}
