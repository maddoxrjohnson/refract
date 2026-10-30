//! The helper requests a migration makes, and **Delete original** (spec §6), with the
//! helper and the Windows operations behind traits so the order of the steps is
//! tested with fakes.
//!
//! [`MigrateHelper`] is the thin request layer: one method per helper request, named
//! in [`method`], each a protocol [`Method`] that [`AgentHelper`] sends. A helper that
//! does not know a request answers `METHOD_NOT_FOUND`, which makes **Delete original**
//! refuse rather than skip its recheck. [`MigrateError::agent`] maps the helper's
//! error codes to [`MigrateErrorKind`]s.
//!
//! `migrateBeginDelete` rechecks the original and, when that passes, puts the migration
//! in the helper's *deleting* state in one step. From then on the helper refuses
//! `migrateResync`, `migrateFinish keep`/`discard` and `migrateCancel`: only
//! `migrateFinish delete` ends it, so a removal that stops halfway can only be
//! finished, never "re-synced" from a half-deleted original.

use std::path::{Path, PathBuf};
use std::time::Duration;

use refract_client::RequestError;
use refract_core::migrate::preflight::Refusal;
use refract_core::protocol::{
    code, FinishAction, Method, MigrateCancelParams, MigrateFinishParams, MigrateIdParams,
    MigrateParams, Reply, StopAgentParams,
};
pub use refract_core::protocol::{BeginDelete, PendingMigration, Recheck};
use serde::Serialize;

use super::snapshot::RefusalView;
use super::vscode::VsCodeWindow;
use crate::agent::Agent;

/// The helper's method names, in one place so they can follow the protocol.
pub mod method {
    pub const MIGRATE: &str = "migrate";
    pub const BEGIN_DELETE: &str = "migrateBeginDelete";
    pub const RESYNC: &str = "migrateResync";
    pub const FINISH: &str = "migrateFinish";
    pub const CANCEL: &str = "migrateCancel";
    pub const PENDING: &str = "migratePending";
    pub const STOP_AGENT: &str = "stopAgent";
}

/// How long `migrate` waits: the helper answers once it has read the snapshot file.
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// How long `migrateFinish` waits: `discard` deletes the whole copy.
const FINISH_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long `migrateCancel` waits: it deletes the partial copy.
const CANCEL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// How long `migrateBeginDelete` waits: it stats every file of the source over `/mnt`.
const BEGIN_DELETE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long the quick requests wait: `migrateResync` answers before it copies,
/// `migratePending` and `stopAgent` read state the helper holds.
const QUICK_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a migration command failed, sent as `{"kind": "...", "message": "..."}`, with
/// `refusals` for `refused`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateError {
    pub kind: MigrateErrorKind,
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub refusals: Vec<RefusalView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MigrateErrorKind {
    /// A path or argument was refused before anything ran.
    Invalid,
    /// The source can't be moved; `refusals` says why.
    Refused,
    /// A VS Code window has the folder open, or a migration of it is already running.
    Busy,
    /// The snapshot was cancelled.
    Cancelled,
    /// git failed while taking the snapshot.
    Git,
    /// The helper failed, is not running, did not answer or does not support the
    /// request yet.
    Agent,
    /// A file operation on Windows failed.
    Io,
    /// The migration is unknown or not at the step the command needs.
    NotReady,
    /// The command's task itself failed.
    Internal,
}

impl MigrateError {
    pub fn new(kind: MigrateErrorKind, message: impl Into<String>) -> MigrateError {
        MigrateError {
            kind,
            message: message.into(),
            refusals: Vec::new(),
        }
    }

    pub fn refused(refusals: Vec<Refusal>) -> MigrateError {
        MigrateError::refused_views(refusals.into_iter().map(RefusalView::from).collect())
    }

