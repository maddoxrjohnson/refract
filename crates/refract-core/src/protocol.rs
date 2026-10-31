//! Messages between the Windows app and the WSL helper.
//!
//! Each message is one line of JSON. The app sends requests
//! `{"id": 1, "method": "getRepo", "params": {"path": "..."}}` and gets exactly one
//! response `{"id": 1, "result": ...}` or `{"id": 1, "error": {"code": ..., "message": ...}}`.
//! The id may be any JSON number or string and is echoed back as sent; a line whose id
//! cannot be read is answered with `"id": null`.
//! The helper also sends events `{"event": "repoChanged", "data": ...}`, which have no id.
//! Field names are camelCase on the wire.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::classify::SessionState;
use crate::migrate::report::Report;
use crate::preselect::{Preselect, ResumeReason};
use crate::usage::UsageSummary;

/// Bumped whenever a message changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

/// Error codes, following JSON-RPC where one fits.
pub mod code {
    /// The line was not valid JSON.
    pub const PARSE_ERROR: i32 = -32700;
    /// The JSON was not a request object with a number or string `id` and a `method`.
    pub const INVALID_REQUEST: i32 = -32600;
    /// The method is not known.
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// The method is known but its params are wrong.
    pub const INVALID_PARAMS: i32 = -32602;
    /// The helper failed while handling the request.
    pub const INTERNAL_ERROR: i32 = -32603;
    /// `getRepo` named a path that is not a known repo, `prepareLaunch` a path that is
    /// not a directory, or `addRepo` a path that is not in a git working tree.
    pub const REPO_NOT_FOUND: i32 = 1;
    /// `setupServer` failed to read or change the WSL server's files, or `code-server`
    /// failed.
    pub const SETUP_FAILED: i32 = 2;
    /// `cloneRepo` failed: the destination already exists and is not a clone of the
    /// same URL, or `git clone` failed or took too long.
    pub const CLONE_FAILED: i32 = 3;
    /// `hookInfo`, `installHook` or `uninstallHook` could not read or change Claude
    /// Code's `settings.json` or Refract's hook files.
    pub const HOOK_FAILED: i32 = 4;
    /// `prepareLaunch` asked to focus a session that no process runs any more, or
    /// whose process does not run in the Refract window on that folder.
    pub const NOT_RUNNING: i32 = 5;
    /// `migrate` could not start (a preflight refusal, or the snapshot file is missing
    /// or unreadable), or `migrateFinish`, `migrateCancel`, `migrateRecheck`,
    /// `migrateBeginDelete` or `migrateResync` named a migration that is unknown or not
    /// at that step, or that is refused there (see [`Method::MigrateBeginDelete`]).
    pub const MIGRATE_FAILED: i32 = 6;
    /// `stopAgent` named a session that is working on a turn, so stopping it could lose
    /// work.
    pub const AGENT_BUSY: i32 = 7;
}

/// A request from the app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: Id,
    #[serde(flatten)]
    pub method: Method,
}

/// What a request asks for, with its params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
pub enum Method {
    /// Hello: answered with [`Hello`].
    Ping,
    /// Every local repo: answered with [`RepoList`].
    ListRepos,
    /// One repo by path: answered with a [`RepoSnapshot`].
    GetRepo { path: String },
    /// Leaves a request file for the VS Code extension: answered with a
    /// [`LaunchTicket`]. See `docs/launch.md`.
    PrepareLaunch { path: String, mode: LaunchMode },
    /// Installs the Refract extension in the WSL VS Code server and applies it to every
    /// profile: answered with a [`ServerSetup`]. `vsix` is a Linux path, a Windows
    /// drive path or a `\\wsl.localhost\<distro>\…` path (see
    /// [`crate::launch::wsl_path`]); `commit` is the Windows VS Code's commit, with
    /// `None` meaning the newest server. See `docs/launch.md`.
    SetupServer {
        vsix: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
    },
    /// Clones `url` into `~/projects/<name>` and reports the new repo: answered with
    /// the clone's path as a [`Reply::Path`]. When that directory is already a clone of
    /// `url`, nothing is cloned and its path is the answer. `url` must be `https://`
    /// and `name` a plain folder name (see [`check_clone`]).
    CloneRepo { url: String, name: String },
    /// Reports the repo whose working tree holds `path` from now on, adding it to the
    /// config's `extra` when no root covers it: answered with `null`. `path` is a
    /// Linux, Windows drive or `\\wsl.localhost\<distro>\…` path.
    AddRepo { path: String },
    /// Token use added up from Claude Code's transcripts: answered with a
    /// [`UsageSummary`].
    Usage,
    /// Whether the status hook is installed: answered with a [`HookInfo`].
    HookInfo,
    /// Installs the status hook (spec §6.2): answered with a [`HookInfo`].
    InstallHook,
    /// Removes the status hook, restoring `settings.json`: answered with a [`HookInfo`].
    UninstallHook,
    /// Answered with `null`, then the helper exits.
    Shutdown,
    /// Starts moving a Windows-drive repo into WSL: answered with a
    /// [`MigrationTicket`], then [`Event::MigrateProgress`] and one
    /// [`Event::MigrateReady`]. See [`crate::migrate`].
    Migrate(MigrateParams),
    /// Ends a migration that is ready (see [`FinishAction`]): answered with `null`.
    MigrateFinish(MigrateFinishParams),
    /// Stops a running migration: answered with a [`CancelOutcome`]. Before the copy
    /// is placed, the temporary copy is deleted and the migration ends (`ended: true`).
    /// Once the copy is placed (from the first `installing` progress on), and during a
    /// `migrateResync`, the copy is never deleted: the step stops, a fresh
    /// [`Event::MigrateReady`] follows (or went out just before), and the migration
    /// waits for `migrateFinish` (`ended: false`).
    MigrateCancel(MigrateCancelParams),
    /// Runs check 4 again on a migration whose report allows deleting the original (or
    /// that is deleting it), and checks that the placed copy is as it was checked:
    /// answered with a [`Recheck`]. Refused otherwise.
    MigrateRecheck(MigrateIdParams),
    /// Sent right before the app recycles or deletes anything of the original: runs
    /// the recheck and, when it is safe ([`Recheck::safe`]), puts the migration in the
    /// *deleting* state, saved in its state file, and answers `ok: true`; see
    /// [`BeginDelete`] and [`MigrateBeginDeleteParams`]. Idempotent: in the deleting
    /// state it runs the same test again (a partly deleted original shows as
    /// removals). In the deleting state `migrateResync`, `migrateFinish` `keep`, and
    /// `migrateCancel` are refused ("The original is being deleted."), and
    /// `migrateFinish` `discard` unless the copy is gone; `migrateFinish` `delete`,
    /// sent once the original is gone, ends it, and is refused outside it. Refused
    /// unless the report allows deleting. While a `migrateFinish` `delete` is finishing
    /// (its config write failed), this and `migrateRecheck` still answer, so the app's
    /// retry (this, then `migrateFinish` `delete` again) works.
    MigrateBeginDelete(MigrateBeginDeleteParams),
    /// Re-copies what changed in the original since the report into the placed copy and
    /// checks it again: answered with `null`, then [`Event::MigrateProgress`] and a fresh
    /// [`Event::MigrateReady`]. Refused in the deleting state, when the original folder
    /// is gone, when the placed copy changed since it was checked, and while the
    /// report's `recoveredChanges` is not empty.
    MigrateResync(MigrateIdParams),
    /// The migrations whose reports wait for `migrateFinish`, including those from
    /// before the helper restarted: answered with a list of [`PendingMigration`].
    MigratePending,
    /// Stops (SIGTERM) the `claude` process of a live session, unless it is working on
    /// a turn: answered with `null`. See [`StopAgentParams`].
    StopAgent(StopAgentParams),
}

/// A request id: any JSON number or string, echoed back unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(serde_json::Number),
    String(String),
}

impl From<u64> for Id {
    fn from(n: u64) -> Id {
        Id::Number(n.into())
    }
}

/// The helper's answer to one request. `id` is `None` (`null` on the wire) only when the
/// request's id could not be read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: Option<Id>,
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// Either a result or an error, as the `result` or `error` key of a response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    Result(Reply),
    Error(ErrorBody),
}

