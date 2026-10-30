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
//!
//! `migrate_preflight` and `migrate_start` walk and read the whole source, so they
//! can take a while; both send `migrate://snapshot` events and stop on
//! `migrate_cancel_snapshot` (failing with `cancelled`). `migrate_start` takes the
//! snapshot again rather than trusting the preflight's, and refuses (`busy`) while a
//! VS Code window has the folder open or a migration of it runs. Agents that are busy
//! block **Start** in the dialog; the interface knows them from the repo's sessions.
//!
//! **Delete original** is `migrate_delete {id, permanent: false}`. Its outcome:
//! - `changed {changed, busyAgents}`: offer **Re-sync** (`migrate_resync`);
//! - `windowsOpen {windows}`: offer **Close** for each, then try again;
//! - `wontFit {reason}`: ask "Delete permanently?" (default **No**, which is
//!   `migrate_finish keep`), and on **Yes** call `migrate_delete {id, permanent: true}`;
//! - `deleted {permanent, shortcut?, shortcutError?}`: done; the gem moves to `dest`.
//!
//! An error from `migrate_delete` after the original was removed (the helper's
//! `migrateFinish` failed) leaves the migration marked removed, and calling it again
//! only finishes. **Keep both** is `migrate_finish keep` (the interface then hides the
//! old gem); **Discard the copy** is `migrate_finish discard`.
//!
//! # Events
//!
//! - `migrate://snapshot`: `{path, phase, filesDone, filesTotal, bytesDone,
//!   bytesTotal}` while a preflight or snapshot runs; `phase` is `git`, `walking`,
//!   `probing` (preflight), `hashing` or `blobs` (snapshot).
//! - The helper's `migrateProgress {id, step, done, total}` and `migrateReady {id,
//!   report}` reach the interface on `agent://event` like every helper event. The app
//!   also reads `migrateReady` to know which migrations may be deleted.
//!
//! A migration that becomes ready, or fails, while the window is hidden should show a
//! Windows notification. The app has no notification plugin and Tauri's tray has no
//! balloon, so for now [`notify`] only logs it.
//!
//! The snapshot file goes to `%LOCALAPPDATA%\<identifier>\migrate`, written under a
//! temporary name and renamed into place before `migrate` is sent, and is deleted when
//! the migration is cancelled, finished or fails to start. Files no migration of this
//! run uses are swept at each start, since the helper reads its file only once.

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

use refract_core::launch::wsl_path;
use refract_core::migrate::preflight::{default_destination, Refusal};
use refract_core::migrate::report::Report;
use refract_core::migrate::SourceSnapshot;
use refract_core::protocol::{Event, FinishAction, MigrateParams, MigrationTicket};
use refract_core::sessions::is_windows_mount;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::agent::{distro_args, wsl_command, Agent};
use crate::open::windows_folder;
use crate::process::output_with_deadline;
use flow::{delete_original, AgentHelper, DeleteTarget, MigrateHelper};
pub use flow::{DeleteOutcome, MigrateError, MigrateErrorKind, PendingMigration};
use git::{find_git_exe, GitExe, GitRun, WslGit};
use snapshot::{
    default_roots, parse_linux_facts, preflight_facts, preflight_walk, repo_shape, summarize,
    take_snapshot, LinuxFacts, PreflightSummary, SnapshotError, SnapshotProgress,
    LINUX_FACTS_SCRIPT,
};
pub use vscode::VsCodeWindow;
use walk::{SourceFs as _, Walk};

/// Emitted with each [`SnapshotEvent`].
pub const SNAPSHOT_EVENT: &str = "migrate://snapshot";

/// How long the Linux facts script gets. WSL may still be starting.
const LINUX_FACTS_TIMEOUT: Duration = Duration::from_secs(30);

/// Deletes run one at a time.
static DELETES: Mutex<()> = Mutex::new(());

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
    /// The original was removed but `migrateFinish delete` did not go through.
    Removed,
}

