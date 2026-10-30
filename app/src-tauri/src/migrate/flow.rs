//! The helper requests a migration makes, and **Delete original** (spec §6), with the
//! helper and the Windows operations behind traits so the order of the steps is
//! tested with fakes.
//!
//! [`MigrateHelper`] is the thin request layer: one method per helper request, named
//! in [`method`]. `migrate`, `migrateFinish` and `migrateCancel` are protocol
//! [`Method`]s already; the others are not in `protocol.rs` yet, so [`AgentHelper`]
//! answers them with [`not_supported`] until they are, which makes **Delete
//! original** refuse rather than skip its recheck.

use std::path::{Path, PathBuf};
use std::time::Duration;

use refract_client::RequestError;
use refract_core::migrate::preflight::Refusal;
use refract_core::migrate::report::Report;
use refract_core::protocol::{
    code, FinishAction, Method, MigrateCancelParams, MigrateFinishParams, MigrateParams, Reply,
};
use serde::{Deserialize, Serialize};

use super::snapshot::RefusalView;
use super::vscode::VsCodeWindow;
use crate::agent::Agent;

/// The helper's method names, in one place so they can follow the protocol.
pub mod method {
    pub const MIGRATE: &str = "migrate";
    pub const RECHECK: &str = "migrateRecheck";
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
        let views: Vec<RefusalView> = refusals.into_iter().map(RefusalView::from).collect();
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

    /// A failed helper request, made while `doing` something.
    pub fn agent(doing: &str, err: RequestError) -> MigrateError {
        let message = match err {
            RequestError::Agent { message, .. } => message,
            err => err.to_string(),
        };
        MigrateError::new(MigrateErrorKind::Agent, format!("{doing}: {message}"))
    }
}

/// The answer to `migrateRecheck`: what changed in the source since the final
/// snapshot, and the agents still running on it. Both empty means deleting is safe.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Recheck {
    #[serde(default)]
    pub changed: Vec<String>,
    /// Session ids of agents still running in the folder.
    #[serde(default)]
    pub busy_agents: Vec<String>,
}

impl Recheck {
    pub fn is_safe(&self) -> bool {
        self.changed.is_empty() && self.busy_agents.is_empty()
    }
}

/// One migration whose report waits for an answer, from `migratePending`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingMigration {
    pub id: String,
    pub report: Report,
}