    pub fn refused_views(views: Vec<RefusalView>) -> MigrateError {
        let message = views
            .iter()
            .map(|r| r.message.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        MigrateError {
            kind: MigrateErrorKind::Refused,
            message,
            refusals: views,
        }
    }

    /// A failed helper request, made while `doing` something. The helper's own error
    /// codes keep their meaning: `MIGRATE_FAILED` (the migration is unknown or not at
    /// that step) is `notReady`, `AGENT_BUSY` (the agent is working) `busy`,
    /// `NOT_RUNNING` (no such agent process) `notReady` and `REPO_NOT_FOUND` `invalid`;
    /// everything else, and a helper that did not answer, is `agent`.
    pub fn agent(doing: &str, err: RequestError) -> MigrateError {
        let (kind, message) = match err {
            RequestError::Agent { code, message } => {
                let kind = match code {
                    code::MIGRATE_FAILED | code::NOT_RUNNING => MigrateErrorKind::NotReady,
                    code::AGENT_BUSY => MigrateErrorKind::Busy,
                    code::REPO_NOT_FOUND => MigrateErrorKind::Invalid,
                    _ => MigrateErrorKind::Agent,
                };
                (kind, message)
            }
            err => (MigrateErrorKind::Agent, err.to_string()),
        };
        MigrateError::new(kind, format!("{doing}: {message}"))
    }

    /// A failed `migrate`: `MIGRATE_FAILED` there is the helper's preflight refusing,
    /// one reason per line.
    pub fn start_failed(err: RequestError) -> MigrateError {
        match err {
            RequestError::Agent {
                code: code::MIGRATE_FAILED,
                message,
            } => MigrateError::new(
                MigrateErrorKind::Refused,
                format!(
                    "Cannot start the move: {}",
                    message.lines().collect::<Vec<_>>().join(" ")
                ),
            ),
            err => MigrateError::agent("Cannot start the move", err),
        }
    }
}

/// The helper's side of a migration (see the module docs).
pub trait MigrateHelper: Send + Sync {
    /// `migrate`: answers with the migration's id.
    fn start(&self, params: MigrateParams) -> Result<String, RequestError>;
    /// `migrateBeginDelete`: the recheck, and the deleting state when it passes.
    fn begin_delete(&self, id: &str) -> Result<BeginDelete, RequestError>;
    /// `migrateResync`: re-copies what changed; progress and a new `migrateReady` follow.
    fn resync(&self, id: &str) -> Result<(), RequestError>;
    /// `migrateFinish`.
    fn finish(&self, id: &str, action: FinishAction) -> Result<(), RequestError>;
    /// `migrateCancel`.
    fn cancel(&self, id: &str) -> Result<(), RequestError>;
    /// `migratePending`: reports still waiting, after a restart.
    fn pending(&self) -> Result<Vec<PendingMigration>, RequestError>;
    /// `stopAgent`: stops an agent that is finished or waiting on the user.
    fn stop_agent(&self, path: &str, session_id: &str) -> Result<(), RequestError>;
}

fn unexpected(method: &str, reply: &Reply) -> RequestError {
    RequestError::Agent {
        code: code::INTERNAL_ERROR,
        message: format!("the helper sent an unexpected answer to {method}: {reply:?}"),
    }
}

/// [`MigrateHelper`] over the running helper.
pub struct AgentHelper<'a>(pub &'a Agent);

impl MigrateHelper for AgentHelper<'_> {
    fn start(&self, params: MigrateParams) -> Result<String, RequestError> {
        match self.0.request(Method::Migrate(params), START_TIMEOUT)? {
            Reply::Migration(ticket) => Ok(ticket.id),
            other => Err(unexpected(method::MIGRATE, &other)),
        }
    }

    fn begin_delete(&self, id: &str) -> Result<BeginDelete, RequestError> {
        let params = MigrateIdParams { id: id.to_owned() };
        match self
            .0
            .request(Method::MigrateBeginDelete(params), BEGIN_DELETE_TIMEOUT)?
        {
            Reply::BeginDelete(begin) => Ok(begin),
            other => Err(unexpected(method::BEGIN_DELETE, &other)),
        }
    }

    fn resync(&self, id: &str) -> Result<(), RequestError> {
        let params = MigrateIdParams { id: id.to_owned() };
        match self
            .0
            .request(Method::MigrateResync(params), QUICK_TIMEOUT)?
        {
            Reply::Done => Ok(()),
            other => Err(unexpected(method::RESYNC, &other)),
        }
    }

    fn finish(&self, id: &str, action: FinishAction) -> Result<(), RequestError> {
        let params = MigrateFinishParams {
            id: id.to_owned(),
            action,
        };
        match self
            .0
            .request(Method::MigrateFinish(params), FINISH_TIMEOUT)?
        {
            Reply::Done => Ok(()),
            other => Err(unexpected(method::FINISH, &other)),
        }
    }

    fn cancel(&self, id: &str) -> Result<(), RequestError> {
        let params = MigrateCancelParams { id: id.to_owned() };
        match self
            .0
            .request(Method::MigrateCancel(params), CANCEL_TIMEOUT)?
        {
            Reply::Done => Ok(()),
            other => Err(unexpected(method::CANCEL, &other)),
        }
    }

    fn pending(&self) -> Result<Vec<PendingMigration>, RequestError> {
        match self.0.request(Method::MigratePending, QUICK_TIMEOUT)? {
            Reply::PendingMigrations(list) => Ok(list),
            other => Err(unexpected(method::PENDING, &other)),
        }
    }

    fn stop_agent(&self, path: &str, session_id: &str) -> Result<(), RequestError> {
        let params = StopAgentParams {
            path: path.to_owned(),
            session_id: session_id.to_owned(),
        };
        match self.0.request(Method::StopAgent(params), QUICK_TIMEOUT)? {
            Reply::Done => Ok(()),
            other => Err(unexpected(method::STOP_AGENT, &other)),
        }
    }
}