impl Migrations {
    /// Records a report from `migrateReady` or `migratePending`.
    fn ready(&self, id: &str, report: &Report) {
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
        if migration.phase != Phase::Removed {
            migration.phase = Phase::Ready {
                can_delete: report.can_delete(),
            };
        }
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

/// Called with every helper event before it is forwarded to the interface.
pub fn on_helper_event(app: &AppHandle, event: &Event) {
    let Event::MigrateReady { id, report } = event else {
        return;
    };
    if let Some(migrations) = app.try_state::<Migrations>() {
        migrations.ready(id, report);
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

/// Shows a Windows notification.
// TODO: show a real toast. The app has no notification plugin
// (tauri-plugin-notification would do) and Tauri's tray icon has no balloon.
fn notify(_app: &AppHandle, title: &str, body: &str) {
    eprintln!("refract: (notification) {title}: {body}");
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
        SnapshotError::Refused(refusals) => MigrateError::refused(refusals),
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

/// Runs [`LINUX_FACTS_SCRIPT`] in the helper's distro.
fn linux_facts(distro: Option<&str>, dest: Option<&str>) -> LinuxFacts {
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
        let mut linux = linux_facts(distro.as_deref(), dest.as_deref());
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
        let package_json = fs.read_text("package.json", 1 << 20).ok();
        let yarn_lock = fs.read_text("yarn.lock", 4096).ok();
        let summary = summarize(
            &facts,
            &walked,
            package_json.as_deref(),
            yarn_lock.as_deref(),
        );
        Ok(Preflight {
            source: path,
            windows_path: windows,
            vscode_windows: win::vscode_windows(&name),
            name,
            dest,
            summary,
            linux_git: git.linux(),
        })
    })
    .await
}

/// Writes `snapshot` under a temporary name in `dir`, renames it into place and
/// returns its path.
fn write_snapshot(dir: &Path, snapshot: &SourceSnapshot) -> Result<PathBuf, MigrateError> {
    let io = |err: std::io::Error| {
        MigrateError::new(
            MigrateErrorKind::Io,
            format!("Cannot write the snapshot file: {err}"),
        )
    };
    std::fs::create_dir_all(dir).map_err(io)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let file = dir.join(format!("snapshot-{}-{stamp}.json", std::process::id()));
    let tmp = file.with_extension("json.tmp");
    let bytes = serde_json::to_vec(snapshot).map_err(|err| io(std::io::Error::other(err)))?;
    std::fs::write(&tmp, bytes).map_err(io)?;
    std::fs::rename(&tmp, &file).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        io(err)
    })?;
    Ok(file)
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
        {
            let inner = lock(&migrations.inner);
            let mut keep = inner.files.clone();
            keep.extend(inner.by_id.values().filter_map(|m| m.snapshot_file.clone()));
            drop(inner);
            sweep_snapshots(&dir, &keep);
        }
        let file = write_snapshot(&dir, &snapshot)?;
        lock(&migrations.inner).files.insert(file.clone());
        let forget_file = || {
            lock(&migrations.inner).files.remove(&file);
            remove_snapshot_file(&file);
        };
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
                return Err(MigrateError::agent("Cannot start the move", err));
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

/// Cancels a migration before its report: the helper deletes the partial copy.
#[tauri::command]
pub async fn migrate_cancel(app: AppHandle, id: String) -> Result<(), MigrateError> {
    blocking(move || {
        let agent = app.state::<Agent>();
        AgentHelper(&agent)
            .cancel(&id)
            .map_err(|err| MigrateError::agent("Cannot cancel the move", err))?;
        app.state::<Migrations>().forget(&id);
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

/// Re-copies what changed since the report; progress and a new report follow.
#[tauri::command]
pub async fn migrate_resync(app: AppHandle, id: String) -> Result<(), MigrateError> {
    blocking(move || {
        let migrations = app.state::<Migrations>();
        if !matches!(migrations.get(&id)?.phase, Phase::Ready { .. }) {
            return Err(MigrateError::new(
                MigrateErrorKind::NotReady,
                "The move has no report to re-sync yet.",
            ));
        }
        AgentHelper(&app.state::<Agent>())
            .resync(&id)
            .map_err(|err| MigrateError::agent("Cannot re-sync", err))?;
        migrations.set_phase(&id, Phase::Running);
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
        let migration = migrations.get(&id)?;
        let removed = match migration.phase {
            Phase::Ready { can_delete: true } => false,
            Phase::Removed => true,
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
        let agent = app.state::<Agent>();
        let distro = agent.distro();
        let target = DeleteTarget {
            id: id.clone(),
            source: PathBuf::from(windows),
            name,
            dest: windows_folder(&migration.dest, distro.as_deref()).ok(),
            files: migration.files,
            bytes: migration.bytes,
            removed,
        };
        match delete_original(&AgentHelper(&agent), &win::Os, &target, permanent) {
            Ok(outcome) => {
                if matches!(outcome, DeleteOutcome::Deleted { .. }) {
                    migrations.forget(&id);
                }
                Ok(outcome)
            }
            Err(failure) => {
                if failure.removed {
                    migrations.set_phase(&id, Phase::Removed);
                }
                Err(failure.error)
            }
        }
    })
    .await
}

/// **Keep both** (`keep`) or **Discard the copy** (`discard`).
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
        match migrations.get(&id)?.phase {
            Phase::Ready { .. } => {}
            Phase::Removed => {
                return Err(MigrateError::new(
                    MigrateErrorKind::NotReady,
                    "The original is already deleted; finish with Delete original.",
                ))
            }
            Phase::Running => {
                return Err(MigrateError::new(
                    MigrateErrorKind::NotReady,
                    "The move hasn't finished its checks yet; cancel it instead.",
                ))
            }
        }
        AgentHelper(&app.state::<Agent>())
            .finish(&id, action)
            .map_err(|err| MigrateError::agent("Cannot finish the move", err))?;
        migrations.forget(&id);
        Ok(())
    })
    .await
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
            migrations.ready(&p.id, &p.report);
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
            history: Default::default(),
            linux_git: false,
            warnings: vec![],
        }
    }

    const NOW: &str = "2026-10-30T12:00:00Z";

    #[test]
    fn reports_mark_migrations_ready_with_their_full_size() {
        let migrations = Migrations::default();
        migrations.ready("m-1", &report("m-1", true));
        let m = migrations.get("m-1").unwrap();
        assert_eq!(m.phase, Phase::Ready { can_delete: true });
        assert_eq!((m.files, m.bytes), (19, 1000));
        assert_eq!(m.source, "/mnt/c/dev/aurora-notes");
        migrations.ready("m-2", &report("m-2", false));
        assert_eq!(
            migrations.get("m-2").unwrap().phase,
            Phase::Ready { can_delete: false }
        );
        // A removed original stays removed whatever the helper says again.
        migrations.set_phase("m-1", Phase::Removed);
        migrations.ready("m-1", &report("m-1", true));
        assert_eq!(migrations.get("m-1").unwrap().phase, Phase::Removed);
        migrations.forget("m-1");
        assert_eq!(
            migrations.get("m-1").unwrap_err().kind,
            MigrateErrorKind::NotReady
        );
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
        let file = write_snapshot(&dir, &snapshot).unwrap();
        assert!(file.extension().is_some_and(|e| e == "json"));
        let back: SourceSnapshot = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(back, snapshot);
        let other = write_snapshot(&dir, &snapshot).unwrap();
        sweep_snapshots(&dir, &HashSet::from([file.clone()]));
        assert!(file.exists());
        assert!(!other.exists());
        let _ = std::fs::remove_dir_all(&dir);
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