/// The helper's side of a migration (see the module docs).
pub trait MigrateHelper: Send + Sync {
    /// `migrate`: answers with the migration's id.
    fn start(&self, params: MigrateParams) -> Result<String, RequestError>;
    /// `migrateRecheck`.
    fn recheck(&self, id: &str) -> Result<Recheck, RequestError>;
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

/// The error for a request the helper protocol does not have yet.
pub fn not_supported(method: &str) -> RequestError {
    RequestError::Agent {
        code: code::METHOD_NOT_FOUND,
        message: format!("this version of the helper does not support {method} yet"),
    }
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

    // TODO: send `migrateRecheck` once protocol.rs has it. It stats every file of the
    // source over `/mnt`, so it needs minutes, like `migrateFinish`.
    fn recheck(&self, _id: &str) -> Result<Recheck, RequestError> {
        Err(not_supported(method::RECHECK))
    }

    // TODO: send `migrateResync` once protocol.rs has it.
    fn resync(&self, _id: &str) -> Result<(), RequestError> {
        Err(not_supported(method::RESYNC))
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

    // TODO: send `migratePending` once protocol.rs has it.
    fn pending(&self) -> Result<Vec<PendingMigration>, RequestError> {
        Err(not_supported(method::PENDING))
    }

    // TODO: send `stopAgent` once protocol.rs has it.
    fn stop_agent(&self, _path: &str, _session_id: &str) -> Result<(), RequestError> {
        Err(not_supported(method::STOP_AGENT))
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

/// What **Delete original** did, sent as `{"kind": "...", ...}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum DeleteOutcome {
    /// The source changed since the report, or agents still run in it: nothing was
    /// deleted. The interface offers **Re-sync**.
    Changed {
        changed: Vec<String>,
        busy_agents: Vec<String>,
    },
    /// VS Code windows still have the folder open: nothing was deleted. The interface
    /// offers **Close** for each.
    WindowsOpen { windows: Vec<VsCodeWindow> },
    /// The Recycle Bin can't take the folder: nothing was deleted. The interface asks
    /// "Delete permanently?" (default **No**, which is **Keep both**).
    WontFit { reason: String },
    /// The original is gone and the helper finished the migration.
    Deleted {
        /// Deleted for good rather than recycled.
        permanent: bool,
        /// The `.lnk` left at the old location.
        #[serde(skip_serializing_if = "Option::is_none")]
        shortcut: Option<String>,
        /// Why no `.lnk` was made; the move itself succeeded.
        #[serde(skip_serializing_if = "Option::is_none")]
        shortcut_error: Option<String>,
    },
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
    /// An earlier attempt already removed the original.
    pub removed: bool,
}

/// A failed **Delete original**: `removed` says whether the original is already gone
/// (so the next attempt only finishes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteFailure {
    pub error: MigrateError,
    pub removed: bool,
}

/// The `.lnk` for a folder at `source`: `<name>.lnk` beside it.
pub fn shortcut_path(source: &Path, name: &str) -> Option<PathBuf> {
    Some(source.parent()?.join(format!("{name}.lnk")))
}

/// **Delete original** (spec §6): the recheck, the VS Code windows, the fit (unless
/// `permanent`), the removal and a check that the folder is gone, the shortcut, then
/// `migrateFinish delete`. Each check that fails stops before anything is deleted and
/// says why in the outcome. When `target.removed`, only the shortcut and the finish
/// are left to do.
pub fn delete_original(
    helper: &dyn MigrateHelper,
    os: &dyn WindowsOps,
    target: &DeleteTarget,
    permanent: bool,
) -> Result<DeleteOutcome, DeleteFailure> {
    let fail = |kind, message: String, removed| DeleteFailure {
        error: MigrateError::new(kind, message),
        removed,
    };
    if !target.removed {
        let recheck = helper.recheck(&target.id).map_err(|err| DeleteFailure {
            error: MigrateError::agent("Cannot check the original again", err),
            removed: false,
        })?;
        if !recheck.is_safe() {
            return Ok(DeleteOutcome::Changed {
                changed: recheck.changed,
                busy_agents: recheck.busy_agents,
            });
        }
        let windows = os.vscode_windows(&target.name);
        if !windows.is_empty() {
            return Ok(DeleteOutcome::WindowsOpen { windows });
        }
        if !permanent {
            if let Fit::WontFit(reason) = os.recycle_fit(&target.source, target.files, target.bytes)
            {
                return Ok(DeleteOutcome::WontFit { reason });
            }
        }
        match os.remove(&target.source, !permanent) {
            Ok(Removal::Done) => {}
            Ok(Removal::WontFit(reason)) => return Ok(DeleteOutcome::WontFit { reason }),
            Err(err) => {
                let gone = !os.exists(&target.source);
                return Err(fail(
                    MigrateErrorKind::Io,
                    format!("Cannot delete the original: {err}"),
                    gone,
                ));
            }
        }
        if os.exists(&target.source) {
            return Err(fail(
                MigrateErrorKind::Io,
                format!(
                    "{} is still there after deleting it. Close any program using it and try again.",
                    target.source.display()
                ),
                false,
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

    helper
        .finish(&target.id, FinishAction::Delete)
        .map_err(|err| DeleteFailure {
            error: MigrateError::agent(
                "The original is in the Recycle Bin, but the helper could not finish",
                err,
            ),
            removed: true,
        })?;
    Ok(DeleteOutcome::Deleted {
        permanent,
        shortcut,
        shortcut_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeHelper {
        recheck: Option<Recheck>,
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
        fn recheck(&self, id: &str) -> Result<Recheck, RequestError> {
            self.log.lock().unwrap().push(format!("recheck {id}"));
            self.recheck
                .clone()
                .ok_or_else(|| not_supported(method::RECHECK))
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
            removed: false,
        }
    }

    fn safe_helper() -> FakeHelper {
        FakeHelper {
            recheck: Some(Recheck::default()),
            ..FakeHelper::default()
        }
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
                shortcut: Some(
                    PathBuf::from(r"C:\dev")
                        .join("aurora-notes.lnk")
                        .display()
                        .to_string()
                ),
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
                    PathBuf::from(r"C:\dev").join("aurora-notes.lnk").display()
                ),
            ]
        );
        assert_eq!(
            *helper.log.lock().unwrap(),
            ["recheck m-1", "finish m-1 Delete"]
        );
    }

    #[test]
    fn changes_or_busy_agents_stop_before_anything_is_touched() {
        let helper = FakeHelper {
            recheck: Some(Recheck {
                changed: vec!["notes.txt".into()],
                busy_agents: vec!["0b7e".into()],
            }),
            ..FakeHelper::default()
        };
        let os = FakeOs::with_source();
        assert_eq!(
            delete_original(&helper, &os, &target(), false).unwrap(),
            DeleteOutcome::Changed {
                changed: vec!["notes.txt".into()],
                busy_agents: vec!["0b7e".into()],
            }
        );
        assert!(os.log.borrow().is_empty());
    }

    #[test]
    fn a_helper_without_recheck_refuses_to_delete() {
        let os = FakeOs::with_source();
        let failure = delete_original(&FakeHelper::default(), &os, &target(), false).unwrap_err();
        assert_eq!(failure.error.kind, MigrateErrorKind::Agent);
        assert!(failure.error.message.contains("migrateRecheck"));
        assert!(!failure.removed);
        assert!(os.log.borrow().is_empty());
    }

    #[test]
    fn open_windows_stop_before_the_fit_check() {
        let os = FakeOs {
            windows: vec![VsCodeWindow {
                id: 7,
                title: "aurora-notes [WSL: Ubuntu] - Visual Studio Code".into(),
            }],
            ..FakeOs::with_source()
        };
        let outcome = delete_original(&safe_helper(), &os, &target(), false).unwrap();
        assert!(matches!(outcome, DeleteOutcome::WindowsOpen { windows } if windows[0].id == 7));
        assert_eq!(*os.log.borrow(), ["windows aurora-notes"]);
    }

    #[test]
    fn a_folder_too_large_for_the_bin_is_kept_unless_permanent() {
        let os = FakeOs {
            fit: Some(Fit::WontFit("too large".into())),
            ..FakeOs::with_source()
        };
        let helper = safe_helper();
        assert_eq!(
            delete_original(&helper, &os, &target(), false).unwrap(),
            DeleteOutcome::WontFit {
                reason: "too large".into()
            }
        );
        assert!(os.exists(Path::new(r"C:\dev\aurora-notes")));
        // The guard in the removal itself says the same.
        let os = FakeOs {
            removal: Some(Ok(Removal::WontFit("would be deleted for good".into()))),
            ..FakeOs::with_source()
        };
        assert!(matches!(
            delete_original(&helper, &os, &target(), false).unwrap(),
            DeleteOutcome::WontFit { .. }
        ));
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
    fn a_folder_that_survives_is_an_error_and_finish_is_not_sent() {
        let helper = safe_helper();
        let os = FakeOs {
            sticky: true,
            ..FakeOs::with_source()
        };
        let failure = delete_original(&helper, &os, &target(), false).unwrap_err();
        assert_eq!(failure.error.kind, MigrateErrorKind::Io);
        assert!(!failure.removed);
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
        let failure = delete_original(&helper, &os, &target(), false).unwrap_err();
        assert!(failure.removed);
        assert_eq!(failure.error.kind, MigrateErrorKind::Agent);

        let helper = safe_helper();
        let retry = DeleteTarget {
            removed: true,
            ..target()
        };
        os.log.borrow_mut().clear();
        let outcome = delete_original(&helper, &os, &retry, false).unwrap();
        assert!(matches!(
            outcome,
            DeleteOutcome::Deleted {
                shortcut: Some(_),
                ..
            }
        ));
        // No recheck, no removal, and the shortcut from the first attempt is kept.
        assert!(os.log.borrow().is_empty());
        assert_eq!(*helper.log.lock().unwrap(), ["finish m-1 Delete"]);
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
            changed: vec!["a".into()],
            busy_agents: vec![],
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"kind": "changed", "changed": ["a"], "busyAgents": []})
        );
        let value = serde_json::to_value(DeleteOutcome::Deleted {
            permanent: false,
            shortcut: None,
            shortcut_error: Some("x".into()),
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"kind": "deleted", "permanent": false, "shortcutError": "x"})
        );
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
}