/// The result of a successful request. Which variant depends on the method.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Reply {
    Hello(Hello),
    Repos(RepoList),
    Repo(Box<RepoSnapshot>),
    Launch(LaunchTicket),
    ServerSetup(ServerSetup),
    Hook(HookInfo),
    Usage(UsageSummary),
    /// The result of `migrate`.
    Migration(MigrationTicket),
    /// The result of `migrateRecheck`.
    Recheck(Recheck),
    /// The result of `migrateBeginDelete`.
    BeginDelete(BeginDelete),
    /// The result of `migratePending`: the pending migrations, oldest first.
    PendingMigrations(Vec<PendingMigration>),
    /// The result of `migrateCancel`.
    Cancelled(CancelOutcome),
    /// A Linux path: the result of `cloneRepo`.
    Path(String),
    /// `null`, for methods with nothing to return.
    Done,
}

/// Why a request failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: i32,
    pub message: String,
}

/// The result of `ping`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub protocol_version: u32,
    pub agent_version: String,
}

/// The result of `listRepos`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoList {
    pub repos: Vec<RepoSnapshot>,
    /// True while the first load is still running: `repos` holds what is loaded so far,
    /// the rest arrive as `repoChanged` events, and `loadComplete` follows.
    #[serde(default)]
    pub loading: bool,
}

/// Version of the [`LaunchRequest`] file format.
pub const LAUNCH_REQUEST_VERSION: u32 = 1;

/// What `prepareLaunch` should start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum LaunchMode {
    /// A new agent, with an optional opening prompt.
    New {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    /// Resume an existing session.
    Resume { session_id: String },
    /// Bring the VS Code terminal running a live session to the front, starting nothing.
    Focus { session_id: String },
}

/// The result of `prepareLaunch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchTicket {
    pub request_id: String,
    /// Generated for a new agent; the given one for a resume or a focus.
    pub session_id: String,
    /// What to pass to `code --folder-uri`.
    pub folder_uri: String,
}

/// The result of `hookInfo`, `installHook` and `uninstallHook`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookInfo {
    pub installed: bool,
    /// Not installed, but `settings.json` already holds Refract entries: an install
    /// from an older Refract (another event set, command form or status directory), or
    /// one whose script is missing or outdated. Installing updates it in place.
    #[serde(default)]
    pub outdated: bool,
    /// Claude Code's `settings.json` in WSL.
    pub settings_path: String,
    /// Unified diff of what installing would change; `None` when installed.
    pub diff: Option<String>,
}

/// The result of `setupServer`: what it changed, or why it did nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSetup {
    /// The vsix was installed now, because its version was not the installed one.
    pub installed: bool,
    /// The extension was marked application-scoped now.
    pub application_scoped: bool,
    /// Why nothing was done: no vsix at the path, or no VS Code server yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

/// The request file `prepareLaunch` leaves for the VS Code extension, as
/// `<requestId>.json` in the launch directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRequest {
    /// [`LAUNCH_REQUEST_VERSION`].
    pub version: u32,
    pub request_id: String,
    /// Absolute Linux path of the repo, with no trailing slash.
    pub folder: String,
    /// The command to run in the terminal; empty for a focus request.
    pub argv: Vec<String>,
    /// The terminal tab's name.
    pub title: String,
    pub created_at: DateTime<Utc>,
    /// Set when the request shows an existing terminal instead of starting one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<FocusTarget>,
}

/// Which terminal a focus request shows: the one whose shell is among `pids`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FocusTarget {
    pub session_id: String,
    /// The session's `claude` process and its ancestors, nearest first, without pid 1.
    pub pids: Vec<u32>,
}

/// Params of `migrate`: moves the repo at `path` (a `/mnt/...` path) to `dest` (a
/// Linux path that must not exist), in the background. Answered at once with a
/// [`MigrationTicket`], followed by `migrateProgress` events and one `migrateReady`.
/// `snapshot` is the Linux path of the [`crate::migrate::SourceSnapshot`] file the app
/// wrote before sending it (see [`crate::migrate`]). A preflight refusal is answered
/// with [`code::MIGRATE_FAILED`] and the refusals' messages, one per line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateParams {
    pub path: String,
    pub dest: String,
    pub snapshot: String,
}

/// Params of `migrateFinish`: ends a migration whose report is ready. Answered with
/// `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateFinishParams {
    pub id: String,
    pub action: FinishAction,
}

/// Params of `migrateCancel`: stops a migration before its report and deletes the
/// temporary copy, leaving the original untouched. Answered with `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateCancelParams {
    pub id: String,
}

/// The result of `migrate`: the id its events and later requests use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationTicket {
    pub id: String,
}

/// Params of `migrateRecheck` and `migrateResync`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateIdParams {
    pub id: String,
}

/// Params of `migrateBeginDelete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateBeginDeleteParams {
    pub id: String,
    /// The app (Windows) no longer sees the original folder. When the helper can't see
    /// it either, it is gone; when only the helper can't (its drive is not mounted in
    /// WSL), nothing of it was checked, so the deleting state is not entered: `ok` is
    /// false with `sourceMissing`. Missing means `false`.
    #[serde(default)]
    pub source_gone: bool,
    /// Only in the deleting state: the paths of `copyChanged` the user saw and agreed
    /// to delete the rest anyway for, though the copy's version differs from the
    /// original's (the copy keeps its version; for one in `copyRemoved` the original's
    /// is then gone for good), each under how it changed as the user saw it. Only
    /// these are skipped: any other copy change still blocks, as does one that has
    /// since changed the other way. Never skips `changed`, `unreadable`, `busyAgents`,
    /// `copyUnreadable`, a hidden original or a copy that is gone. Missing means none.
    #[serde(default)]
    pub force: Forced,
}

/// The copy changes `migrateBeginDelete` may skip ([`MigrateBeginDeleteParams::force`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Forced {
    /// Paths of `copyChanged` not in `copyRemoved` (edited or added in the copy).
    #[serde(default)]
    pub changed: Vec<String>,
    /// Paths of `copyRemoved` (deleted in the copy).
    #[serde(default)]
    pub removed: Vec<String>,
}

/// The result of `migrateCancel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelOutcome {
    /// The temporary copy was deleted and the migration is gone. False: the copy was
    /// already in place (or a re-sync was running), so it stays, and the migration's
    /// report comes (or came) as `migrateReady`.
    pub ended: bool,
}

/// What `migrateFinish` does with a migration whose report is ready. Each forgets the
/// migration afterwards, so its report no longer comes back after a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FinishAction {
    /// The original is gone (the app recycled it after `migrateBeginDelete` answered
    /// `ok: true`, or it was gone already): the helper deletes the old transcripts
    /// that are unchanged since they were copied (only when the history check passed;
    /// one changed since is copied again first, and kept when its copy changed too),
    /// and rewrites `extra` and `hidden` entries in `agent.toml` that name the old
    /// path to the new one, adding the new path to `extra` when no root covers it.
    /// Refused unless the migration is in the deleting state, and when the copy is
    /// gone (then only `discard` ends it).
    Delete,
    /// Both stay: the old path is added to `hidden` in `agent.toml`, so its gem goes,
    /// and the new path to `extra` when no root covers it. The old transcripts stay.
    /// Refused when the copy was never placed (its checks failed) or is gone, and in
    /// the deleting state.
    Keep,
    /// The copy is deleted, and the transcripts copied for it that are unchanged since.
    /// The original is as it was. Refused in the deleting state, when the original
    /// folder is gone (the copy is then the only one), and when the placed copy
    /// changed since it was checked (it may hold work of yours), as while the report's
    /// `recoveredChanges` is not empty. With the copy gone
    /// (deleted by hand) it only forgets the migration, in any state and whatever
    /// became of the original.
    Discard,
}

/// The result of `migrateRecheck`: what changed in the original since the copy was last
/// checked, what of it could not be read, whether the placed copy is still as checked,
/// and the agents running in the original. Paths are relative and sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Recheck {
    /// Paths modified or added in the original since the last check. A folder whose
    /// time alone changed is not listed.
    pub changed: Vec<String>,
    /// Paths gone from the original since the last check (never paths that merely
    /// could not be read). Still in the copy, so deleting the rest loses nothing.
    #[serde(default)]
    pub removed: Vec<String>,
    /// Paths of the original that could not be read (`"."` for the folder itself), so
    /// whether they changed is unknown.
    #[serde(default)]
    pub unreadable: Vec<String>,
    /// Paths of the placed copy that differ from it as last checked (after the
    /// install), added or removed since, or that could not be read; `["."]` when the
    /// copy is gone.
    #[serde(default)]
    pub copy_changed: Vec<String>,
    /// The paths of `copyChanged` that are gone from the copy (deleted there): deleting
    /// the original's version of one loses it for good.
    #[serde(default)]
    pub copy_removed: Vec<String>,
    /// The paths of `copyChanged` that could not be read in the copy (`"."` for the
    /// folder itself), so whether they changed is unknown. Never forced.
    #[serde(default)]
    pub copy_unreadable: Vec<String>,
    /// The placed copy's folder is gone (`copyChanged` is then `["."]`), as against
    /// one that can't be read (`copyUnreadable: ["."]`).
    #[serde(default)]
    pub copy_missing: bool,
    /// One sentence per `claude` process whose working directory is the original or
    /// inside it, naming its pid, folder and session when known.
    pub busy_agents: Vec<String>,
    /// The original folder itself is gone. The lists about it are then empty.
    #[serde(default)]
    pub source_missing: bool,
}