/// Whether the folder can go to the Recycle Bin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fit {
    Fits,
    /// Why not, for a person.
    WontFit(String),
}

/// What removing the folder did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removal {
    Done,
    /// The Recycle Bin would not take it, so nothing was deleted.
    WontFit(String),
}

/// The Windows operations **Delete original** needs.
pub trait WindowsOps {
    /// VS Code windows that have a folder of this name open.
    fn vscode_windows(&self, folder: &str) -> Vec<VsCodeWindow>;
    /// Whether `bytes` in `files` files at `path` fit in its drive's Recycle Bin.
    fn recycle_fit(&self, path: &Path, files: u64, bytes: u64) -> Fit;
    /// Sends `path` to the Recycle Bin, or deletes it for good when `recycle` is false.
    fn remove(&self, path: &Path, recycle: bool) -> Result<Removal, String>;
    /// Something is at `path` (when unsure, true).
    fn exists(&self, path: &Path) -> bool;
    /// Writes a `.lnk` at `lnk` pointing to `target`.
    fn create_shortcut(&self, lnk: &Path, target: &str) -> Result<(), String>;
}

/// How large a folder is in the Recycle Bin's terms: its bytes plus a 4 KiB cluster
/// for each file, the slack a file's last cluster adds on NTFS.
pub fn recycle_bytes(files: u64, bytes: u64) -> u64 {
    bytes.saturating_add(files.saturating_mul(4096))
}

/// Whether `bytes` fit a Recycle Bin, from the drive's settings: off (`nuke`), its
/// size limit (`max_bytes`, the per-drive `MaxCapacity`), or, when that is not set,
/// a conservative 5 % of the drive (`volume_bytes`; Windows' own default is larger).
/// The bin discards its oldest items to make room, so only the limit matters, not
/// what it holds now.
pub fn fits(bytes: u64, nuke: bool, max_bytes: Option<u64>, volume_bytes: Option<u64>) -> Fit {
    if nuke {
        return Fit::WontFit("The Recycle Bin is turned off for this drive.".into());
    }
    let (limit, what) = match (max_bytes, volume_bytes) {
        (Some(max), _) => (max, "the Recycle Bin's size limit"),
        (None, Some(volume)) => (volume / 20, "5 % of the drive"),
        (None, None) => {
            return Fit::WontFit("Refract can't tell how large this drive's Recycle Bin is.".into())
        }
    };
    if bytes <= limit {
        Fit::Fits
    } else {
        Fit::WontFit(format!(
            "The folder ({:.1} GB) is larger than {what} ({:.1} GB).",
            bytes as f64 / 1e9,
            limit as f64 / 1e9
        ))
    }
}

/// What **Delete original** did, sent as `{"kind": "...", ...}`. `deleting` says
/// whether the helper is in the deleting state, where the only way on is to finish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DeleteOutcome {
    /// The recheck did not pass: nothing more was deleted. Before the deleting state
    /// the interface offers **Re-sync** for changed files, **Keep both** for a changed
    /// copy and **Stop** for agents; in it, only **Finish deleting** once fixed.
    Changed {
        #[serde(flatten)]
        recheck: Recheck,
        deleting: bool,
    },
    /// VS Code windows still have the folder open: nothing was deleted. The interface
    /// offers **Close** for each.
    WindowsOpen {
        windows: Vec<VsCodeWindow>,
        deleting: bool,
    },
    /// The Recycle Bin can't take the folder: nothing was deleted. The interface asks
    /// "Delete permanently?" (default **No**, which is **Keep both**, or, in the
    /// deleting state, leaving it for later).
    WontFit { reason: String, deleting: bool },
    /// The original is still there on Windows, but the helper can't see it over `/mnt`
    /// (its drive isn't mounted in WSL), so it could not be checked: nothing was
    /// deleted. The interface asks to make the drive visible in WSL, then try again.
    DriveHidden { deleting: bool },
    /// The helper is in the deleting state but the original is not (all) gone, or the
    /// helper did not finish: the interface offers only **Finish deleting**.
    Unfinished { reason: String },
    /// The original is gone and the helper finished the migration.
    Deleted {
        /// Deleted for good rather than recycled.
        permanent: bool,
        /// The original folder was already gone (deleted by hand) before this.
        already_gone: bool,
        /// The `.lnk` left at the old location.
        #[serde(skip_serializing_if = "Option::is_none")]
        shortcut: Option<String>,
        /// Why no `.lnk` was made; the move itself succeeded.
        #[serde(skip_serializing_if = "Option::is_none")]
        shortcut_error: Option<String>,
    },
}