impl Recheck {
    /// True when deleting (the rest of) the original loses nothing: nothing changed or
    /// unreadable in it, the copy as checked, and no agent running in it. Removed
    /// paths and a missing original are fine, since the copy still holds them.
    pub fn safe(&self) -> bool {
        self.changed.is_empty()
            && self.unreadable.is_empty()
            && self.copy_changed.is_empty()
            && self.busy_agents.is_empty()
    }
}

/// The result of `migrateBeginDelete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BeginDelete {
    /// [`Recheck::safe`]: the migration is now in the deleting state, and the app may
    /// recycle or delete the original. False leaves the state as it was.
    pub ok: bool,
    pub recheck: Recheck,
}

/// A migration whose report waits for `migrateFinish`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingMigration {
    pub id: String,
    pub report: Box<Report>,
    /// In the deleting state (see [`Method::MigrateBeginDelete`]): only
    /// `migrateFinish` `delete` ends it, or `discard` once the copy is gone.
    #[serde(default)]
    pub deleting: bool,
    /// The placed copy is gone (deleted by hand): `migrateFinish` `discard` then only
    /// forgets the migration, whatever became of the original.
    #[serde(default)]
    pub copy_missing: bool,
}

/// Params of `stopAgent`: the repo (as `getRepo` takes it) and the live session to
/// stop. Answered with `null` once SIGTERM is sent. The session's state decides: a
/// session that is idle or waiting on the user is stopped; one that is working is
/// refused with [`code::AGENT_BUSY`]. Without a status hook the live state is unknown,
/// and the transcript decides: stopped when its last turn ended, refused otherwise. A
/// session with no process known for certain is refused with [`code::NOT_RUNNING`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopAgentParams {
    pub path: String,
    pub session_id: String,
}

/// A step of a running migration, as the panel names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MigrateStep {
    /// Copying files; `done` and `total` count bytes.
    Copying,
    /// Rewriting tracked files as Linux git checks them out; counts files.
    Normalising,
    /// Hashing the copy; counts files.
    Hashing,
    /// Comparing git state; counts repos (submodules included).
    CheckingGit,
    /// Re-checking the source for changes and re-copying them; counts rounds of 3.
    Rechecking,
    /// Moving session history; counts transcripts.
    MovingHistory,
    /// Running the install; counts steps.
    Installing,
}

/// Data of a `migrateProgress` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrateProgress {
    pub id: String,
    /// Linux path of the original (`/mnt/...`) and of the copy, as in the report, so a
    /// window that missed the start (a reload) can place the progress.
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub dest: String,
    pub step: MigrateStep,
    pub done: u64,
    pub total: u64,
    /// The copy is in place at `dest` (from the install on, and during a re-sync): a
    /// cancel then keeps it and a report follows.
    #[serde(default)]
    pub placed: bool,
}

/// An unsolicited message from the helper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "camelCase")]
pub enum Event {
    /// A repo appeared or something about it changed. Carries the full snapshot.
    RepoChanged(Box<RepoSnapshot>),
    /// A repo is gone, or no longer under a configured root.
    RepoRemoved { path: String },
    /// One session of a repo changed state or content.
    SessionChanged(SessionChange),
    /// The first load finished; every repo has been reported. Carries no data.
    LoadComplete,
    /// A migration moved on.
    MigrateProgress(MigrateProgress),
    /// A migration finished its checks, passed or failed, and waits for
    /// `migrateFinish`. Also sent again for a pending migration after the helper
    /// restarts.
    MigrateReady { id: String, report: Box<Report> },
}

/// Data of a `sessionChanged` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionChange {
    pub repo_path: String,
    pub session: SessionInfo,
    /// The repo's picker choice, which may change with the session.
    pub preselect: Preselect,
}

/// Anything the helper writes, as the app reads it: a response if it has an `id`,
/// otherwise an event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Response(Response),
    Event(Event),
}

/// Everything the interface shows about one local repo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoSnapshot {
    /// Absolute path of the working tree.
    pub path: String,
    /// The folder name.
    pub name: String,
    /// `remote.origin.url` as configured.
    pub origin_url: Option<String>,
    /// The origin URL's host, owner and name, when it parses.
    pub remote: Option<RemoteId>,
    /// Current branch; `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Commits ahead of the upstream; `None` without an upstream.
    pub ahead: Option<u32>,
    /// Commits behind the upstream; `None` without an upstream.
    pub behind: Option<u32>,
    /// Entries in `git status`: changed, staged, conflicted and untracked.
    pub changed_files: u32,
    /// Absolute paths of those entries. An untracked directory is one entry.
    pub dirty_paths: Vec<String>,
    /// `None` in a repo with no commits.
    pub last_commit: Option<LastCommit>,
    /// Best-effort main language of tracked files.
    pub language: Option<String>,
    /// Total size of files tracked at HEAD, in bytes.
    pub tracked_bytes: u64,
    /// Commits reachable from HEAD in the last 30 days.
    pub commits_30d: u32,
    /// Relative weight in 0..=1 across all repos (see [`crate::mass`]).
    pub mass: f64,
    /// Sessions whose working directory is this repo, most recent first.
    pub sessions: Vec<SessionInfo>,
    pub preselect: Preselect,
}

/// The newest commit on HEAD.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastCommit {
    /// First line of the message.
    pub summary: String,
    /// Committer time, Unix seconds.
    pub time: i64,
}

/// One Claude Code session in a repo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    pub state: SessionState,
    pub last_timestamp: Option<DateTime<Utc>>,
    /// First line of the assistant's last text reply, truncated.
    pub last_assistant_text: Option<String>,
    /// Transcript size in bytes.
    pub byte_size: u64,
    /// Live, and its process is known and runs in a terminal of the Refract VS Code
    /// window on exactly this repo, so the picker can go to it.
    #[serde(default)]
    pub focusable: bool,
    /// Why the session is worth resuming; `null` when it is running or done.
    #[serde(default)]
    pub resume_reason: Option<ResumeReason>,
}

/// A hosted repo named by its remote URL, such as `github.com` / `dev` / `tidepool-api`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteId {
    /// Lowercase host, without user or port.
    pub host: String,
    /// Owner or group path; GitLab subgroups keep their slashes (`group/sub`).
    pub owner: String,
    /// Repo name without `.git`.
    pub name: String,
}

impl RemoteId {
    /// Parses `https://host/owner/name(.git)`, `ssh://git@host[:port]/owner/name.git`
    /// and scp-like `git@host:owner/name.git`. Returns `None` for local paths and
    /// anything without both an owner and a name.
    pub fn parse(url: &str) -> Option<RemoteId> {
        let url = url.trim();
        let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
            if scheme.eq_ignore_ascii_case("file") {
                return None;
            }
            rest.split_once('/')?
        } else {
            // scp-like syntax; a colon before any slash separates host and path.
            let (host, path) = url.split_once(':')?;
            if host.contains('/') {
                return None;
            }
            (host, path)
        };
        let host = host.rsplit('@').next()?;
        let host = host.split(':').next()?.to_ascii_lowercase();
        let path = path.trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let (owner, name) = path.rsplit_once('/')?;
        let owner = owner.trim_start_matches('~');
        if host.is_empty() || owner.is_empty() || name.is_empty() {
            return None;
        }
        Some(RemoteId {
            host,
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }
}

/// A line that could not be turned into a [`Request`]. `id` is set when the line had
/// a readable one; otherwise the error is answered with a `null` id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestError {
    pub id: Option<Id>,
    pub error: ErrorBody,
}

impl RequestError {
    /// The response that reports this error.
    pub fn response(self) -> Response {
        Response {
            id: self.id,
            outcome: Outcome::Error(self.error),
        }
    }
}

/// Checks `cloneRepo` params before anything reaches `git`: `url` must be `https://`
/// with a host, no user part (no `@` before the first `/` after the scheme) and no
/// whitespace or control characters, and `name` a single folder
/// name that is not `.`, holds no `..`, `/`, `\` or control characters and does not
/// start with `-`. Returns why when they are not.
pub fn check_clone(url: &str, name: &str) -> Result<(), String> {
    let https = url
        .get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"));
    let host = url.get(8..).unwrap_or("");
    if !https || host.is_empty() || host.starts_with('/') {
        return Err(format!("url is not an https:// URL: {url:?}"));
    }
    if host[..host.find('/').unwrap_or(host.len())].contains('@') {
        return Err("url must not hold a user name or token".into());
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("url contains whitespace: {url:?}"));
    }
    let bad_name = name.is_empty()
        || name == "."
        || name.contains("..")
        || name.starts_with('-')
        || name
            .chars()
            .any(|c| matches!(c, '/' | '\\') || c.is_control());
    if bad_name {
        return Err(format!("name is not a plain folder name: {name:?}"));
    }
    Ok(())
}

/// Parses one request line, telling apart bad JSON, a missing or unreadable id, an
/// unknown method and bad params. Methods without params also accept `"params": {}`
/// and `"params": null`. `cloneRepo` params are also checked with [`check_clone`].
pub fn parse_request(line: &str) -> Result<Request, RequestError> {
    let fail = |id, code, message: String| RequestError {
        id,
        error: ErrorBody { code, message },
    };
    let mut value: Value = serde_json::from_str(line)
        .map_err(|e| fail(None, code::PARSE_ERROR, format!("invalid JSON: {e}")))?;
    let Some(object) = value.as_object_mut() else {
        return Err(fail(None, code::INVALID_REQUEST, "not an object".into()));
    };
    let id = object
        .get("id")
        .and_then(|id| serde_json::from_value::<Id>(id.clone()).ok());
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Err(fail(id, code::INVALID_REQUEST, "missing method".into()));
    };
    let Some(id) = id else {
        return Err(fail(
            None,
            code::INVALID_REQUEST,
            "missing id, or not a number or string".into(),
        ));
    };
    let takes_params = match method {
        "getRepo" | "prepareLaunch" | "setupServer" | "cloneRepo" | "addRepo" | "migrate"
        | "migrateFinish" | "migrateCancel" | "migrateRecheck" | "migrateBeginDelete"
        | "migrateResync" | "stopAgent" => true,
        "ping" | "listRepos" | "usage" | "hookInfo" | "installHook" | "uninstallHook"
        | "shutdown" | "migratePending" => false,
        _ => {
            return Err(fail(
                Some(id),
                code::METHOD_NOT_FOUND,
                format!("unknown method {method:?}"),
            ))
        }
    };
    let empty = match object.get("params") {
        Some(Value::Null) => true,
        Some(Value::Object(params)) => params.is_empty(),
        _ => false,
    };
    if !takes_params && empty {
        object.remove("params");
    }
    let request: Request = serde_json::from_value(value).map_err(|e| {
        fail(
            Some(id.clone()),
            code::INVALID_PARAMS,
            format!("bad params: {e}"),
        )
    })?;
    if let Method::CloneRepo { url, name } = &request.method {
        check_clone(url, name).map_err(|e| fail(Some(id), code::INVALID_PARAMS, e))?;
    }
    Ok(request)
}

impl Response {
    /// A successful response.
    pub fn ok(id: impl Into<Id>, reply: Reply) -> Response {
        Response {
            id: Some(id.into()),
            outcome: Outcome::Result(reply),
        }
    }