impl DeleteOutcome {
    /// The helper is left in the deleting state.
    pub fn deleting(&self) -> bool {
        match self {
            DeleteOutcome::Changed { deleting, .. }
            | DeleteOutcome::WindowsOpen { deleting, .. }
            | DeleteOutcome::WontFit { deleting, .. }
            | DeleteOutcome::DriveHidden { deleting } => *deleting,
            DeleteOutcome::Unfinished { .. } => true,
            DeleteOutcome::Deleted { .. } => false,
        }
    }
}

/// The migration **Delete original** acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteTarget {
    pub id: String,
    /// The original's Windows path.
    pub source: PathBuf,
    /// The original folder's name.
    pub name: String,
    /// The copy as Windows reaches it (`\\wsl.localhost\<distro>\…`); `None` when the
    /// distro is not known, which leaves out the shortcut.
    pub dest: Option<String>,
    /// Files and bytes of the original, skipped folders included.
    pub files: u64,
    pub bytes: u64,
    /// The helper is already in the deleting state: an earlier attempt began.
    pub deleting: bool,
}

/// The `.lnk` for a folder at `source`: `<name>.lnk` beside it.
pub fn shortcut_path(source: &Path, name: &str) -> Option<PathBuf> {
    Some(source.parent()?.join(format!("{name}.lnk")))
}