    /// A failed response.
    pub fn err(id: impl Into<Id>, code: i32, message: impl Into<String>) -> Response {
        Response {
            id: Some(id.into()),
            outcome: Outcome::Error(ErrorBody {
                code,
                message: message.into(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{InterruptReason, LiveState};
    use crate::limits::{LimitWindow, PlanLimits};
    use crate::usage::{ModelUsage, ModelWindows, RepoUsage, TokenTotals};
    use serde_json::json;

    fn snapshot() -> RepoSnapshot {
        RepoSnapshot {
            path: "/home/dev/projects/tidepool-api".into(),
            name: "tidepool-api".into(),
            origin_url: Some("git@github.com:dev/tidepool-api.git".into()),
            remote: Some(RemoteId {
                host: "github.com".into(),
                owner: "dev".into(),
                name: "tidepool-api".into(),
            }),
            branch: Some("main".into()),
            ahead: Some(2),
            behind: Some(0),
            changed_files: 1,
            dirty_paths: vec!["/home/dev/projects/tidepool-api/src/lib.rs".into()],
            last_commit: Some(LastCommit {
                summary: "Add tide tables".into(),
                time: 1_773_489_600,
            }),
            language: Some("Rust".into()),
            tracked_bytes: 48_213,
            commits_30d: 9,
            mass: 0.625,
            sessions: vec![SessionInfo {
                id: "3f6c1a52-0000-4000-8000-000000000001".into(),
                state: SessionState::Interrupted(InterruptReason::ApiError),
                last_timestamp: Some("2026-03-14T12:00:00Z".parse().unwrap()),
                last_assistant_text: Some("Tide tables are wired up.".into()),
                byte_size: 20_480,
                focusable: false,
                resume_reason: Some(ResumeReason::Interrupted),
            }],
            preselect: Preselect::Resume("3f6c1a52-0000-4000-8000-000000000001".into()),
        }
    }

    fn empty_snapshot() -> RepoSnapshot {
        RepoSnapshot {
            path: "/home/dev/projects/aurora-notes".into(),
            name: "aurora-notes".into(),
            origin_url: None,
            remote: None,
            branch: None,
            ahead: None,
            behind: None,
            changed_files: 0,
            dirty_paths: vec![],
            last_commit: None,
            language: None,
            tracked_bytes: 0,
            commits_30d: 0,
            mass: 0.0,
            sessions: vec![],
            preselect: Preselect::NoPicker,
        }
    }

    fn round_trip<T>(value: &T) -> Value
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let text = serde_json::to_string(value).unwrap();
        assert!(!text.contains('\n'));
        let back: T = serde_json::from_str(&text).unwrap();
        assert_eq!(&back, value);
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn requests_round_trip() {
        let ping = Request {
            id: 1.into(),
            method: Method::Ping,
        };
        assert_eq!(round_trip(&ping), json!({"id": 1, "method": "ping"}));
        let list = Request {
            id: 2.into(),
            method: Method::ListRepos,
        };
        assert_eq!(round_trip(&list), json!({"id": 2, "method": "listRepos"}));
        let get = Request {
            id: 3.into(),
            method: Method::GetRepo {
                path: "/home/dev/projects/tidepool-api".into(),
            },
        };
        assert_eq!(
            round_trip(&get),
            json!({"id": 3, "method": "getRepo", "params": {"path": "/home/dev/projects/tidepool-api"}})
        );
        let shutdown = Request {
            id: 4.into(),
            method: Method::Shutdown,
        };
        assert_eq!(
            round_trip(&shutdown),
            json!({"id": 4, "method": "shutdown"})
        );
    }

    #[test]
    fn launch_messages_round_trip() {
        let new = Request {
            id: 5.into(),
            method: Method::PrepareLaunch {
                path: "/home/dev/projects/tidepool-api".into(),
                mode: LaunchMode::New { prompt: None },
            },
        };
        assert_eq!(
            round_trip(&new),
            json!({"id": 5, "method": "prepareLaunch", "params": {
                "path": "/home/dev/projects/tidepool-api", "mode": {"kind": "new"}}})
        );
        let req = parse_request(
            r#"{"id":6,"method":"prepareLaunch","params":{"path":"/x","mode":{"kind":"new","prompt":"Fix it"}}}"#,
        )
        .unwrap();
        assert_eq!(
            req.method,
            Method::PrepareLaunch {
                path: "/x".into(),
                mode: LaunchMode::New {
                    prompt: Some("Fix it".into())
                }
            }
        );
        let req = parse_request(
            r#"{"id":7,"method":"prepareLaunch","params":{"path":"/x","mode":{"kind":"resume","sessionId":"abc"}}}"#,
        )
        .unwrap();
        assert_eq!(
            req.method,
            Method::PrepareLaunch {
                path: "/x".into(),
                mode: LaunchMode::Resume {
                    session_id: "abc".into()
                }
            }
        );
        let err = parse_request(
            r#"{"id":8,"method":"prepareLaunch","params":{"path":"/x","mode":{"kind":"resume"}}}"#,
        )
        .unwrap_err();
        assert_eq!(err.error.code, code::INVALID_PARAMS);

        let ticket = Response::ok(
            5,
            Reply::Launch(LaunchTicket {
                request_id: "6f1c0000-0000-4000-8000-000000000001".into(),
                session_id: "0b7e0000-0000-4000-8000-000000000002".into(),
                folder_uri: "vscode-remote://wsl+Ubuntu/home/dev/projects/tidepool-api".into(),
            }),
        );
        assert_eq!(
            round_trip(&ticket),
            json!({"id": 5, "result": {
                "requestId": "6f1c0000-0000-4000-8000-000000000001",
                "sessionId": "0b7e0000-0000-4000-8000-000000000002",
                "folderUri": "vscode-remote://wsl+Ubuntu/home/dev/projects/tidepool-api"}})
        );

        let file = LaunchRequest {
            version: LAUNCH_REQUEST_VERSION,
            request_id: "6f1c0000-0000-4000-8000-000000000001".into(),
            folder: "/home/dev/projects/tidepool-api".into(),
            argv: vec!["claude".into(), "--resume".into(), "abc".into()],
            title: "Resume".into(),
            created_at: "2026-10-29T17:54:44Z".parse().unwrap(),
            focus: None,
        };
        assert_eq!(
            round_trip(&file),
            json!({
                "version": 1,
                "requestId": "6f1c0000-0000-4000-8000-000000000001",
                "folder": "/home/dev/projects/tidepool-api",
                "argv": ["claude", "--resume", "abc"],
                "title": "Resume",
                "createdAt": "2026-10-29T17:54:44Z"
            })
        );

        let req = parse_request(
            r#"{"id":9,"method":"prepareLaunch","params":{"path":"/x","mode":{"kind":"focus","sessionId":"abc"}}}"#,
        )
        .unwrap();
        assert_eq!(
            req.method,
            Method::PrepareLaunch {
                path: "/x".into(),
                mode: LaunchMode::Focus {
                    session_id: "abc".into()
                }
            }
        );
        let focus = LaunchRequest {
            argv: vec![],
            title: "Running agent".into(),
            focus: Some(FocusTarget {
                session_id: "abc".into(),
                pids: vec![812, 640],
            }),
            ..file
        };
        assert_eq!(
            round_trip(&focus),
            json!({
                "version": 1,
                "requestId": "6f1c0000-0000-4000-8000-000000000001",
                "folder": "/home/dev/projects/tidepool-api",
                "argv": [],
                "title": "Running agent",
                "createdAt": "2026-10-29T17:54:44Z",
                "focus": {"sessionId": "abc", "pids": [812, 640]}
            })
        );
    }

    #[test]
    fn setup_server_round_trips() {
        let setup = Request {
            id: 9.into(),
            method: Method::SetupServer {
                vsix: "C:\\Users\\dev\\refract.vsix".into(),
                commit: Some("1a2b3c".into()),
            },
        };
        assert_eq!(
            round_trip(&setup),
            json!({"id": 9, "method": "setupServer", "params": {
                "vsix": "C:\\Users\\dev\\refract.vsix", "commit": "1a2b3c"}})
        );
        let newest = Request {
            id: 10.into(),
            method: Method::SetupServer {
                vsix: "/home/dev/refract.vsix".into(),
                commit: None,
            },
        };
        assert_eq!(
            round_trip(&newest),
            json!({"id": 10, "method": "setupServer", "params": {"vsix": "/home/dev/refract.vsix"}})
        );
        for line in [
            r#"{"id":10,"method":"setupServer","params":{"vsix":"/home/dev/refract.vsix"}}"#,
            r#"{"id":10,"method":"setupServer","params":{"vsix":"/home/dev/refract.vsix","commit":null}}"#,
        ] {
            assert_eq!(parse_request(line).unwrap(), newest, "{line}");
        }
        for line in [
            r#"{"id":11,"method":"setupServer"}"#,
            r#"{"id":11,"method":"setupServer","params":{}}"#,
            r#"{"id":11,"method":"setupServer","params":{"vsix":7}}"#,
        ] {
            let err = parse_request(line).unwrap_err();
            assert_eq!(err.error.code, code::INVALID_PARAMS, "{line}");
        }

        let done = Response::ok(
            9,
            Reply::ServerSetup(ServerSetup {
                installed: true,
                application_scoped: false,
                skipped: None,
            }),
        );
        assert_eq!(
            round_trip(&done),
            json!({"id": 9, "result": {"installed": true, "applicationScoped": false}})
        );
        let skipped = Response::ok(
            10,
            Reply::ServerSetup(ServerSetup {
                installed: false,
                application_scoped: false,
                skipped: Some("no vsix at /home/dev/refract.vsix".into()),
            }),
        );
        assert_eq!(
            round_trip(&skipped)["result"]["skipped"],
            "no vsix at /home/dev/refract.vsix"
        );
    }

    #[test]
    fn clone_and_add_repo_round_trip() {
        let clone = Request {
            id: 12.into(),
            method: Method::CloneRepo {
                url: "https://example.com/dev/kelp-cli.git".into(),
                name: "kelp-cli".into(),
            },
        };
        let value = round_trip(&clone);
        assert_eq!(
            value,
            json!({"id": 12, "method": "cloneRepo", "params": {
                "url": "https://example.com/dev/kelp-cli.git", "name": "kelp-cli"}})
        );
        assert_eq!(parse_request(&value.to_string()).unwrap(), clone);

        let add = Request {
            id: 13.into(),
            method: Method::AddRepo {
                path: "/home/dev/work/aurora-notes".into(),
            },
        };
        let value = round_trip(&add);
        assert_eq!(
            value,
            json!({"id": 13, "method": "addRepo", "params": {"path": "/home/dev/work/aurora-notes"}})
        );
        assert_eq!(parse_request(&value.to_string()).unwrap(), add);

        let cloned = Response::ok(12, Reply::Path("/home/dev/projects/kelp-cli".into()));
        assert_eq!(
            round_trip(&cloned),
            json!({"id": 12, "result": "/home/dev/projects/kelp-cli"})
        );
        for line in [
            r#"{"id":14,"method":"cloneRepo","params":{"url":"https://example.com/dev/kelp-cli"}}"#,
            r#"{"id":14,"method":"addRepo"}"#,
            r#"{"id":14,"method":"addRepo","params":{}}"#,
        ] {
            let err = parse_request(line).unwrap_err();
            assert_eq!(err.error.code, code::INVALID_PARAMS, "{line}");
        }
    }

    #[test]
    fn clone_params_are_checked() {
        let parse = |url: &str, name: &str| {
            let line =
                json!({"id": 15, "method": "cloneRepo", "params": {"url": url, "name": name}});
            parse_request(&line.to_string())
        };
        for (url, name) in [
            ("https://example.com/dev/kelp-cli.git", "kelp-cli"),
            (
                "HTTPS://git.example.com/example-group/reef-maps",
                "reef-maps",
            ),
            ("https://example.com/dev/tidepool.api", "tidepool.api"),
            ("https://example.com/dev/kelp-cli@v2", "kelp-cli"),
        ] {
            assert!(parse(url, name).is_ok(), "{url} {name}");
        }
        for (url, name) in [
            ("http://example.com/dev/kelp-cli.git", "kelp-cli"),
            ("git@example.com:dev/kelp-cli.git", "kelp-cli"),
            ("ssh://git@example.com/dev/kelp-cli.git", "kelp-cli"),
            ("file:///srv/git/kelp-cli.git", "kelp-cli"),
            ("--upload-pack=touch /tmp/x", "kelp-cli"),
            ("/srv/git/kelp-cli.git", "kelp-cli"),
            ("https://", "kelp-cli"),
            ("https:///srv/git/kelp-cli.git", "kelp-cli"),
            ("https://token@example.com/dev/kelp-cli.git", "kelp-cli"),
            (
                "https://dev:secret@example.com/dev/kelp-cli.git",
                "kelp-cli",
            ),
            ("https://@example.com/dev/kelp-cli.git", "kelp-cli"),
            ("https://example.com/dev/kelp cli.git", "kelp-cli"),
            ("https://example.com/dev/kelp-cli.git\n", "kelp-cli"),
            ("https://example.com/dev/kelp-cli.git", ""),
            ("https://example.com/dev/kelp-cli.git", "."),
            ("https://example.com/dev/kelp-cli.git", ".."),
            ("https://example.com/dev/kelp-cli.git", "../kelp-cli"),
            ("https://example.com/dev/kelp-cli.git", "dev/kelp-cli"),
            ("https://example.com/dev/kelp-cli.git", "dev\\kelp-cli"),
            ("https://example.com/dev/kelp-cli.git", "-kelp-cli"),
            ("https://example.com/dev/kelp-cli.git", "--template=/tmp"),
            ("https://example.com/dev/kelp-cli.git", "kelp\u{0}cli"),
        ] {
            let err = parse(url, name).unwrap_err();
            assert_eq!(
                (err.id, err.error.code),
                (Some(15.into()), code::INVALID_PARAMS),
                "{url:?} {name:?}"
            );
        }
    }

    /// `Reply` is untagged, so each variant must read back as itself, as the app reads
    /// it: inside a [`Message`].
    #[test]
    fn hook_messages_round_trip() {
        for (method, name) in [
            (Method::HookInfo, "hookInfo"),
            (Method::InstallHook, "installHook"),
            (Method::UninstallHook, "uninstallHook"),
        ] {
            let request = Request {
                id: 1.into(),
                method: method.clone(),
            };
            assert_eq!(round_trip(&request), json!({"id": 1, "method": name}));
            let line = format!(r#"{{"id":1,"method":"{name}","params":{{}}}}"#);
            assert_eq!(parse_request(&line).unwrap().method, method);
        }
        let not_installed = Reply::Hook(HookInfo {
            installed: false,
            outdated: true,
            settings_path: "/home/dev/.claude/settings.json".into(),
            diff: Some("--- a\n+++ b\n".into()),
        });
        let installed = Reply::Hook(HookInfo {
            installed: true,
            outdated: false,
            settings_path: "/home/dev/.claude/settings.json".into(),
            diff: None,
        });
        let wire = round_trip(&Message::Response(Response::ok(1, installed)));
        assert_eq!(
            wire["result"],
            json!({"installed": true, "outdated": false, "settingsPath": "/home/dev/.claude/settings.json", "diff": null})
        );
        round_trip(&Message::Response(Response::ok(2, not_installed)));
    }

    #[test]
    fn every_reply_reads_back_as_its_own_variant() {
        let replies = [
            Reply::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                agent_version: "0.1.0".into(),
            }),
            Reply::Repos(RepoList {
                repos: vec![],
                loading: false,
            }),
            Reply::Repos(RepoList {
                repos: vec![snapshot()],
                loading: true,
            }),
            Reply::Repo(Box::new(snapshot())),
            Reply::Repo(Box::new(empty_snapshot())),
            Reply::Launch(LaunchTicket {
                request_id: "6f1c0000-0000-4000-8000-000000000001".into(),
                session_id: "0b7e0000-0000-4000-8000-000000000002".into(),
                folder_uri: "vscode-remote://wsl+Ubuntu/home/dev/projects/tidepool-api".into(),
            }),
            Reply::ServerSetup(ServerSetup {
                installed: false,
                application_scoped: false,
                skipped: None,
            }),
            Reply::ServerSetup(ServerSetup {
                installed: false,
                application_scoped: false,
                skipped: Some("no VS Code server".into()),
            }),
            Reply::Usage(usage()),
            Reply::Usage(UsageSummary {
                by_repo: vec![],
                ..usage()
            }),
            Reply::Migration(MigrationTicket { id: "m-1".into() }),
            Reply::Recheck(Recheck::default()),
            Reply::BeginDelete(BeginDelete {
                ok: true,
                recheck: Recheck {
                    removed: vec!["notes.txt".into()],
                    source_missing: true,
                    ..Recheck::default()
                },
            }),
            Reply::PendingMigrations(vec![]),
            Reply::Cancelled(CancelOutcome { ended: true }),
            Reply::Cancelled(CancelOutcome { ended: false }),
            Reply::Path("/home/dev/projects/kelp-cli".into()),
            Reply::Done,
        ];
        for reply in replies {
            let message = Message::Response(Response::ok(1, reply));
            round_trip(&message);
        }
    }

    fn usage() -> UsageSummary {
        let totals = TokenTotals {
            input: 1_200,
            output: 3_400,
            cache_read: 56_000,
            cache_write: 7_800,
        };
        UsageSummary {
            today: totals,
            last_7_days: totals,
            last_5_hours: TokenTotals::default(),
            live_agents: 2,
            by_repo: vec![RepoUsage {
                path: "/home/dev/projects/tidepool-api".into(),
                name: "tidepool-api".into(),
                last_7_days: totals,
            }],
            by_model: ModelWindows {
                today: vec![ModelUsage {
                    model: "claude-opus-5-5".into(),
                    tokens: totals,
                    cache_write_1h: 800,
                }],
                last_7_days: vec![],
                last_5_hours: vec![],
            },
            limits: Some(Box::new(PlanLimits {
                five_hour: Some(LimitWindow {
                    used_percent: 37.5,
                    resets_at: "2026-03-14T20:30:00Z".parse().unwrap(),
                }),
                seven_day: None,
                updated_at: "2026-03-14T17:59:00Z".parse().unwrap(),
            })),
            computed_at: "2026-03-14T18:00:00Z".parse().unwrap(),
            ready: true,
        }
    }

    #[test]
    fn usage_round_trips() {
        let request = parse_request(r#"{"id":16,"method":"usage","params":{}}"#).unwrap();
        assert_eq!(request.method, Method::Usage);
        assert_eq!(round_trip(&request), json!({"id": 16, "method": "usage"}));
        let totals = json!({"input": 1200, "output": 3400, "cacheRead": 56000, "cacheWrite": 7800});
        assert_eq!(
            round_trip(&Response::ok(16, Reply::Usage(usage()))),
            json!({"id": 16, "result": {
                "today": totals,
                "last7Days": totals,
                "last5Hours": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                "liveAgents": 2,
                "byRepo": [{"path": "/home/dev/projects/tidepool-api", "name": "tidepool-api", "last7Days": totals}],
                "byModel": {
                    "today": [{"model": "claude-opus-5-5", "input": 1200, "output": 3400,
                        "cacheRead": 56000, "cacheWrite": 7800, "cacheWrite1h": 800}],
                    "last7Days": [],
                    "last5Hours": []},
                "limits": {
                    "fiveHour": {"usedPercent": 37.5, "resetsAt": "2026-03-14T20:30:00Z"},
                    "updatedAt": "2026-03-14T17:59:00Z"},
                "computedAt": "2026-03-14T18:00:00Z",
                "ready": true}})
        );
        // With no reading, `limits` is left out rather than sent as null.
        let none = serde_json::to_value(UsageSummary {
            limits: None,
            ..usage()
        })
        .unwrap();
        assert!(none.get("limits").is_none());
        // A summary from a helper older than `byModel` still reads.
        let old: UsageSummary = serde_json::from_value(json!({
            "today": totals, "last7Days": totals, "last5Hours": totals, "liveAgents": 0,
            "byRepo": [], "computedAt": "2026-03-14T18:00:00Z", "ready": true}))
        .unwrap();
        assert_eq!(old.by_model, ModelWindows::default());
        assert_eq!(old.limits, None);
    }

    #[test]
    fn responses_round_trip() {
        let hello = Response::ok(
            1,
            Reply::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                agent_version: "0.1.0".into(),
            }),
        );
        assert_eq!(
            round_trip(&hello),
            json!({"id": 1, "result": {"protocolVersion": 1, "agentVersion": "0.1.0"}})
        );

        let repos = Response::ok(
            2,
            Reply::Repos(RepoList {
                repos: vec![snapshot(), empty_snapshot()],
                loading: true,
            }),
        );
        assert_eq!(round_trip(&repos)["result"]["loading"], true);
        let old: RepoList = serde_json::from_str(r#"{"repos":[]}"#).unwrap();
        assert!(!old.loading);

        let repo = Response::ok(3, Reply::Repo(Box::new(snapshot())));
        round_trip(&repo);
        let empty = Response::ok(3, Reply::Repo(Box::new(empty_snapshot())));
        round_trip(&empty);

        let done = Response::ok(4, Reply::Done);
        assert_eq!(round_trip(&done), json!({"id": 4, "result": null}));

        let error = Response::err(5, code::REPO_NOT_FOUND, "no such repo");
        assert_eq!(
            round_trip(&error),
            json!({"id": 5, "error": {"code": 1, "message": "no such repo"}})
        );
    }

    #[test]
    fn snapshot_uses_camel_case() {
        let value = serde_json::to_value(snapshot()).unwrap();
        for key in [
            "originUrl",
            "changedFiles",
            "dirtyPaths",
            "lastCommit",
            "trackedBytes",
            "commits30d",
        ] {
            assert!(value.get(key).is_some(), "missing {key}");
        }
        let session = &value["sessions"][0];
        assert_eq!(
            session["state"],
            json!({"kind": "interrupted", "detail": "apiError"})
        );
        assert_eq!(session["lastTimestamp"], json!("2026-03-14T12:00:00Z"));
        assert_eq!(session["resumeReason"], json!("interrupted"));
        assert_eq!(
            value["preselect"],
            json!({"kind": "resume", "sessionId": "3f6c1a52-0000-4000-8000-000000000001"})
        );
        let empty = serde_json::to_value(empty_snapshot()).unwrap();
        assert_eq!(empty["preselect"], json!({"kind": "noPicker"}));
    }

    #[test]
    fn events_round_trip() {
        let changed = Event::RepoChanged(Box::new(snapshot()));
        let value = round_trip(&changed);
        assert_eq!(value["event"], "repoChanged");
        assert_eq!(value["data"]["name"], "tidepool-api");

        let removed = Event::RepoRemoved {
            path: "/home/dev/projects/aurora-notes".into(),
        };
        assert_eq!(
            round_trip(&removed),
            json!({"event": "repoRemoved", "data": {"path": "/home/dev/projects/aurora-notes"}})
        );

        let session = Event::SessionChanged(SessionChange {
            repo_path: "/home/dev/projects/tidepool-api".into(),
            session: SessionInfo {
                id: "3f6c1a52-0000-4000-8000-000000000002".into(),
                state: SessionState::Live(LiveState::Unknown),
                last_timestamp: None,
                last_assistant_text: None,
                byte_size: 0,
                focusable: true,
                resume_reason: None,
            },
            preselect: Preselect::Focus("3f6c1a52-0000-4000-8000-000000000002".into()),
        });
        let value = round_trip(&session);
        assert_eq!(value["event"], "sessionChanged");
        assert_eq!(
            value["data"]["session"]["state"],
            json!({"kind": "live", "detail": "unknown"})
        );
        assert_eq!(value["data"]["session"]["focusable"], true);
        assert_eq!(value["data"]["session"]["resumeReason"], Value::Null);
        assert_eq!(
            value["data"]["preselect"],
            json!({"kind": "focus", "sessionId": "3f6c1a52-0000-4000-8000-000000000002"})
        );

        assert_eq!(
            round_trip(&Event::LoadComplete),
            json!({"event": "loadComplete"})
        );
        let message: Message = serde_json::from_str(r#"{"event":"loadComplete"}"#).unwrap();
        assert_eq!(message, Message::Event(Event::LoadComplete));
    }

    #[test]
    fn migration_messages_round_trip() {
        let params = MigrateParams {
            path: "/mnt/c/Users/dev/Desktop/aurora-notes".into(),
            dest: "/home/dev/projects/aurora-notes".into(),
            snapshot: "/mnt/c/Users/dev/AppData/Local/refract/migrate/m-1.json".into(),
        };
        assert_eq!(
            round_trip(&params),
            json!({"path": "/mnt/c/Users/dev/Desktop/aurora-notes",
                "dest": "/home/dev/projects/aurora-notes",
                "snapshot": "/mnt/c/Users/dev/AppData/Local/refract/migrate/m-1.json"})
        );
        for (action, name) in [
            (FinishAction::Delete, "delete"),
            (FinishAction::Keep, "keep"),
            (FinishAction::Discard, "discard"),
        ] {
            let finish = MigrateFinishParams {
                id: "m-1".into(),
                action,
            };
            assert_eq!(round_trip(&finish), json!({"id": "m-1", "action": name}));
        }
        assert!(serde_json::from_value::<MigrateFinishParams>(
            json!({"id": "m-1", "action": "erase"})
        )
        .is_err());
        assert_eq!(
            round_trip(&MigrateCancelParams { id: "m-1".into() }),
            json!({"id": "m-1"})
        );

        let ticket = Message::Response(Response::ok(
            20,
            Reply::Migration(MigrationTicket { id: "m-1".into() }),
        ));
        assert_eq!(
            round_trip(&ticket),
            json!({"id": 20, "result": {"id": "m-1"}})
        );

        let progress = Event::MigrateProgress(MigrateProgress {
            id: "m-1".into(),
            source: "/mnt/c/dev/aurora-notes".into(),
            dest: "/home/dev/projects/aurora-notes".into(),
            step: MigrateStep::CheckingGit,
            done: 1,
            total: 3,
            placed: true,
        });
        assert_eq!(
            round_trip(&Message::Event(progress)),
            json!({"event": "migrateProgress", "data": {
                "id": "m-1", "source": "/mnt/c/dev/aurora-notes",
                "dest": "/home/dev/projects/aurora-notes", "step": "checkingGit",
                "done": 1, "total": 3, "placed": true}})
        );

        let report = Report {
            version: crate::migrate::report::REPORT_VERSION,
            id: "m-1".into(),
            source: "/mnt/c/Users/dev/Desktop/aurora-notes".into(),
            dest: "/home/dev/projects/aurora-notes".into(),
            started_at: "2026-10-30T10:00:00Z".parse().unwrap(),
            finished_at: "2026-10-30T10:05:00Z".parse().unwrap(),
            checks: vec![],
            skipped: vec![],
            dropped_streams: vec![],
            new_objects: 0,
            source_totals: Default::default(),
            copy_totals: Default::default(),
            install: vec![],
            install_changes: Default::default(),
            recovered_changes: Default::default(),
            install_backup: None,
            history: Default::default(),
            linux_git: false,
            warnings: vec![],
            placed: true,
        };
        let ready = Event::MigrateReady {
            id: "m-1".into(),
            report: Box::new(report),
        };
        let value = round_trip(&Message::Event(ready));
        assert_eq!(value["event"], "migrateReady");
        assert_eq!(value["data"]["id"], "m-1");
        assert_eq!(
            value["data"]["report"]["dest"],
            "/home/dev/projects/aurora-notes"
        );
    }

    #[test]
    fn migration_follow_up_messages_round_trip() {
        for (method, value) in [
            ("migrateRecheck", json!({"id": "m-1"})),
            ("migrateResync", json!({"id": "m-1"})),
        ] {
            let line = json!({"id": 1, "method": method, "params": value}).to_string();
            let request = parse_request(&line).unwrap();
            let params = MigrateIdParams { id: "m-1".into() };
            let expected = match method {
                "migrateRecheck" => Method::MigrateRecheck(params),
                _ => Method::MigrateResync(params),
            };
            assert_eq!(request.method, expected);
            assert_eq!(round_trip(&request)["params"], value);
        }
        // An older app sends only the id: neither gone nor forced.
        let begin =
            parse_request(r#"{"id":1,"method":"migrateBeginDelete","params":{"id":"m-1"}}"#);
        assert_eq!(
            begin.unwrap().method,
            Method::MigrateBeginDelete(MigrateBeginDeleteParams {
                id: "m-1".into(),
                source_gone: false,
                force: Forced::default(),
            })
        );
        let begin = MigrateBeginDeleteParams {
            id: "m-1".into(),
            source_gone: true,
            force: Forced {
                changed: vec!["README.md".into()],
                removed: vec!["notes.txt".into()],
            },
        };
        assert_eq!(
            round_trip(&begin),
            json!({"id": "m-1", "sourceGone": true,
                   "force": {"changed": ["README.md"], "removed": ["notes.txt"]}})
        );
        // Either list may be left out.
        let begin = parse_request(
            r#"{"id":1,"method":"migrateBeginDelete","params":{"id":"m-1","force":{"removed":["a"]}}}"#,
        );
        let Method::MigrateBeginDelete(params) = begin.unwrap().method else {
            panic!("not migrateBeginDelete");
        };
        assert_eq!(params.force.changed, Vec::<String>::new());
        assert_eq!(params.force.removed, vec!["a".to_owned()]);
        let cancelled = Message::Response(Response::ok(
            4,
            Reply::Cancelled(CancelOutcome { ended: false }),
        ));
        assert_eq!(round_trip(&cancelled)["result"], json!({"ended": false}));
        let pending = parse_request(r#"{"id":2,"method":"migratePending","params":{}}"#);
        assert_eq!(pending.unwrap().method, Method::MigratePending);
        let stop = parse_request(
            r#"{"id":3,"method":"stopAgent","params":{"path":"/mnt/c/dev/aurora-notes","sessionId":"s-1"}}"#,
        )
        .unwrap();
        assert_eq!(
            stop.method,
            Method::StopAgent(StopAgentParams {
                path: "/mnt/c/dev/aurora-notes".into(),
                session_id: "s-1".into(),
            })
        );
        let missing = parse_request(r#"{"id":4,"method":"stopAgent","params":{"path":"/x"}}"#);
        assert_eq!(missing.unwrap_err().error.code, code::INVALID_PARAMS);

        let recheck = Message::Response(Response::ok(
            5,
            Reply::Recheck(Recheck {
                changed: vec!["notes.txt".into()],
                removed: vec!["old.txt".into()],
                unreadable: vec!["locked".into()],
                copy_changed: vec!["."].into_iter().map(String::from).collect(),
                copy_removed: vec![],
                copy_unreadable: vec![],
                copy_missing: true,
                busy_agents: vec![
                    "An agent (pid 4242) is running in /mnt/c/dev/aurora-notes.".into()
                ],
                source_missing: false,
            }),
        ));
        assert_eq!(
            round_trip(&recheck),
            json!({"id": 5, "result": {"changed": ["notes.txt"], "removed": ["old.txt"],
                "unreadable": ["locked"], "copyChanged": ["."], "copyRemoved": [],
                "copyUnreadable": [], "copyMissing": true, "busyAgents": [
                "An agent (pid 4242) is running in /mnt/c/dev/aurora-notes."],
                "sourceMissing": false}})
        );
        let empty = Message::Response(Response::ok(6, Reply::Recheck(Recheck::default())));
        round_trip(&empty);
        assert!(Recheck::default().safe());
        let gone = Recheck {
            removed: vec!["a".into()],
            source_missing: true,
            ..Recheck::default()
        };
        assert!(gone.safe());
        for unsafe_one in [
            Recheck {
                changed: vec!["a".into()],
                ..Recheck::default()
            },
            Recheck {
                unreadable: vec!["a".into()],
                ..Recheck::default()
            },
            Recheck {
                copy_changed: vec![".".into()],
                ..Recheck::default()
            },
            Recheck {
                busy_agents: vec!["x".into()],
                ..Recheck::default()
            },
        ] {
            assert!(!unsafe_one.safe(), "{unsafe_one:?}");
        }
        let begin = Message::Response(Response::ok(
            8,
            Reply::BeginDelete(BeginDelete {
                ok: false,
                recheck: Recheck {
                    changed: vec!["notes.txt".into()],
                    ..Recheck::default()
                },
            }),
        ));
        let value = round_trip(&begin);
        assert_eq!(value["result"]["ok"], false);
        assert_eq!(value["result"]["recheck"]["changed"][0], "notes.txt");

        let report = Report {
            version: crate::migrate::report::REPORT_VERSION,
            id: "m-1".into(),
            source: "/mnt/c/Users/dev/Desktop/aurora-notes".into(),
            dest: "/home/dev/projects/aurora-notes".into(),
            started_at: "2026-10-30T10:00:00Z".parse().unwrap(),
            finished_at: "2026-10-30T10:05:00Z".parse().unwrap(),
            checks: vec![],
            skipped: vec![],
            dropped_streams: vec![],
            new_objects: 0,
            source_totals: Default::default(),
            copy_totals: Default::default(),
            install: vec![],
            install_changes: Default::default(),
            recovered_changes: Default::default(),
            install_backup: None,
            history: Default::default(),
            linux_git: false,
            warnings: vec![],
            placed: true,
        };
        for migrations in [
            vec![],
            vec![PendingMigration {
                id: "m-1".into(),
                report: Box::new(report),
                deleting: true,
                copy_missing: true,
            }],
        ] {
            let pending = Message::Response(Response::ok(7, Reply::PendingMigrations(migrations)));
            let value = round_trip(&pending);
            assert!(value["result"].is_array());
        }
    }

    #[test]
    fn messages_are_told_apart_by_id() {
        let response = Message::Response(Response::ok(7, Reply::Done));
        round_trip(&response);
        let event = Message::Event(Event::RepoRemoved {
            path: "/home/dev/projects/aurora-notes".into(),
        });
        round_trip(&event);
        let hello: Message = serde_json::from_str(
            r#"{"id":1,"result":{"protocolVersion":1,"agentVersion":"0.1.0"}}"#,
        )
        .unwrap();
        assert!(matches!(
            hello,
            Message::Response(Response {
                outcome: Outcome::Result(Reply::Hello(_)),
                ..
            })
        ));
    }

    #[test]
    fn parse_request_accepts_valid_lines() {
        let req = parse_request(r#"{"id":9,"method":"getRepo","params":{"path":"/x"}}"#);
        assert_eq!(
            req,
            Ok(Request {
                id: 9.into(),
                method: Method::GetRepo { path: "/x".into() }
            })
        );
        let req = parse_request(r#"{"id":1,"method":"ping","params":null}"#).unwrap();
        assert_eq!(req.method, Method::Ping);
        let req = parse_request(r#"{"id":1,"method":"ping","params":{}}"#).unwrap();
        assert_eq!(req.method, Method::Ping);
        let req = parse_request(r#"{"id":1,"method":"listRepos","params":{}}"#).unwrap();
        assert_eq!(req.method, Method::ListRepos);
    }

    #[test]
    fn ids_may_be_numbers_or_strings_and_are_echoed() {
        let req = parse_request(r#"{"id":"7","method":"ping"}"#).unwrap();
        assert_eq!(req.id, Id::String("7".into()));
        let value = serde_json::to_value(Response::ok(req.id, Reply::Done)).unwrap();
        assert_eq!(value, json!({"id": "7", "result": null}));
        let req = parse_request(r#"{"id":-2.5,"method":"ping"}"#).unwrap();
        assert_eq!(serde_json::to_value(&req.id).unwrap(), json!(-2.5));
    }

    #[test]
    fn unreadable_ids_are_answered_with_null() {
        for line in [
            r#"{"id":{"x":1},"method":"ping"}"#,
            r#"{"id":true,"method":"ping"}"#,
            r#"{"method":"ping"}"#,
            "[1]",
            "{not json",
        ] {
            let err = parse_request(line).unwrap_err();
            assert_eq!(err.id, None, "{line}");
            let value = serde_json::to_value(err.response()).unwrap();
            assert_eq!(value["id"], Value::Null, "{line}");
            assert!(value["error"]["code"].is_i64());
        }
    }

    #[test]
    fn parse_request_reports_each_kind_of_error() {
        let err = parse_request("{not json").unwrap_err();
        assert_eq!((err.id, err.error.code), (None, code::PARSE_ERROR));

        let err = parse_request(r#"{"method":"ping"}"#).unwrap_err();
        assert_eq!((err.id, err.error.code), (None, code::INVALID_REQUEST));

        let err = parse_request(r#"{"id":3}"#).unwrap_err();
        assert_eq!(
            (err.id, err.error.code),
            (Some(3.into()), code::INVALID_REQUEST)
        );

        let err = parse_request(r#"{"id":4,"method":"explode"}"#).unwrap_err();
        assert_eq!(
            (err.id, err.error.code),
            (Some(4.into()), code::METHOD_NOT_FOUND)
        );

        let err = parse_request(r#"{"id":5,"method":"getRepo","params":{}}"#).unwrap_err();
        assert_eq!(
            (err.id, err.error.code),
            (Some(5.into()), code::INVALID_PARAMS)
        );

        let err = parse_request(r#"{"id":6,"method":"ping","params":{"x":1}}"#).unwrap_err();
        assert_eq!(
            (err.id, err.error.code),
            (Some(6.into()), code::INVALID_PARAMS)
        );
    }

    #[test]
    fn remote_urls_parse() {
        let expect = |host: &str, owner: &str, name: &str| {
            Some(RemoteId {
                host: host.into(),
                owner: owner.into(),
                name: name.into(),
            })
        };
        assert_eq!(
            RemoteId::parse("https://github.com/dev/tidepool-api.git"),
            expect("github.com", "dev", "tidepool-api")
        );
        assert_eq!(
            RemoteId::parse("https://token@GitHub.com/dev/tidepool-api/"),
            expect("github.com", "dev", "tidepool-api")
        );
        assert_eq!(
            RemoteId::parse("git@github.com:dev/tidepool-api.git"),
            expect("github.com", "dev", "tidepool-api")
        );
        assert_eq!(
            RemoteId::parse("ssh://git@git.example.com:2222/example-group/app/aurora-notes.git"),
            expect("git.example.com", "example-group/app", "aurora-notes")
        );
    }

    #[test]
    fn local_and_odd_urls_do_not_parse() {
        assert_eq!(RemoteId::parse("/srv/git/tidepool-api.git"), None);
        assert_eq!(RemoteId::parse("../tidepool-api"), None);
        assert_eq!(RemoteId::parse("file:///srv/git/tidepool-api.git"), None);
        assert_eq!(RemoteId::parse("https://github.com/tidepool-api"), None);
        assert_eq!(RemoteId::parse(""), None);
    }
}