/// **Delete original** (spec §6). Before the deleting state: the VS Code windows and
/// the fit (unless `permanent`), each of which stops before anything changes, then
/// `migrateBeginDelete`. Once that passes, the removal (whatever is left of the
/// folder; skipped when it is already gone) and a check that it is gone, the shortcut,
/// then `migrateFinish delete`; a failure there is [`DeleteOutcome::Unfinished`], and
/// calling this again with `target.deleting` picks up where it stopped. Only a
/// failure before the helper took the deleting state is an error.
pub fn delete_original(
    helper: &dyn MigrateHelper,
    os: &dyn WindowsOps,
    target: &DeleteTarget,
    permanent: bool,
) -> Result<DeleteOutcome, MigrateError> {
    let deleting = target.deleting;
    let present = os.exists(&target.source);
    // A window or a full bin stops here before the deleting state, so **Keep both**
    // is still open.
    let checks = |deleting: bool| -> Option<DeleteOutcome> {
        let windows = os.vscode_windows(&target.name);
        if !windows.is_empty() {
            return Some(DeleteOutcome::WindowsOpen { windows, deleting });
        }
        if !permanent {
            if let Fit::WontFit(reason) = os.recycle_fit(&target.source, target.files, target.bytes)
            {
                return Some(DeleteOutcome::WontFit { reason, deleting });
            }
        }
        None
    };
    if present && !deleting {
        if let Some(outcome) = checks(false) {
            return Ok(outcome);
        }
    }
    let begin = helper
        .begin_delete(&target.id)
        .map_err(|err| MigrateError::agent("Cannot check the original again", err))?;
    // `sourceMissing` only says the `/mnt` path is absent. Still there on Windows, it is
    // on a drive WSL doesn't see, so the recheck saw nothing of it.
    if begin.recheck.source_missing && os.exists(&target.source) {
        return Ok(DeleteOutcome::DriveHidden {
            deleting: deleting || begin.ok,
        });
    }
    if !begin.ok {
        return Ok(DeleteOutcome::Changed {
            recheck: begin.recheck,
            deleting,
        });
    }

    // The helper now only lets this migration finish deleting.
    let unfinished = |reason: String| Ok(DeleteOutcome::Unfinished { reason });
    if os.exists(&target.source) {
        if deleting {
            if let Some(outcome) = checks(true) {
                return Ok(outcome);
            }
        }
        match os.remove(&target.source, !permanent) {
            Ok(Removal::Done) => {}
            Ok(Removal::WontFit(reason)) => {
                return Ok(DeleteOutcome::WontFit {
                    reason,
                    deleting: true,
                })
            }
            Err(err) => return unfinished(format!("Cannot delete the original: {err}")),
        }
        if os.exists(&target.source) {
            return unfinished(format!(
                "{} is still there after deleting it. Close any program using it and finish deleting.",
                target.source.display()
            ));
        }
    }

    let (mut shortcut, mut shortcut_error) = (None, None);
    match (&target.dest, shortcut_path(&target.source, &target.name)) {
        (Some(dest), Some(lnk)) => {
            // A retry finds the one the first attempt made.
            let made = if os.exists(&lnk) {
                Ok(())
            } else {
                os.create_shortcut(&lnk, dest)
            };
            match made {
                Ok(()) => shortcut = Some(lnk.display().to_string()),
                Err(err) => shortcut_error = Some(err),
            }
        }
        (None, _) => {
            shortcut_error = Some("Refract does not know which WSL distro the copy is in.".into())
        }
        (_, None) => shortcut_error = Some("The original had no parent folder.".into()),
    }

    if let Err(err) = helper.finish(&target.id, FinishAction::Delete) {
        let err = MigrateError::agent("The original is gone, but the helper could not finish", err);
        return unfinished(err.message);
    }
    Ok(DeleteOutcome::Deleted {
        permanent,
        already_gone: !present && !deleting,
        shortcut,
        shortcut_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::Mutex;

    /// What an older helper answers to a request it does not know.
    fn not_supported(method: &str) -> RequestError {
        RequestError::Agent {
            code: code::METHOD_NOT_FOUND,
            message: format!("unknown method {method}"),
        }
    }

    #[derive(Default)]
    struct FakeHelper {
        /// What `migrateBeginDelete` answers; `None` is an older helper.
        begin: Option<BeginDelete>,
        finish_fails: bool,
        log: Mutex<Vec<String>>,
    }

    impl MigrateHelper for FakeHelper {
        fn start(&self, params: MigrateParams) -> Result<String, RequestError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("start {}", params.path));
            Ok("m-1".into())
        }
        fn begin_delete(&self, id: &str) -> Result<BeginDelete, RequestError> {
            self.log.lock().unwrap().push(format!("begin {id}"));
            self.begin
                .clone()
                .ok_or_else(|| not_supported(method::BEGIN_DELETE))
        }
        fn resync(&self, _: &str) -> Result<(), RequestError> {
            Ok(())
        }
        fn finish(&self, id: &str, action: FinishAction) -> Result<(), RequestError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("finish {id} {action:?}"));
            if self.finish_fails {
                Err(RequestError::Timeout)
            } else {
                Ok(())
            }
        }
        fn cancel(&self, _: &str) -> Result<(), RequestError> {
            Ok(())
        }
        fn pending(&self) -> Result<Vec<PendingMigration>, RequestError> {
            Ok(Vec::new())
        }
        fn stop_agent(&self, _: &str, _: &str) -> Result<(), RequestError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeOs {
        windows: Vec<VsCodeWindow>,
        fit: Option<Fit>,
        removal: Option<Result<Removal, String>>,
        /// The folder survives removal.
        sticky: bool,
        /// Removal deletes part of the folder, then fails.
        partial: bool,
        existing: RefCell<Vec<PathBuf>>,
        log: RefCell<Vec<String>>,
    }

    impl FakeOs {
        fn with_source() -> FakeOs {
            let os = FakeOs::default();
            os.existing
                .borrow_mut()
                .push(PathBuf::from(r"C:\dev\aurora-notes"));
            os
        }
    }

    impl WindowsOps for FakeOs {
        fn vscode_windows(&self, folder: &str) -> Vec<VsCodeWindow> {
            self.log.borrow_mut().push(format!("windows {folder}"));
            self.windows.clone()
        }
        fn recycle_fit(&self, _: &Path, files: u64, bytes: u64) -> Fit {
            self.log.borrow_mut().push(format!("fit {files} {bytes}"));
            self.fit.clone().unwrap_or(Fit::Fits)
        }
        fn remove(&self, path: &Path, recycle: bool) -> Result<Removal, String> {
            self.log
                .borrow_mut()
                .push(format!("remove recycle={recycle}"));
            if self.partial {
                return Err("a file is in use".into());
            }
            let result = self.removal.clone().unwrap_or(Ok(Removal::Done));
            if matches!(result, Ok(Removal::Done)) && !self.sticky {
                self.existing.borrow_mut().retain(|p| p != path);
            }
            result
        }
        fn exists(&self, path: &Path) -> bool {
            self.existing.borrow().iter().any(|p| p == path)
        }
        fn create_shortcut(&self, lnk: &Path, target: &str) -> Result<(), String> {
            self.log
                .borrow_mut()
                .push(format!("shortcut {} -> {target}", lnk.display()));
            self.existing.borrow_mut().push(lnk.to_owned());
            Ok(())
        }
    }

    fn target() -> DeleteTarget {
        DeleteTarget {
            id: "m-1".into(),
            source: PathBuf::from(r"C:\dev\aurora-notes"),
            name: "aurora-notes".into(),
            dest: Some(r"\\wsl.localhost\Ubuntu\home\dev\projects\aurora-notes".into()),
            files: 3,
            bytes: 100,
            deleting: false,
        }
    }

    fn safe_helper() -> FakeHelper {
        FakeHelper {
            begin: Some(BeginDelete {
                ok: true,
                recheck: Recheck::default(),
            }),
            ..FakeHelper::default()
        }
    }

    fn lnk() -> String {
        PathBuf::from(r"C:\dev")
            .join("aurora-notes.lnk")
            .display()
            .to_string()
    }

    #[test]
    fn deletes_in_order_and_leaves_a_shortcut() {
        let helper = safe_helper();
        let os = FakeOs::with_source();
        let outcome = delete_original(&helper, &os, &target(), false).unwrap();
        assert_eq!(
            outcome,
            DeleteOutcome::Deleted {
                permanent: false,
                already_gone: false,
                shortcut: Some(lnk()),
                shortcut_error: None,
            }
        );
        assert_eq!(
            *os.log.borrow(),
            [
                "windows aurora-notes".to_owned(),
                "fit 3 100".into(),
                "remove recycle=true".into(),
                format!(
                    "shortcut {} -> \\\\wsl.localhost\\Ubuntu\\home\\dev\\projects\\aurora-notes",
                    lnk()
                ),
            ]
        );
        assert_eq!(
            *helper.log.lock().unwrap(),
            ["begin m-1", "finish m-1 Delete"]
        );
    }

    #[test]
    fn a_failed_recheck_stops_before_anything_is_touched() {
        let helper = FakeHelper {
            begin: Some(BeginDelete {
                ok: false,
                recheck: Recheck {
                    changed: vec!["notes.txt".into()],
                    busy_agents: vec!["claude (pid 7) is working in aurora-notes.".into()],
                    ..Recheck::default()
                },
            }),
            ..FakeHelper::default()
        };
        let os = FakeOs::with_source();
        let outcome = delete_original(&helper, &os, &target(), false).unwrap();
        assert!(
            matches!(&outcome, DeleteOutcome::Changed { recheck, deleting: false }
                if recheck.changed == ["notes.txt"] && recheck.busy_agents.len() == 1)
        );
        assert!(!os.log.borrow().iter().any(|l| l.starts_with("remove")));
        assert!(os.exists(Path::new(r"C:\dev\aurora-notes")));
    }

    #[test]
    fn a_helper_without_begin_delete_refuses_to_delete() {
        let os = FakeOs::with_source();
        let err = delete_original(&FakeHelper::default(), &os, &target(), false).unwrap_err();
        assert_eq!(err.kind, MigrateErrorKind::Agent);
        assert!(err.message.contains("migrateBeginDelete"));
        assert!(!os.log.borrow().iter().any(|l| l.starts_with("remove")));
    }

    #[test]
    fn open_windows_and_a_full_bin_stop_before_the_deleting_state() {
        let os = FakeOs {
            windows: vec![VsCodeWindow {
                id: 7,
                title: "aurora-notes - Visual Studio Code".into(),
            }],
            ..FakeOs::with_source()
        };
        let helper = safe_helper();
        let outcome = delete_original(&helper, &os, &target(), false).unwrap();
        assert!(
            matches!(outcome, DeleteOutcome::WindowsOpen { ref windows, deleting: false } if windows[0].id == 7)
        );
        assert_eq!(*os.log.borrow(), ["windows aurora-notes"]);
        let os = FakeOs {
            fit: Some(Fit::WontFit("too large".into())),
            ..FakeOs::with_source()
        };
        assert_eq!(
            delete_original(&helper, &os, &target(), false).unwrap(),
            DeleteOutcome::WontFit {
                reason: "too large".into(),
                deleting: false,
            }
        );
        assert!(os.exists(Path::new(r"C:\dev\aurora-notes")));
        // Neither began the delete.
        assert!(helper.log.lock().unwrap().is_empty());
    }

    #[test]
    fn a_folder_too_large_for_the_bin_is_kept_unless_permanent() {
        let helper = safe_helper();
        // The guard in the removal itself: nothing deleted, but the delete has begun.
        let os = FakeOs {
            removal: Some(Ok(Removal::WontFit("would be deleted for good".into()))),
            ..FakeOs::with_source()
        };
        assert!(matches!(
            delete_original(&helper, &os, &target(), false).unwrap(),
            DeleteOutcome::WontFit { deleting: true, .. }
        ));
        assert!(os.exists(Path::new(r"C:\dev\aurora-notes")));
        // Asked to delete for good, no fit check and no recycling.
        let os = FakeOs {
            fit: Some(Fit::WontFit("too large".into())),
            ..FakeOs::with_source()
        };
        let outcome = delete_original(&helper, &os, &target(), true).unwrap();
        assert!(matches!(
            outcome,
            DeleteOutcome::Deleted {
                permanent: true,
                ..
            }
        ));
        assert!(os.log.borrow().contains(&"remove recycle=false".to_owned()));
        assert!(!os.log.borrow().iter().any(|l| l.starts_with("fit")));
    }

    #[test]
    fn a_partial_removal_is_unfinished_and_is_finished_later() {
        let helper = safe_helper();
        let os = FakeOs {
            partial: true,
            ..FakeOs::with_source()
        };
        let outcome = delete_original(&helper, &os, &target(), false).unwrap();
        assert!(
            matches!(&outcome, DeleteOutcome::Unfinished { reason } if reason.contains("in use"))
        );
        assert!(outcome.deleting());
        assert!(!helper
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("finish")));

        // Finishing begins again (the helper allows it in the deleting state), checks
        // the windows and the bin, and removes what is left.
        let os = FakeOs::with_source();
        let helper = safe_helper();
        let resume = DeleteTarget {
            deleting: true,
            ..target()
        };
        let outcome = delete_original(&helper, &os, &resume, false).unwrap();
        assert!(matches!(
            outcome,
            DeleteOutcome::Deleted {
                already_gone: false,
                ..
            }
        ));
        assert_eq!(
            os.log.borrow()[..3],
            [
                "windows aurora-notes".to_owned(),
                "fit 3 100".into(),
                "remove recycle=true".into()
            ]
        );
        assert_eq!(
            *helper.log.lock().unwrap(),
            ["begin m-1", "finish m-1 Delete"]
        );

        // A window opened meanwhile still blocks, and says the delete has begun.
        let os = FakeOs {
            windows: vec![VsCodeWindow {
                id: 3,
                title: "aurora-notes - Visual Studio Code".into(),
            }],
            ..FakeOs::with_source()
        };
        assert!(matches!(
            delete_original(&safe_helper(), &os, &resume, false).unwrap(),
            DeleteOutcome::WindowsOpen { deleting: true, .. }
        ));
    }

    #[test]
    fn a_failed_recheck_while_deleting_stays_deleting() {
        let helper = FakeHelper {
            begin: Some(BeginDelete {
                ok: false,
                recheck: Recheck {
                    changed: vec!["notes.txt".into()],
                    ..Recheck::default()
                },
            }),
            ..FakeHelper::default()
        };
        let os = FakeOs::with_source();
        let resume = DeleteTarget {
            deleting: true,
            ..target()
        };
        let outcome = delete_original(&helper, &os, &resume, false).unwrap();
        assert!(matches!(
            outcome,
            DeleteOutcome::Changed { deleting: true, .. }
        ));
        assert!(os.log.borrow().is_empty());
    }

    #[test]
    fn a_source_wsl_cannot_see_is_not_deleted() {
        let helper = FakeHelper {
            begin: Some(BeginDelete {
                ok: true,
                recheck: Recheck {
                    source_missing: true,
                    ..Recheck::default()
                },
            }),
            ..FakeHelper::default()
        };
        let os = FakeOs::with_source();
        let outcome = delete_original(&helper, &os, &target(), false).unwrap();
        assert_eq!(outcome, DeleteOutcome::DriveHidden { deleting: true });
        assert!(!os.log.borrow().iter().any(|l| l.starts_with("remove")));
        assert!(!helper
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("finish")));
        // Gone on Windows too: the move only needs finishing.
        let os = FakeOs::default();
        assert!(matches!(
            delete_original(&helper, &os, &target(), false).unwrap(),
            DeleteOutcome::Deleted {
                already_gone: true,
                ..
            }
        ));
    }

    #[test]
    fn a_folder_that_survives_is_unfinished_and_finish_is_not_sent() {
        let helper = safe_helper();
        let os = FakeOs {
            sticky: true,
            ..FakeOs::with_source()
        };
        let outcome = delete_original(&helper, &os, &target(), false).unwrap();
        assert!(matches!(outcome, DeleteOutcome::Unfinished { .. }));
        assert!(!helper
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("finish")));
    }

    #[test]
    fn a_failed_finish_is_retried_without_deleting_again() {
        let helper = FakeHelper {
            finish_fails: true,
            ..safe_helper()
        };
        let os = FakeOs::with_source();
        let outcome = delete_original(&helper, &os, &target(), false).unwrap();
        assert!(
            matches!(&outcome, DeleteOutcome::Unfinished { reason } if reason.contains("could not finish"))
        );

        let helper = safe_helper();
        let retry = DeleteTarget {
            deleting: true,
            ..target()
        };
        os.log.borrow_mut().clear();
        let outcome = delete_original(&helper, &os, &retry, false).unwrap();
        assert!(matches!(
            outcome,
            DeleteOutcome::Deleted {
                shortcut: Some(_),
                already_gone: false,
                ..
            }
        ));
        // No removal, and the shortcut from the first attempt is kept.
        assert!(os.log.borrow().is_empty());
        assert_eq!(
            *helper.log.lock().unwrap(),
            ["begin m-1", "finish m-1 Delete"]
        );
    }

    #[test]
    fn an_original_deleted_by_hand_is_finished_without_removing() {
        let helper = safe_helper();
        let os = FakeOs::default();
        let outcome = delete_original(&helper, &os, &target(), false).unwrap();
        assert!(matches!(
            outcome,
            DeleteOutcome::Deleted {
                already_gone: true,
                shortcut: Some(_),
                ..
            }
        ));
        assert!(!os
            .log
            .borrow()
            .iter()
            .any(|l| l.starts_with("remove") || l.starts_with("windows")));
        assert_eq!(
            *helper.log.lock().unwrap(),
            ["begin m-1", "finish m-1 Delete"]
        );
    }

    #[test]
    fn an_unknown_distro_only_loses_the_shortcut() {
        let os = FakeOs::with_source();
        let outcome = delete_original(
            &safe_helper(),
            &os,
            &DeleteTarget {
                dest: None,
                ..target()
            },
            false,
        )
        .unwrap();
        assert!(matches!(
            outcome,
            DeleteOutcome::Deleted {
                shortcut: None,
                shortcut_error: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn the_bin_fits_by_its_limit_or_five_percent_of_the_drive() {
        assert_eq!(recycle_bytes(10, 1000), 1000 + 40_960);
        assert_eq!(fits(10, false, Some(10), None), Fit::Fits);
        assert!(matches!(
            fits(11, false, Some(10), Some(u64::MAX)),
            Fit::WontFit(_)
        ));
        assert_eq!(fits(5, false, None, Some(100)), Fit::Fits);
        assert!(matches!(fits(6, false, None, Some(100)), Fit::WontFit(_)));
        assert!(
            matches!(fits(1, true, Some(100), None), Fit::WontFit(r) if r.contains("turned off"))
        );
        assert!(matches!(fits(1, false, None, None), Fit::WontFit(_)));
    }

    #[test]
    fn outcomes_and_errors_have_the_wire_shape() {
        let value = serde_json::to_value(DeleteOutcome::Changed {
            recheck: Recheck {
                changed: vec!["a".into()],
                ..Recheck::default()
            },
            deleting: false,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"kind": "changed", "changed": ["a"], "removed": [],
                "unreadable": [], "copyChanged": [], "busyAgents": [], "sourceMissing": false,
                "deleting": false})
        );
        let value = serde_json::to_value(DeleteOutcome::Deleted {
            permanent: false,
            already_gone: false,
            shortcut: None,
            shortcut_error: Some("x".into()),
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"kind": "deleted", "permanent": false, "alreadyGone": false,
                "shortcutError": "x"})
        );
        let value = serde_json::to_value(DeleteOutcome::Unfinished { reason: "x".into() }).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"kind": "unfinished", "reason": "x"})
        );
        // What the helper sends: the newer lists read as empty when missing.
        let begin: BeginDelete = serde_json::from_value(serde_json::json!(
            {"ok": false, "recheck": {"changed": ["a"], "busyAgents": [], "sourceMissing": true}}
        ))
        .unwrap();
        assert!(!begin.ok && begin.recheck.source_missing && begin.recheck.removed.is_empty());
        let err = MigrateError::refused(vec![Refusal::LinkedWorktree]);
        let value = serde_json::to_value(&err).unwrap();
        assert_eq!(value["kind"], "refused");
        assert_eq!(value["refusals"][0]["kind"], "linkedWorktree");
        assert!(value["refusals"][0]["message"]
            .as_str()
            .unwrap()
            .contains("worktree"));
        let plain = serde_json::to_value(MigrateError::new(MigrateErrorKind::Busy, "x")).unwrap();
        assert_eq!(plain, serde_json::json!({"kind": "busy", "message": "x"}));
    }

    #[test]
    fn helper_error_codes_keep_their_meaning() {
        let agent = |code| RequestError::Agent {
            code,
            message: "why".into(),
        };
        let kind = |code| MigrateError::agent("Doing", agent(code)).kind;
        assert_eq!(kind(code::MIGRATE_FAILED), MigrateErrorKind::NotReady);
        assert_eq!(kind(code::AGENT_BUSY), MigrateErrorKind::Busy);
        assert_eq!(kind(code::NOT_RUNNING), MigrateErrorKind::NotReady);
        assert_eq!(kind(code::REPO_NOT_FOUND), MigrateErrorKind::Invalid);
        assert_eq!(kind(code::METHOD_NOT_FOUND), MigrateErrorKind::Agent);
        assert_eq!(
            MigrateError::agent("Doing", RequestError::Timeout).kind,
            MigrateErrorKind::Agent
        );
        let refused = MigrateError::start_failed(RequestError::Agent {
            code: code::MIGRATE_FAILED,
            message: "The destination exists.\nNot enough space.".into(),
        });
        assert_eq!(refused.kind, MigrateErrorKind::Refused);
        assert_eq!(
            refused.message,
            "Cannot start the move: The destination exists. Not enough space."
        );
        assert_eq!(MigrateError::agent("Doing", agent(7)).message, "Doing: why");
    }
}
