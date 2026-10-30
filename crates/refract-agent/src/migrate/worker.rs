//! One migration on its own thread: preflight, copy, normalise, verify until the source
//! holds still, place, install, move history, report; then the follow-up requests
//! (`migrateRecheck`, `migrateBeginDelete`, `migrateResync`, `migrateFinish`,
//! `migrateCancel`) until it ends.
//!
//! What may happen after the report is guarded by what is on disk, never by the report
//! alone: the placed copy's stat as last checked is kept (`Record::copy_stats`), and
//! `migrateRecheck`, `migrateBeginDelete`, `migrateResync` and `discard` compare the
//! copy with it, so a copy that changed or went is never trusted, re-synced over or
//! deleted. A source that cannot be read is never taken for one whose files are gone.
//! Once `migrateBeginDelete` passes, the migration is *deleting* (saved in its state
//! file): only `migrateFinish delete` ends it.
//!
//! Once the copy is placed it is never deleted by anything but `migrateFinish discard`
//! and its guards: a cancel or a panic after that point stops the step at hand and
//! ends in a report. Each step that changes the placed copy saves [`Record::running`]
//! before it starts, so a helper that stopped mid-step reports the step as cut short
//! after a restart ([`Worker::recover`]) instead of trusting what it left.
//!
//! The main thread talks to it through a [`Handle`]: commands in, [`Update`]s out
//! through the sink. Cancelling while it runs sets a flag the long steps check between
//! files, then sends [`Command::Cancel`], which the worker answers once it has deleted
//! what it made (or, once placed, once the report is out). The worker reads commands
//! until the main thread drops the handle, so none sent in the meantime goes
//! unanswered.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::process::Command as Process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use refract_core::migrate::gitstate::{self, StatusKind};
use refract_core::migrate::manifest::{
    carry_unreadable, compare_readable, diff, ManifestDiff, RelPath, StatChanges, StatEntry,
};
use refract_core::migrate::preflight::{self, GitOperation, PreflightFacts};
use refract_core::migrate::report::{
    manifest_checks, Check, CheckResult, CheckStatus, InstallChanges, InstallResult, Problem,
    Report, REPORT_VERSION,
};
use refract_core::migrate::{SourceSnapshot, SOURCE_SNAPSHOT_VERSION};
use refract_core::protocol::{
    code, BeginDelete, CancelOutcome, Event, FinishAction, Forced, Id, MigrateParams,
    MigrateProgress, MigrateStep, MigrationTicket, Recheck, Reply, Response,
};
use refract_core::sessions::{is_windows_mount, path_within};

use super::store::{self, HistoryPlan, Record, Running};
use super::tree::{self, Cancel, Cancelled, Stats};
use super::{fsx, gitx, history, install, source};
use crate::{git, procs, state};

/// Tests set this to a shell script the worker runs at fixed points, with the point,
/// the round, the copy and the source as arguments: `copied` after the first copy,
/// before normalising; `verified` after each round's checks, before the source is
/// statted again; `placed` once the copy is in place, before the install; `moved` after
/// the history is copied, before its result is saved and the report sent.
pub const TEST_HOOK: &str = "REFRACT_AGENT_TEST_MIGRATE_HOOK";

/// Rounds of re-copying a changing source before check 4 fails (spec §5).
pub const MAX_ROUNDS: u64 = 3;

/// Least time between two progress events of one step.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// Refusal while the original is being deleted.
pub const DELETING: &str = "The original is being deleted.";

/// Check 4's message for a re-sync that was cut short.
const RESYNC_CUT: &str = "The copy is part way between the original as it was checked and \
    as it is now. Re-sync again to finish it, or keep both, or discard the copy.";

/// Check 1's message for a file [`gitx::Normalised::kept`] whose blob then differs.
const KEPT: &str = "Kept byte for byte: this tracked file mixes line endings (or a git \
    filter changes it), so a Linux checkout of it would not have the same bytes, and git \
    in WSL shows it as modified. The original can't be deleted while it differs. On \
    Windows, give it one kind of line ending throughout (or restore it with git checkout \
    -- <file>), then retry.";

/// What the main thread asks of a migration; each carries the request to answer.
#[derive(Debug)]
pub enum Command {
    Cancel(Id),
    Recheck(Id),
    /// `migrateBeginDelete` (see its params for `source_gone` and `force`).
    BeginDelete {
        request: Id,
        source_gone: bool,
        force: Forced,
    },
    Resync(Id),
    Finish(Id, FinishAction),
}

/// What a migration tells the main thread.
#[derive(Debug)]
pub enum Update {
    /// A response to write.
    Answer(Response),
    /// An event to write.
    Event(Event),
    /// `migrateFinish` is done on the worker's side; the main thread updates the
    /// config, and only then removes the state file and drops the handle (which ends
    /// the worker), and answers `request`. If the config can't be written it answers
    /// the error and keeps both; the same `migrateFinish` again sends this again.
    Finished {
        request: Id,
        id: String,
        action: FinishAction,
        source: PathBuf,
        dest: PathBuf,
    },
    /// The migration is over (cancelled before it was placed, or refused at the
    /// start): forget it, dropping the handle, which lets the worker end. Sent before
    /// the worker answers the request that ended it.
    Ended { id: String },
    /// `migrateBeginDelete` passed: the migration is deleting from now on.
    Deleting { id: String },
    /// A `migrateResync` was refused: the migration is ready again, not re-syncing.
    Idle { id: String },
}

/// Where the main thread's [`Update`]s go. Returns false once nobody listens.
pub type Sink = Arc<dyn Fn(Update) -> bool + Send + Sync>;

/// Where a migration is, as the main thread tracks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Before its first report.
    Running,
    /// Its report is out and it waits for the app.
    Ready,
    /// A `migrateResync` is running.
    Resyncing,
}

/// The main thread's end of a migration.
pub struct Handle {
    pub id: String,
    pub source: String,
    pub dest: String,
    pub phase: Phase,
    /// The latest report, while [`Phase::Ready`].
    pub report: Option<Box<Report>>,
    /// In the deleting state (see [`DELETING`]).
    pub deleting: bool,
    commands: Sender<Command>,
    cancel: Arc<AtomicBool>,
}

impl Handle {
    /// Hands `command` to the worker. False when the worker is gone.
    pub fn send(&self, command: Command) -> bool {
        self.commands.send(command).is_ok()
    }

    /// Stops the running steps, then hands over the cancel request.
    pub fn cancel(&self, request: Id) -> bool {
        self.cancel.store(true, Ordering::Relaxed);
        self.send(Command::Cancel(request))
    }
}

/// What a migration needs from the helper's config.
#[derive(Debug, Clone)]
pub struct Context {
    /// Claude Code's `projects` folder.
    pub claude_projects: PathBuf,
}

/// Starts a migration for a `migrate` request on its own thread. The worker answers
/// `request` itself: with the ticket once preflight passes, or with the refusals.
pub fn start(
    id: String,
    request: Id,
    params: MigrateParams,
    ctx: Context,
    sink: Sink,
) -> io::Result<Handle> {
    let (commands, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let handle = Handle {
        id: id.clone(),
        source: trim(&params.path),
        dest: trim(&params.dest),
        phase: Phase::Running,
        report: None,
        deleting: false,
        commands,
        cancel: cancel.clone(),
    };
    let worker = Worker::new(
        id,
        PathBuf::from(&handle.source),
        PathBuf::from(&handle.dest),
        ctx,
        cancel,
        sink,
    );
    std::thread::Builder::new()
        .name("migrate".into())
        .spawn(move || worker.run_new(request, params.snapshot, rx))?;
    Ok(handle)
}

/// Picks up a migration placed before the helper restarted: its worker sends its
/// report again as `migrateReady` and waits for the app. A record naming paths it
/// should not ([`Record::check`]) is refused. A report that allowed deleting while
/// the copy is gone no longer does.
pub fn resume(record: Record, ctx: Context, sink: Sink) -> io::Result<Handle> {
    record
        .check(&ctx.claude_projects)
        .map_err(|e| io::Error::other(format!("its state file is not valid: {e}")))?;
    let (commands, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let handle = Handle {
        id: record.id.clone(),
        source: record.source.clone(),
        dest: record.dest.clone(),
        phase: Phase::Ready,
        report: Some(Box::new(record.report.clone())),
        deleting: record.deleting,
        commands,
        cancel: cancel.clone(),
    };
    let mut worker = Worker::new(
        record.id.clone(),
        PathBuf::from(&record.source),
        PathBuf::from(&record.dest),
        ctx,
        cancel,
        sink,
    );
    std::thread::Builder::new()
        .name("migrate".into())
        .spawn(move || {
            worker.snapshot = Some(record.snapshot.clone());
            worker.started_at = record.report.started_at;
            worker.report_path = record.report_path.clone();
            worker.placed = true;
            worker.progress.placed = true;
            worker.deleting = record.deleting;
            worker.install_changes = record.report.install_changes.clone();
            worker.recovered_changes = record.report.recovered_changes.clone();
            worker.install_backup = record.report.install_backup.clone();
            worker.copy_errors = record
                .copy_errors
                .iter()
                .map(|p| (p.path.clone().unwrap_or_default(), p.clone()))
                .collect();
            worker.kept = record.kept.iter().cloned().collect();
            let running = record.running;
            let mut report = record.report.clone();
            worker.record = Some(record);
            if running.is_some() {
                report = worker.recover("the helper stopped while it ran");
            }
            report.placed = true;
            if !worker.deleting && report.can_delete() && !worker.dest.is_dir() {
                let gone = Problem {
                    path: None,
                    message: format!(
                        "The copy at {} is gone, so the original must stay.",
                        worker.dest_str()
                    ),
                };
                for check in &mut report.checks {
                    if check.check == Check::Files {
                        *check = with_problems(check, vec![gone.clone()]);
                    }
                }
                worker.save_report(&report);
                worker.save_record_report(&report);
            }
            worker.ready(report);
            worker.serve(rx);
        })?;
    Ok(handle)
}

fn trim(path: &str) -> String {
    match path.strip_suffix('/') {
        Some(trimmed) if !trimmed.is_empty() => trimmed.to_owned(),
        _ => path.to_owned(),
    }
}

/// Why the pipeline stopped early.
enum Stop {
    Cancelled,
    /// Something it cannot go on from; the report says what.
    Fatal(String),
}

impl From<Cancelled> for Stop {
    fn from(_: Cancelled) -> Stop {
        Stop::Cancelled
    }
}

/// Sends progress events, at most one per [`PROGRESS_EVERY`] for a step, always the
/// first and the last.
struct Progress {
    id: String,
    source: String,
    dest: String,
    sink: Sink,
    step: Option<MigrateStep>,
    last: Instant,
    /// The copy is in place (sent with each event).
    placed: bool,
}

impl Progress {
    fn report(&mut self, step: MigrateStep, done: u64, total: u64) {
        let now = Instant::now();
        let same = self.step == Some(step);
        if same && done < total && now - self.last < PROGRESS_EVERY {
            return;
        }
        self.step = Some(step);
        self.last = now;
        (self.sink)(Update::Event(Event::MigrateProgress(MigrateProgress {
            id: self.id.clone(),
            source: self.source.clone(),
            dest: self.dest.clone(),
            step,
            done,
            total,
            placed: self.placed,
        })));
    }
}

/// The placed copy against its last check ([`Worker::copy_check`]).
#[derive(Default)]
struct CopyCheck {
    /// Every path that differs or could not be read; `["."]` when the copy is gone.
    changed: Vec<String>,
    /// Of those, the paths gone from the copy.
    removed: Vec<String>,
    /// Of those, the paths that could not be read (`"."` for the folder itself).
    unreadable: Vec<String>,
    /// The copy's folder is gone.
    missing: bool,
}

/// The checks 1 to 4 of one verification.
struct Verdict {
    checks: [CheckResult; 4],
    diff: ManifestDiff,
    /// The source's stat at the end, for `migrateRecheck`.
    final_stats: Vec<StatEntry>,
}

struct Worker {
    id: String,
    source: PathBuf,
    dest: PathBuf,
    ctx: Context,
    cancel: Arc<AtomicBool>,
    sink: Sink,
    progress: Progress,
    /// The source as the checks last saw it.
    snapshot: Option<SourceSnapshot>,
    started_at: DateTime<Utc>,
    report_path: PathBuf,
    /// The temporary copy, until placed.
    temp: Option<PathBuf>,
    /// The copy is at `dest`.
    placed: bool,
    record: Option<Record>,
    /// The last report sent.
    report: Option<Report>,
    /// Copy problems by path (`""` for none), replaced when a path is copied again.
    copy_errors: BTreeMap<String, Problem>,
    warnings: Vec<String>,
    /// What the install did to the placed copy, allowed in later checks.
    install_changes: InstallChanges,
    /// What changed in the copy while an install was cut short ([`Worker::recover`]):
    /// maybe the user's work, so discard and re-sync are refused while it is not empty.
    recovered_changes: InstallChanges,
    /// Where the original's versions of the files the install changed are.
    install_backup: Option<String>,
    /// Tracked files kept byte for byte when normalised, by path from the top.
    kept: BTreeSet<String>,
    /// `migrateBeginDelete` passed.
    deleting: bool,
    /// While a re-sync runs: the paths it copied over or removed in the placed copy.
    written: Option<BTreeSet<String>>,
    /// `migrateFinish` did its part with this action; the main thread may still fail
    /// to write the config, and the same request again repeats only the answer.
    finished: Option<FinishAction>,
}

impl Worker {
    fn new(
        id: String,
        source: PathBuf,
        dest: PathBuf,
        ctx: Context,
        cancel: Arc<AtomicBool>,
        sink: Sink,
    ) -> Worker {
        let progress = Progress {
            id: id.clone(),
            source: state::path_str(&source),
            dest: state::path_str(&dest),
            sink: sink.clone(),
            step: None,
            last: Instant::now(),
            placed: false,
        };
        Worker {
            id,
            source,
            dest,
            ctx,
            cancel,
            sink,
            progress,
            snapshot: None,
            started_at: Utc::now(),
            report_path: PathBuf::new(),
            temp: None,
            placed: false,
            record: None,
            report: None,
            copy_errors: BTreeMap::new(),
            warnings: Vec::new(),
            install_changes: InstallChanges::default(),
            recovered_changes: InstallChanges::default(),
            install_backup: None,
            kept: BTreeSet::new(),
            deleting: false,
            written: None,
            finished: None,
        }
    }

    fn send(&self, update: Update) {
        (self.sink)(update);
    }

    fn answer(&self, response: Response) {
        self.send(Update::Answer(response));
    }

    fn snapshot(&self) -> &SourceSnapshot {
        self.snapshot.as_ref().expect("snapshot read")
    }

    fn skipped(&self) -> Vec<RelPath> {
        self.snapshot()
            .manifest
            .skipped
            .iter()
            .map(|s| s.path.clone())
            .collect()
    }

    fn source_str(&self) -> String {
        state::path_str(&self.source)
    }

    fn dest_str(&self) -> String {
        state::path_str(&self.dest)
    }

    /// A `migrate` request: preflight, then the pipeline, then the follow-ups.
    fn run_new(mut self, request: Id, snapshot_path: String, rx: Receiver<Command>) {
        let prepared = catch_unwind(AssertUnwindSafe(|| self.prepare(&snapshot_path)))
            .unwrap_or_else(|_| Err("internal error".into()));
        match prepared {
            Ok(snapshot) => {
                self.snapshot = Some(snapshot);
                self.answer(Response::ok(
                    request,
                    Reply::Migration(MigrationTicket {
                        id: self.id.clone(),
                    }),
                ));
            }
            Err(message) => {
                self.send(Update::Ended {
                    id: self.id.clone(),
                });
                self.answer(Response::err(request, code::MIGRATE_FAILED, message));
                self.drain(&rx, "the migration could not start");
                return;
            }
        }
        let outcome = catch_unwind(AssertUnwindSafe(|| self.pipeline()))
            .unwrap_or_else(|_| Err(Stop::Fatal("internal error".into())));
        match outcome {
            Ok(()) => {}
            Err(_) if self.placed => {
                // Only a panic gets here once placed: the copy stays, and the report
                // says which step was cut short.
                let report = self.recover("an internal error stopped it");
                self.ready(report);
            }
            Err(Stop::Cancelled) => {
                self.remove_temp();
                // Forgotten before the cancel is answered, so a request sent after the
                // answer finds no migration rather than a worker that stopped reading.
                self.send(Update::Ended {
                    id: self.id.clone(),
                });
                self.drain(&rx, "the migration was cancelled");
                return;
            }
            Err(Stop::Fatal(message)) => {
                self.remove_temp();
                let mut checks = not_run_checks();
                checks[2] = CheckResult::from_problems(
                    Check::NothingSkipped,
                    vec![Problem {
                        path: None,
                        message: format!("The copy stopped: {message}"),
                    }],
                );
                let mut checks = checks.to_vec();
                checks.push(CheckResult::not_run(Check::History));
                let report = self.build_report(checks, None, vec![], Default::default());
                self.save_report(&report);
                self.ready(report);
            }
        }
        self.serve(rx);
    }

    /// After [`Update::Ended`]: answers what is still sent until the main thread drops
    /// the handle. A cancel (the one that ended it, or a repeat) is answered with
    /// `ended: true`, anything else refused with `why`.
    fn drain(&self, rx: &Receiver<Command>, why: &str) {
        for command in rx.iter() {
            match command {
                Command::Cancel(request) => self.answer(Response::ok(
                    request,
                    Reply::Cancelled(CancelOutcome { ended: true }),
                )),
                other => self.refuse(other, why),
            }
        }
    }

    /// Reads the snapshot file and runs the Linux side of preflight (spec §4.1).
    fn prepare(&mut self, snapshot_path: &str) -> Result<SourceSnapshot, String> {
        let bytes = fs::read(snapshot_path)
            .map_err(|e| format!("Cannot read the snapshot file {snapshot_path}: {e}."))?;
        let snapshot: SourceSnapshot = serde_json::from_slice(&bytes)
            .map_err(|e| format!("The snapshot file {snapshot_path} is not valid: {e}."))?;
        if snapshot.version != SOURCE_SNAPSHOT_VERSION {
            return Err(format!(
                "The snapshot file has version {}, not {SOURCE_SNAPSHOT_VERSION}.",
                snapshot.version
            ));
        }
        let plain = |p: &Path| {
            p.is_absolute()
                && p.components()
                    .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        };
        if !plain(&self.source) || !plain(&self.dest) {
            return Err(
                "The source and destination must be absolute Linux paths without . or ...".into(),
            );
        }
        store::check_snapshot_paths(&snapshot)?;
        let facts = self.facts(&snapshot);
        let refusals = preflight::refusals(&facts);
        if !refusals.is_empty() {
            let messages: Vec<String> = refusals.iter().map(|r| r.message()).collect();
            return Err(messages.join("\n"));
        }
        Ok(snapshot)
    }

    fn facts(&self, snapshot: &SourceSnapshot) -> PreflightFacts {
        let git = self.source.join(".git");
        let git_meta = fs::symlink_metadata(&git).ok();
        let operation = [
            ("MERGE_HEAD", GitOperation::Merge),
            ("rebase-merge", GitOperation::Rebase),
            ("rebase-apply", GitOperation::Rebase),
            ("CHERRY_PICK_HEAD", GitOperation::CherryPick),
            ("REVERT_HEAD", GitOperation::Revert),
            ("BISECT_LOG", GitOperation::Bisect),
        ]
        .into_iter()
        .find(|(name, _)| git.join(name).exists())
        .map(|(_, op)| op);
        let uses_lfs = git.join("lfs").is_dir()
            || fs::read_to_string(self.source.join(".gitattributes"))
                .is_ok_and(|text| text.contains("filter=lfs"));
        let dest_on_linux = on_linux_fs(&self.dest);
        PreflightFacts {
            is_git_repo: git_meta.is_some(),
            on_windows_drive: git::on_windows_drive(&self.source)
                || state::under_test_slow_prefix(&self.source),
            is_linked_worktree: git_meta.as_ref().is_some_and(|m| m.is_file()),
            has_linked_worktrees: fs::read_dir(git.join("worktrees"))
                .is_ok_and(|mut entries| entries.next().is_some()),
            operation,
            external_links: external_links(snapshot),
            // The app checks what it can't open; the copy reports the rest (check 3).
            unreadable: Vec::new(),
            uses_lfs,
            wsl_has_git_lfs: uses_lfs && gitx::has_git_lfs(),
            dest_on_linux,
            dest_exists: fs::symlink_metadata(&self.dest).is_ok(),
            copy_bytes: snapshot.manifest.totals().bytes,
            free_bytes: fsx::free_bytes(&self.dest).unwrap_or(0),
        }
    }

    fn check_cancel(&self) -> Result<(), Stop> {
        if self.cancel.cancelled() {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Copy, normalise, verify, place, install, move history, report.
    fn pipeline(&mut self) -> Result<(), Stop> {
        let name = self
            .source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".into());
        (self.report_path, _) = store::report_paths(self.started_at, &name);
        let parent = self
            .dest
            .parent()
            .ok_or_else(|| Stop::Fatal("The destination has no parent folder.".into()))?
            .to_owned();
        fs::create_dir_all(&parent)
            .map_err(|e| Stop::Fatal(format!("Cannot create {}: {e}.", parent.display())))?;
        let temp = parent.join(format!("{}{name}-{}", store::TEMP_PREFIX, self.id));
        store::mark_temp(&self.id, &temp).map_err(Stop::Fatal)?;
        fs::create_dir(&temp)
            .and_then(|()| fs::set_permissions(&temp, fs::Permissions::from_mode(0o755)))
            .map_err(|e| Stop::Fatal(format!("Cannot create {}: {e}.", temp.display())))?;
        self.temp = Some(temp.clone());

        let skipped = self.skipped();
        let before = tree::stats(&self.source, &skipped, &*self.cancel)?;
        let unreadable = before.unreadable_paths();
        let known = tree::manifest_stats(&self.snapshot().manifest);
        let pending = compare_readable(&known, &before.entries, &unreadable);
        let before = carry_unreadable(&known, &before.entries, &unreadable);
        let total = self.snapshot().manifest.totals().bytes;
        self.progress.report(MigrateStep::Copying, 0, total);
        let progress = &mut self.progress;
        let errors = tree::copy_tree(&self.source, &temp, &skipped, &*self.cancel, &mut |done| {
            progress.report(MigrateStep::Copying, done.min(total), total)
        })?;
        self.progress.report(MigrateStep::Copying, total, total);
        self.add_copy_errors(&StatChanges::default(), errors);
        self.test_hook("copied", 0, &temp);
        self.normalise(&temp, None)?;

        let verdict = self.verify_rounds(&temp, before, pending)?;
        let passed = verdict
            .checks
            .iter()
            .all(|c| c.status == CheckStatus::Passed);
        if !passed {
            self.remove_temp();
            let mut checks = verdict.checks.to_vec();
            checks.push(CheckResult::not_run(Check::History));
            let report = self.build_report(checks, Some(&verdict.diff), vec![], Default::default());
            self.save_report(&report);
            self.ready(report);
            return Ok(());
        }

        self.check_cancel()?;
        if let Err(e) = fsx::rename_noreplace(&temp, &self.dest) {
            let message = if e.kind() == io::ErrorKind::AlreadyExists {
                format!(
                    "Something appeared at {} during the copy.",
                    self.dest.display()
                )
            } else {
                format!("Cannot move the copy to {}: {e}.", self.dest.display())
            };
            return Err(Stop::Fatal(message));
        }
        self.placed = true;
        self.progress.placed = true;
        self.temp = None;
        let report = self.build_report(
            not_moved(&verdict.checks),
            Some(&verdict.diff),
            vec![],
            Default::default(),
        );
        let mut record = Record::new(
            &self.id,
            report,
            self.report_path.clone(),
            self.snapshot().clone(),
            verdict.final_stats.clone(),
        );
        // Until the install has run, the copy as checked is the one to compare with.
        record.copy_stats = self.copy_state(&self.dest).entries;
        record.running = Some(Running::Installing);
        self.record = Some(record);
        self.save_record();
        store::unmark_temp(&self.id);
        self.finish_placed(verdict);
        Ok(())
    }

    /// After the copy is placed: install, history, report. Nothing here deletes the
    /// copy. A cancel stops the install and skips the history, and the report says so;
    /// the user then answers it like any other.
    fn finish_placed(&mut self, verdict: Verdict) {
        self.test_hook("placed", 0, &self.dest.clone());
        let (install, backup_problems) = self.install();
        let stopped = self.cancel.cancelled();
        if stopped {
            self.warnings.push(
                "You stopped the move once the copy was in place, so the copy was kept and \
                 the rest of the install and history steps didn't run."
                    .into(),
            );
        }
        let mut checks = verdict.checks.clone();
        checks[2] = with_problems(&checks[2], backup_problems);
        let report = self.build_report(
            not_moved(&checks),
            Some(&verdict.diff),
            install.clone(),
            Default::default(),
        );
        let copy_stats = self.copy_state(&self.dest).entries;
        let plan = if stopped {
            history::Plan::default()
        } else {
            history::plan(
                &self.ctx.claude_projects,
                &self.source_str(),
                &self.dest_str(),
            )
        };
        if let Some(record) = &mut self.record {
            record.copy_stats = copy_stats;
            record.report = report;
            record.running = Some(Running::MovingHistory);
            record.history_planned = plan
                .files
                .iter()
                .map(|(original, copy)| HistoryPlan {
                    original: original.clone(),
                    copy: copy.clone(),
                })
                .collect();
        }
        // Saved before anything is written, so that a restart finds the copies.
        self.save_record();
        let history = if stopped {
            history::Copied {
                problems: vec![Problem {
                    path: None,
                    message: "You stopped the move before the session history was moved, \
                              so the old transcripts stay where they are."
                        .into(),
                }],
                ..history::Copied::default()
            }
        } else {
            self.move_history(plan)
        };
        if let Some(record) = &mut self.record {
            record.history = history.files.clone();
            record.history_planned.clear();
        }
        self.test_hook("moved", 0, &self.dest.clone());
        let history_check = CheckResult::from_problems(Check::History, history.problems.clone());
        let mut checks = checks.to_vec();
        checks.push(history_check);
        let report = self.build_report(checks, Some(&verdict.diff), install, history.moved);
        if let Some(record) = &mut self.record {
            record.running = None;
        }
        self.save_record_report(&report);
        self.save_report(&report);
        self.ready(report);
    }

    /// A placed migration whose step [`Record::running`] was cut short (`why`: the
    /// helper stopped, or a panic): makes its report from what is on disk, saves it and
    /// returns it. The copy is never deleted here.
    ///
    /// - The install: what changed in the copy since it was placed may be the install's
    ///   or the user's own edits made while the helper was down, so it is kept apart
    ///   (`recovered_changes`): allowed in later checks, the original's versions backed
    ///   up as for an install, but never discarded or re-synced over. Each step is
    ///   marked cancelled; the history did not move.
    /// - The history: the copies that were written, unchanged since, are taken as this
    ///   migration's ([`history::adopt`]), so a discard deletes them; the history check
    ///   fails, so the old transcripts are never deleted.
    /// - A re-sync: only what it wrote counts as its doing ([`Worker::resync_stats`]);
    ///   check 4 fails, so the original can't go before a re-sync runs to its end.
    fn recover(&mut self, why: &str) -> Report {
        let Some(record) = self.record.clone() else {
            // Placed but not yet recorded: nothing is known of the copy, so nothing
            // may happen to it (a discard finds no record and leaves it).
            let mut checks = not_run_checks().to_vec();
            checks[2] = CheckResult::from_problems(
                Check::NothingSkipped,
                vec![Problem {
                    path: None,
                    message: format!(
                        "The move stopped right after the copy was put in place ({why})."
                    ),
                }],
            );
            checks.push(CheckResult::not_run(Check::History));
            return self.build_report(checks, None, vec![], Default::default());
        };
        let mut report = record.report.clone();
        report.finished_at = Utc::now();
        report.placed = true;
        let history_problem = |what: &str| Problem {
            path: None,
            message: format!(
                "The session history {what} ({why}), so the old transcripts stay where they are."
            ),
        };
        let set_check = |report: &mut Report, result: CheckResult| match report
            .checks
            .iter_mut()
            .find(|c| c.check == result.check)
        {
            Some(check) => *check = result,
            None => report.checks.push(result),
        };
        match record.running {
            Some(Running::Installing) => {
                let folders = self.snapshot().manifest.skipped.clone();
                let (_, log) = store::report_paths(self.started_at, &file_name(&self.source));
                report.install = install::plan(&self.dest, &folders)
                    .into_iter()
                    .map(|step| InstallResult {
                        step,
                        status: refract_core::migrate::report::InstallStatus::Cancelled,
                        exit_code: None,
                        log_path: Some(state::path_str(&log)),
                    })
                    .collect();
                let now = self.copy_state(&self.dest);
                let changes =
                    compare_readable(&record.copy_stats, &now.entries, &now.unreadable_paths());
                let problems = self.take_install_changes(&changes, &now.unreadable, true);
                report.install_changes = self.install_changes.clone();
                report.recovered_changes = self.recovered_changes.clone();
                report.install_backup = self.install_backup.clone();
                if let Some(files) = report
                    .checks
                    .iter()
                    .find(|c| c.check == Check::NothingSkipped)
                {
                    let files = with_problems(files, problems);
                    set_check(&mut report, files);
                }
                report.warnings.push(format!(
                    "The install didn't finish ({why}). Run it again in the new folder."
                ));
                let history = CheckResult::from_problems(
                    Check::History,
                    vec![history_problem("wasn't moved")],
                );
                set_check(&mut report, history);
                if let Some(r) = &mut self.record {
                    r.copy_stats = now.entries;
                }
            }
            Some(Running::MovingHistory) => {
                let planned: Vec<(PathBuf, PathBuf)> = record
                    .history_planned
                    .iter()
                    .map(|p| (p.original.clone(), p.copy.clone()))
                    .collect();
                let files = history::adopt(&planned, &self.source_str(), &self.dest_str());
                report.history.sessions = files
                    .iter()
                    .filter(|f| f.original.extension().is_some_and(|e| e == "jsonl"))
                    .count() as u32;
                let history = CheckResult::from_problems(
                    Check::History,
                    vec![history_problem("didn't finish moving")],
                );
                set_check(&mut report, history);
                if let Some(r) = &mut self.record {
                    r.history = files;
                }
            }
            Some(Running::Resyncing) => {
                let written: BTreeSet<String> = record.resync_written.iter().cloned().collect();
                let copy_stats = self.resync_stats(&record.copy_stats, &written);
                let history = report
                    .checks
                    .iter()
                    .find(|c| c.check == Check::History)
                    .cloned();
                let mut checks = not_run_checks().to_vec();
                checks[3] = CheckResult::from_problems(
                    Check::Unchanged,
                    vec![Problem {
                        path: None,
                        message: format!("The re-sync didn't finish ({why}). {RESYNC_CUT}"),
                    }],
                );
                checks.extend(history);
                report.checks = checks;
                if let Some(r) = &mut self.record {
                    r.copy_stats = copy_stats;
                }
            }
            None => {}
        }
        if let Some(r) = &mut self.record {
            r.running = None;
            r.resync_written.clear();
            r.history_planned.clear();
        }
        self.save_record_report(&report);
        self.save_report(&report);
        report
    }

    /// Saves the record as it is, with the copy problems and kept files.
    fn save_record(&mut self) {
        let copy_errors: Vec<Problem> = self.copy_errors.values().cloned().collect();
        let kept: Vec<String> = self.kept.iter().cloned().collect();
        let deleting = self.deleting;
        if let Some(record) = &mut self.record {
            record.copy_errors = copy_errors;
            record.kept = kept;
            record.deleting = deleting;
            if let Err(e) = store::save_record(record) {
                eprintln!("refract-agent: {e}");
            }
        }
    }

    /// Saves the record with `report` as its report.
    fn save_record_report(&mut self, report: &Report) {
        if let Some(record) = &mut self.record {
            record.report = report.clone();
        }
        self.save_record();
    }

    /// The placed copy's stat facts as [`Record::copy_stats`] keeps them: the tree
    /// outside the skipped folders, each repo's index by its entries
    /// ([`gitx::index_digest`]) rather than its bytes, which any `git status` may
    /// rewrite.
    fn copy_state(&self, root: &Path) -> Stats {
        let skipped = self.skipped();
        let mut stats = tree::stats(root, &skipped, &AtomicBool::new(false)).unwrap_or_default();
        for repo in self.snapshot().git.iter().map(|g| g.path.as_str()) {
            let Some(index) = gitx::index_path(root, repo) else {
                continue;
            };
            let Some(entry) = stats.entries.iter_mut().find(|e| e.path.as_str() == index) else {
                continue;
            };
            let dir = if repo.is_empty() {
                root.to_owned()
            } else {
                root.join(repo)
            };
            match gitx::index_digest(&dir) {
                Ok(digest) => {
                    entry.size = 0;
                    entry.mtime_ns = 0;
                    entry.hash = Some(digest);
                }
                Err(message) => stats.unreadable.push(tree::Unreadable {
                    path: index,
                    message,
                }),
            }
        }
        stats
    }

    /// True when a difference of the copy at `path` is the install's, or was found
    /// after an install that was cut short.
    fn allowed(&self, path: &str) -> bool {
        self.install_changes.allows(path) || self.recovered_changes.allows(path)
    }

    /// The paths of the placed copy that differ from it as last checked
    /// ([`Record::copy_stats`]), or could not be read; `["."]` when it is gone.
    fn copy_changes(&self) -> Vec<String> {
        self.copy_check().changed
    }

    /// [`Worker::copy_changes`], with which of them are gone from the copy and which
    /// could not be read. A copy placed but not yet recorded is unknown: unreadable.
    fn copy_check(&self) -> CopyCheck {
        let Some(record) = &self.record else {
            return CopyCheck {
                changed: vec![".".into()],
                unreadable: vec![".".into()],
                ..CopyCheck::default()
            };
        };
        if self.copy_missing() {
            return CopyCheck {
                changed: vec![".".into()],
                missing: true,
                ..CopyCheck::default()
            };
        }
        let now = self.copy_state(&self.dest);
        let unreadable: Vec<String> = {
            let mut paths: Vec<String> = now.unreadable_paths().into_iter().map(dot).collect();
            paths.sort();
            paths.dedup();
            paths
        };
        let changes = compare_readable(&record.copy_stats, &now.entries, &now.unreadable_paths());
        let removed = changes
            .removed
            .iter()
            .map(|p| p.as_str().to_owned())
            .collect();
        let mut changed = changed_paths(&changes);
        changed.extend(unreadable.iter().cloned());
        changed.sort();
        changed.dedup();
        CopyCheck {
            changed,
            removed,
            unreadable,
            missing: false,
        }
    }

    /// Check 4 again, and the copy against its last check (`migrateRecheck`). With
    /// `relax` (deleting, or the original gone on both sides) only the copy's changes
    /// that matter for what is left of the original are listed: paths changed or
    /// removed in the copy whose original is still there. A path added to the copy,
    /// or one whose original is already gone, loses nothing when the rest goes. A copy
    /// that is gone (`"."`) is always listed.
    fn recheck(&self, relax: bool) -> Recheck {
        let source_missing = matches!(
            fs::symlink_metadata(&self.source),
            Err(e) if e.kind() == io::ErrorKind::NotFound
        );
        let mut recheck = Recheck {
            source_missing,
            ..Recheck::default()
        };
        if !source_missing {
            let skipped = self.skipped();
            let now =
                tree::stats(&self.source, &skipped, &AtomicBool::new(false)).unwrap_or_default();
            let before = self
                .record
                .as_ref()
                .map(|r| r.final_stats.clone())
                .unwrap_or_default();
            let unreadable = now.unreadable_paths();
            let changes = compare_readable(&before, &now.entries, &unreadable);
            let mut changed: Vec<String> = changes
                .changed
                .iter()
                .chain(&changes.added)
                .map(|p| p.as_str().to_owned())
                .collect();
            changed.sort();
            recheck.changed = changed;
            recheck.removed = changes
                .removed
                .iter()
                .map(|p| p.as_str().to_owned())
                .collect();
            recheck.unreadable = unreadable.into_iter().map(dot).collect();
            recheck.unreadable.sort();
            recheck.unreadable.dedup();
        }
        let copy = self.copy_check();
        recheck.copy_changed = copy.changed;
        recheck.copy_removed = copy.removed;
        recheck.copy_unreadable = copy.unreadable;
        recheck.copy_missing = copy.missing;
        if relax {
            let known: BTreeSet<&str> = self
                .record
                .iter()
                .flat_map(|r| r.copy_stats.iter().map(|e| e.path.as_str()))
                .collect();
            let matters = |path: &String| {
                path == "."
                    || (known.contains(path.as_str())
                        && !matches!(
                            fs::symlink_metadata(self.source.join(path)),
                            Err(e) if e.kind() == io::ErrorKind::NotFound
                        ))
            };
            recheck.copy_changed.retain(matters);
            recheck.copy_removed.retain(matters);
            recheck.copy_unreadable.retain(matters);
        }
        recheck.busy_agents = self.busy_agents();
        recheck
    }

    /// Replaces the copy problems of the paths in `changes` with `errors`.
    fn add_copy_errors(&mut self, changes: &StatChanges, errors: Vec<Problem>) {
        for path in changes
            .added
            .iter()
            .chain(&changes.changed)
            .chain(&changes.removed)
        {
            self.copy_errors.remove(path.as_str());
        }
        for error in errors {
            let key = error.path.clone().unwrap_or_default();
            self.copy_errors.insert(key, error);
        }
    }

    /// Normalises each repo of the copy at `copy` (spec §3.3); with `only`, just those
    /// files (paths relative to the top). Files that fail count as copy problems. A
    /// repo whose index is in `only` (copied again from the source, with Windows stat
    /// data) has all its entries' stat cleared, so the refresh reads them again. A repo
    /// whose git folder lies outside the copy stops it: git would change the original.
    fn normalise(&mut self, copy: &Path, only: Option<&BTreeSet<String>>) -> Result<(), Stop> {
        self.check_cancel()?;
        let repos: Vec<(String, Vec<String>)> = self
            .snapshot()
            .git
            .iter()
            .map(|g| {
                let config = gitx::windows_config(&g.snapshot.config, g.autocrlf.as_deref());
                (g.path.clone(), config)
            })
            .collect();
        let total = match only {
            Some(only) => only.len() as u64,
            None => self
                .snapshot()
                .git
                .iter()
                .map(|g| g.snapshot.index.len() as u64)
                .sum(),
        };
        let mut done = 0;
        self.progress.report(MigrateStep::Normalising, 0, total);
        for (path, windows) in repos {
            let prefix = if path.is_empty() {
                String::new()
            } else {
                format!("{path}/")
            };
            let only_here: Option<BTreeSet<String>> = only.map(|only| {
                only.iter()
                    .filter_map(|p| p.strip_prefix(&prefix).map(str::to_owned))
                    .collect()
            });
            let dir = if path.is_empty() {
                copy.to_owned()
            } else {
                copy.join(&path)
            };
            gitx::git_dir(copy, &path).map_err(Stop::Fatal)?;
            // Its index copied again: which files are tracked may have changed, so all.
            let restat_all = only.is_some_and(|only| {
                gitx::index_path(copy, &path).is_some_and(|index| only.contains(&index))
            });
            let only_here = if restat_all { None } else { only_here };
            let progress = &mut self.progress;
            let normalised =
                gitx::normalise(&dir, &windows, only_here.as_ref(), restat_all, &mut || {
                    done += 1;
                    progress.report(MigrateStep::Normalising, done.min(total), total);
                })
                .map_err(Stop::Fatal)?;
            self.kept.retain(|kept| {
                let Some(rel) = kept.strip_prefix(&prefix) else {
                    return true;
                };
                only_here.as_ref().is_some_and(|only| !only.contains(rel))
            });
            self.kept
                .extend(normalised.kept.iter().map(|file| format!("{prefix}{file}")));
            // A re-sync's: a whole repo normalised again rewrites files no change listed.
            let rewritten: Vec<String> = normalised
                .rewritten
                .iter()
                .map(|file| format!("{prefix}{file}"))
                .collect();
            self.note_written(rewritten.iter().map(String::as_str));
            for (file, message) in normalised.problems {
                let full = format!("{prefix}{file}");
                self.copy_errors.insert(
                    full.clone(),
                    Problem {
                        path: Some(full),
                        message,
                    },
                );
            }
            self.check_cancel()?;
        }
        self.progress.report(MigrateStep::Normalising, total, total);
        Ok(())
    }

    /// Checks the copy at `copy`, then whether the source changed since `before`;
    /// changes are copied again and checked again, up to [`MAX_ROUNDS`] rounds (spec §5
    /// check 4). `pending` holds changes the snapshot does not know of yet.
    /// A path of the source that cannot be read fails check 4 rather than counting as
    /// removed.
    fn verify_rounds(
        &mut self,
        copy: &Path,
        before: Vec<StatEntry>,
        mut pending: StatChanges,
    ) -> Result<Verdict, Stop> {
        let skipped = self.skipped();
        let mut previous = before;
        let mut round = 0;
        loop {
            if !pending.is_empty() {
                let mut snapshot = self.snapshot().clone();
                source::refresh(&mut snapshot, &self.source, &pending).map_err(Stop::Fatal)?;
                self.snapshot = Some(snapshot);
                let note = "The original changed during the copy; Linux git read it again.";
                if !self.warnings.iter().any(|w| w == note) {
                    self.warnings.push(note.into());
                }
            }
            let (files, git, skipped_check, diff) = self.check(copy)?;
            self.test_hook("verified", round, copy);
            let now = tree::stats(&self.source, &skipped, &*self.cancel)?;
            let unreadable = now.unreadable_paths();
            let changes = compare_readable(&previous, &now.entries, &unreadable);
            previous = carry_unreadable(&previous, &now.entries, &unreadable);
            let unknown: Vec<Problem> = now
                .unreadable
                .iter()
                .map(|u| Problem {
                    path: Some(dot(u.path.clone())),
                    message: format!(
                        "It could not be read in the original ({}), so whether it changed is unknown.",
                        u.message
                    ),
                })
                .collect();
            if changes.is_empty() {
                let mut problems = unknown;
                problems.extend(self.busy_problems());
                let unchanged = CheckResult::from_problems(Check::Unchanged, problems);
                return Ok(Verdict {
                    checks: [files, git, skipped_check, unchanged],
                    diff,
                    final_stats: previous,
                });
            }
            round += 1;
            self.progress
                .report(MigrateStep::Rechecking, round.min(MAX_ROUNDS), MAX_ROUNDS);
            if round > MAX_ROUNDS {
                let mut problems: Vec<Problem> = changed_paths(&changes)
                    .into_iter()
                    .map(|path| Problem {
                        path: Some(path),
                        message: "It changed again while the copy was checked.".into(),
                    })
                    .collect();
                problems.extend(unknown);
                problems.extend(self.busy_problems());
                let unchanged = CheckResult::from_problems(Check::Unchanged, problems);
                return Ok(Verdict {
                    checks: [files, git, skipped_check, unchanged],
                    diff,
                    final_stats: previous,
                });
            }
            self.recopy(copy, &changes)?;
            pending = changes;
        }
    }

    /// Copies `changes` of the source into the copy at `copy` again and normalises what
    /// they touched. A repo whose index is among them is normalised whole, and a file
    /// it no longer tracks is copied again as it is in the source, since its copy was
    /// rewritten as a tracked file.
    fn recopy(&mut self, copy: &Path, changes: &StatChanges) -> Result<(), Stop> {
        let listed: BTreeSet<&str> = changes
            .added
            .iter()
            .chain(&changes.changed)
            .chain(&changes.removed)
            .map(RelPath::as_str)
            .collect();
        let repos: Vec<String> = self
            .snapshot()
            .git
            .iter()
            .map(|g| g.path.clone())
            .filter(|repo| {
                gitx::index_path(copy, repo).is_some_and(|i| listed.contains(i.as_str()))
            })
            .collect();
        let tracked = |repo: &str| {
            let dir = if repo.is_empty() {
                copy.to_owned()
            } else {
                copy.join(repo)
            };
            gitx::tracked(&dir, &[]).unwrap_or_default()
        };
        let before: Vec<BTreeSet<String>> = repos.iter().map(|r| tracked(r)).collect();
        self.note_written(listed.iter().copied());
        let errors = tree::recopy(&self.source, copy, changes, &*self.cancel)?;
        self.add_copy_errors(changes, errors);
        self.install_changes.forget(&changed_paths(changes));
        let mut untracked = StatChanges::default();
        for (repo, before) in repos.iter().zip(before) {
            let now = tracked(repo);
            let prefix = if repo.is_empty() {
                String::new()
            } else {
                format!("{repo}/")
            };
            untracked.changed.extend(
                before
                    .difference(&now)
                    .filter_map(|p| RelPath::new(format!("{prefix}{p}")).ok())
                    .filter(|p| !listed.contains(p.as_str())),
            );
        }
        if !untracked.is_empty() {
            self.note_written(untracked.changed.iter().map(RelPath::as_str));
            let errors = tree::recopy(&self.source, copy, &untracked, &*self.cancel)?;
            self.add_copy_errors(&untracked, errors);
        }
        let touched: BTreeSet<String> = changes
            .added
            .iter()
            .chain(&changes.changed)
            .map(|p| p.as_str().to_owned())
            .collect();
        self.normalise(copy, Some(&touched))
    }

    /// Checks 1 to 3 of the copy at `copy` against the snapshot.
    fn check(
        &mut self,
        copy: &Path,
    ) -> Result<(CheckResult, CheckResult, CheckResult, ManifestDiff), Stop> {
        let skipped = self.skipped();
        let repos: Vec<String> = self.snapshot().git.iter().map(|g| g.path.clone()).collect();
        let progress = &mut self.progress;
        let built = tree::manifest(
            copy,
            &skipped,
            &repos,
            &[],
            &*self.cancel,
            &mut |done, total| progress.report(MigrateStep::Hashing, done, total),
        )?;
        let snapshot = self.snapshot().clone();
        let (mut diff, manifest_error) = match built {
            Ok(manifest) => (diff(&snapshot.manifest, &manifest), None),
            Err(e) => (ManifestDiff::default(), Some(e)),
        };
        // What the install did is listed in the report, not failed on.
        diff.problems.retain(|p| !self.allowed(p.path()));
        let copy_errors: Vec<Problem> = self.copy_errors.values().cloned().collect();
        let [mut files, nothing_skipped] = manifest_checks(&diff, &copy_errors);
        for problem in &mut files.problems {
            if problem.path.as_ref().is_some_and(|p| self.kept.contains(p)) {
                problem.message = KEPT.into();
            }
        }
        if let Some(message) = manifest_error {
            files = CheckResult::from_problems(
                Check::Files,
                vec![Problem {
                    path: None,
                    message: format!("The copy could not be read: {message}"),
                }],
            );
        }

        let mut problems = Vec::new();
        let total = snapshot.git.len() as u64;
        for (n, repo) in snapshot.git.iter().enumerate() {
            self.check_cancel()?;
            self.progress
                .report(MigrateStep::CheckingGit, n as u64, total);
            let prefix = if repo.path.is_empty() {
                String::new()
            } else {
                format!("{}/", repo.path)
            };
            let dir = if repo.path.is_empty() {
                copy.to_owned()
            } else {
                copy.join(&repo.path)
            };
            let skipped_here: Vec<RelPath> = if prefix.is_empty() {
                skipped.clone()
            } else {
                skipped
                    .iter()
                    .filter_map(|s| RelPath::new(s.as_str().strip_prefix(&prefix)?).ok())
                    .collect()
            };
            if let Err(message) = gitx::git_dir(copy, &repo.path) {
                problems.push(Problem {
                    path: (!repo.path.is_empty()).then(|| repo.path.clone()),
                    message,
                });
                continue;
            }
            match gitx::snapshot(&dir, &[]) {
                Ok(mut theirs) => {
                    // Build folders reinstalled in a placed copy are not the copy's doing.
                    theirs.status.retain(|e| {
                        let path = e.path.trim_end_matches('/');
                        !(e.kind == StatusKind::Ignored
                            && skipped_here
                                .iter()
                                .any(|s| RelPath::new(path).is_ok_and(|p| p.within(s.as_str()))))
                    });
                    for mut problem in gitstate::compare(&repo.snapshot, &theirs, &skipped_here) {
                        if let Some(path) = &problem.path {
                            problem.path = Some(format!("{prefix}{path}"));
                        }
                        if problem.path.as_deref().is_some_and(|p| self.allowed(p)) {
                            continue;
                        }
                        // Git shows a file kept byte for byte as modified: say why.
                        if problem.path.as_ref().is_some_and(|p| self.kept.contains(p)) {
                            problem.message = KEPT.into();
                        }
                        problems.push(problem);
                    }
                }
                Err(message) => problems.push(Problem {
                    path: (!repo.path.is_empty()).then(|| repo.path.clone()),
                    message: format!("Git could not read the copy: {message}"),
                }),
            }
            let uses_lfs = copy.join(".git/lfs").is_dir()
                || fs::read_to_string(dir.join(".gitattributes"))
                    .is_ok_and(|t| t.contains("filter=lfs"));
            if uses_lfs {
                if let Some(message) = gitx::lfs_fsck(&dir) {
                    problems.push(Problem {
                        path: (!repo.path.is_empty()).then(|| repo.path.clone()),
                        message,
                    });
                }
            }
        }
        self.progress.report(MigrateStep::CheckingGit, total, total);
        let git = CheckResult::from_problems(Check::GitState, problems);
        Ok((files, git, nothing_skipped, diff))
    }

    /// Agents running in the source, as check 4 problems.
    fn busy_problems(&self) -> Vec<Problem> {
        self.busy_agents()
            .into_iter()
            .map(|message| Problem {
                path: None,
                message,
            })
            .collect()
    }

    /// A sentence per `claude` process running in the source.
    fn busy_agents(&self) -> Vec<String> {
        let source = self.source_str();
        procs::scan()
            .into_iter()
            .filter(|p| path_within(&p.cwd, &source))
            .map(|p| match p.session_id {
                Some(session) => format!(
                    "An agent (pid {}, session {session}) is running in {}. Stop it first.",
                    p.pid, p.cwd
                ),
                None => format!(
                    "An agent (pid {}) is running in {}. Stop it first.",
                    p.pid, p.cwd
                ),
            })
            .collect()
    }

    /// Runs the install steps in the placed copy, and records what they did outside the
    /// build folders in `install_changes` (the copy is statted before and after). The
    /// original's version of each file it changed or removed is backed up
    /// ([`install::backup`]); a file that could not be is a copy problem, returned too.
    /// A cancel stops the step that runs and the rest; what they did is recorded all
    /// the same. Its first progress event (sent even with nothing to install) tells the
    /// app the copy is in place.
    fn install(&mut self) -> (Vec<InstallResult>, Vec<Problem>) {
        let skipped = self.snapshot().manifest.skipped.clone();
        let steps = install::plan(&self.dest, &skipped);
        let total = steps.len() as u64;
        self.progress.report(MigrateStep::Installing, 0, total);
        if steps.is_empty() {
            return (Vec::new(), Vec::new());
        }
        let folders = self.skipped();
        // Statted whatever the cancel flag says: the copy is placed, and what the
        // install did must be known.
        let never = AtomicBool::new(false);
        let before = tree::stats(&self.dest, &folders, &never).unwrap_or_default();
        let (_, log) = store::report_paths(self.started_at, &file_name(&self.source));
        let progress = &mut self.progress;
        let results = install::run(
            &self.dest,
            steps,
            &log,
            install::INSTALL_TIMEOUT,
            &*self.cancel,
            &mut |done| progress.report(MigrateStep::Installing, done, total),
        );
        let after = tree::stats(&self.dest, &folders, &never).unwrap_or_default();
        let changes = compare_readable(&before.entries, &after.entries, &after.unreadable_paths());
        let problems = self.take_install_changes(&changes, &after.unreadable, false);
        (results, problems)
    }

    /// Records `changes` to the placed copy as the install's (`install_changes`), or,
    /// `recovered` from an install that was cut short, apart (`recovered_changes`); backs
    /// up the original's version of what they changed or removed either way, and
    /// returns the problems (paths of the copy that could not be read, and files that
    /// could not be backed up), which are copy problems too.
    fn take_install_changes(
        &mut self,
        changes: &StatChanges,
        unreadable: &[tree::Unreadable],
        recovered: bool,
    ) -> Vec<Problem> {
        let found = install::changes(&self.dest, changes);
        let mut problems: Vec<Problem> = unreadable
            .iter()
            .map(|u| Problem {
                path: Some(dot(u.path.clone())),
                message: format!(
                    "The copy could not be read after the install: {}",
                    u.message
                ),
            })
            .collect();
        if !found.changed.is_empty() || !found.removed.is_empty() {
            let backup = store::backup_dir(&self.report_path);
            let final_stats = self
                .record
                .as_ref()
                .map(|r| r.final_stats.clone())
                .unwrap_or_default();
            problems.extend(install::backup(
                &self.source,
                &found,
                &backup,
                &final_stats,
                &self.snapshot().manifest,
            ));
            self.install_backup = Some(state::path_str(&backup));
        }
        if recovered {
            self.recovered_changes = found;
        } else {
            self.install_changes = found;
        }
        for problem in &problems {
            let key = problem.path.clone().unwrap_or_default();
            self.copy_errors.insert(key, problem.clone());
        }
        problems
    }

    fn move_history(&mut self, plan: history::Plan) -> history::Copied {
        let (source, dest) = (self.source_str(), self.dest_str());
        let progress = &mut self.progress;
        history::copy_planned(plan, &source, &dest, &mut |done, total| {
            progress.report(MigrateStep::MovingHistory, done, total)
        })
    }

    fn build_report(
        &self,
        checks: Vec<CheckResult>,
        diff: Option<&ManifestDiff>,
        install: Vec<InstallResult>,
        history: refract_core::migrate::report::HistoryMoved,
    ) -> Report {
        let snapshot = self.snapshot();
        let mut warnings = preflight::windows_tool_warnings(
            snapshot.manifest.entries.iter().map(|e| e.path.as_str()),
        );
        warnings.extend(self.warnings.iter().cloned());
        Report {
            version: REPORT_VERSION,
            id: self.id.clone(),
            source: self.source_str(),
            dest: self.dest_str(),
            started_at: self.started_at,
            finished_at: Utc::now(),
            checks,
            skipped: snapshot.manifest.skipped.clone(),
            dropped_streams: snapshot.manifest.streams.clone(),
            new_objects: diff.map_or(0, |d| d.new_objects.len() as u64),
            source_totals: diff.map_or_else(|| snapshot.manifest.totals(), |d| d.source_totals),
            copy_totals: diff.map_or_else(Default::default, |d| d.copy_totals),
            install,
            install_changes: self.install_changes.clone(),
            recovered_changes: self.recovered_changes.clone(),
            install_backup: self.install_backup.clone(),
            history,
            linux_git: snapshot.linux_git,
            warnings,
            placed: self.placed,
        }
    }

    fn save_report(&self, report: &Report) {
        if let Err(e) = store::save_report(&self.report_path, report) {
            eprintln!("refract-agent: {e}");
        }
    }

    fn ready(&mut self, report: Report) {
        self.report = Some(report.clone());
        self.send(Update::Event(Event::MigrateReady {
            id: self.id.clone(),
            report: Box::new(report),
        }));
    }

    /// Deletes the temporary copy of a migration that was not placed.
    fn remove_temp(&mut self) {
        if let Some(temp) = self.temp.take() {
            let _ = fsx::remove_any(&temp);
        }
        store::unmark_temp(&self.id);
    }

    /// `migrateFinish discard` of a placed copy that passed its guards: deletes the
    /// copy, the transcripts copied for it that nobody wrote to since and the install
    /// backup. The state file stays until the main thread is done ([`Update::Finished`]).
    fn discard_placed(&mut self) {
        let _ = fsx::remove_any(&self.dest);
        self.remove_history_copies();
        if self.install_backup.is_some() {
            let _ = fsx::remove_any(&store::backup_dir(&self.report_path));
        }
        self.placed = false;
    }

    /// The transcript copies made for this migration that nobody wrote to since go;
    /// the originals stay.
    fn remove_history_copies(&self) {
        if let Some(record) = &self.record {
            for kept in history::remove_copies(&self.ctx.claude_projects, &record.history) {
                eprintln!(
                    "refract-agent: kept {}: {}",
                    kept.path.unwrap_or_default(),
                    kept.message
                );
            }
        }
    }

    fn refuse(&self, command: Command, why: &str) {
        let request = match command {
            Command::Cancel(id)
            | Command::Recheck(id)
            | Command::BeginDelete { request: id, .. }
            | Command::Resync(id)
            | Command::Finish(id, _) => id,
        };
        self.answer(Response::err(request, code::MIGRATE_FAILED, why));
    }

    /// Answers follow-up requests until the main thread drops the handle (after a
    /// finish it has written the config for).
    fn serve(mut self, rx: Receiver<Command>) {
        for command in rx.iter() {
            let done = catch_unwind(AssertUnwindSafe(|| self.follow_up(command)));
            if done.is_err() {
                eprintln!("refract-agent: a migration request failed");
            }
        }
    }

    /// True when the original folder itself is gone.
    fn source_missing(&self) -> bool {
        matches!(
            fs::symlink_metadata(&self.source),
            Err(e) if e.kind() == io::ErrorKind::NotFound
        )
    }

    /// True when the placed copy's folder is gone.
    fn copy_missing(&self) -> bool {
        !fs::symlink_metadata(&self.dest).is_ok_and(|m| m.is_dir())
    }

    fn source_gone_message(&self) -> String {
        format!(
            "The original folder {} is gone, so the copy at {} is the only one left.",
            self.source_str(),
            self.dest_str()
        )
    }

    /// Why `migrateRecheck` and `migrateBeginDelete` are refused now, if they are.
    fn recheck_refusal(&self) -> Option<&'static str> {
        if !self.placed {
            return Some("the copy was not placed, so there is nothing to recheck");
        }
        let passes = self.report.as_ref().is_some_and(|r| r.can_delete());
        if !self.deleting && !passes {
            return Some("the report does not allow deleting the original");
        }
        None
    }

    /// Why `doing` (deleting or re-syncing over the copy) is refused while the copy
    /// holds what changed during an install that was cut short, if it is.
    fn recovered_refusal(&self, doing: &str) -> Option<String> {
        let found = &self.recovered_changes;
        let paths: Vec<String> = found
            .created
            .iter()
            .chain(&found.changed)
            .chain(&found.removed)
            .cloned()
            .collect();
        (!paths.is_empty()).then(|| {
            format!(
                "The copy at {} changed while the install was cut short ({}), which may include \
                 your own edits, so Refract won't {doing}. Keep both, or delete the original \
                 (its versions of those files are saved).",
                self.dest_str(),
                sample(&paths)
            )
        })
    }

    /// Saves `step` as running (with nothing written yet) before it touches the copy.
    fn mark_running(&mut self, step: Running) -> Result<(), String> {
        let Some(record) = &mut self.record else {
            return Err("the migration has no state file".into());
        };
        record.running = Some(step);
        record.resync_written.clear();
        store::save_record(record).inspect_err(|_| {
            record.running = None;
        })
    }

    /// Handles one follow-up, answering it even if it panics.
    fn follow_up(&mut self, command: Command) {
        let request = match &command {
            Command::Cancel(id)
            | Command::Recheck(id)
            | Command::BeginDelete { request: id, .. }
            | Command::Resync(id)
            | Command::Finish(id, _) => id.clone(),
        };
        if catch_unwind(AssertUnwindSafe(|| self.handle(command))).is_err() {
            eprintln!("refract-agent: a migration request failed");
            self.answer(Response::err(
                request,
                code::MIGRATE_FAILED,
                "internal error",
            ));
        }
    }

    fn handle(&mut self, command: Command) {
        if let Some(done) = self.finished {
            // Once the delete is finishing, the recheck and `migrateBeginDelete` only
            // read (deleting already), so the app's Finish deleting, which sends
            // `migrateBeginDelete` before `migrateFinish delete`, still works.
            let reads = done == FinishAction::Delete
                && matches!(command, Command::Recheck(_) | Command::BeginDelete { .. });
            if !reads && !matches!(command, Command::Finish(..)) {
                let why = format!(
                    "the migration is finishing ({}); send that migrateFinish again",
                    action_name(done)
                );
                self.refuse(command, &why);
                return;
            }
        }
        match command {
            Command::Recheck(request) => {
                if let Some(why) = self.recheck_refusal() {
                    self.refuse(Command::Recheck(request), why);
                    return;
                }
                let reply = Reply::Recheck(self.recheck(self.deleting));
                self.answer(Response::ok(request, reply));
            }
            Command::BeginDelete {
                request,
                source_gone,
                force,
            } => self.begin_delete(request, source_gone, force),
            Command::Resync(request) => {
                let refusal = if !self.placed {
                    Some("the copy was not placed, so there is nothing to re-sync".to_owned())
                } else if self.deleting {
                    Some(DELETING.to_owned())
                } else if self.source_missing() {
                    Some(format!(
                        "{} It can't be re-synced.",
                        self.source_gone_message()
                    ))
                } else if let Some(why) = self.recovered_refusal("re-sync over it") {
                    Some(why)
                } else {
                    let changed = self.copy_changes();
                    (!changed.is_empty()).then(|| {
                        format!(
                            "The copy at {} changed since it was checked ({}), so Refract won't copy over it.",
                            self.dest_str(),
                            sample(&changed)
                        )
                    })
                };
                // Saved before anything is copied, so a restart knows it was cut short.
                let refusal = refusal.or_else(|| self.mark_running(Running::Resyncing).err());
                if let Some(why) = refusal {
                    self.send(Update::Idle {
                        id: self.id.clone(),
                    });
                    self.refuse(Command::Resync(request), &why);
                    return;
                }
                self.answer(Response::ok(request, Reply::Done));
                self.resync();
            }
            Command::Finish(request, action) => self.finish(request, action),
            Command::Cancel(request) => {
                if self.cancel.swap(false, Ordering::Relaxed) && !self.deleting {
                    // Sent while the copy was placed and finishing, or while a re-sync
                    // ran: that stopped, and its report is out.
                    let reply = Reply::Cancelled(CancelOutcome { ended: false });
                    self.answer(Response::ok(request, reply));
                    return;
                }
                let why = if self.deleting {
                    DELETING
                } else {
                    "the migration's report is out; end it with migrateFinish"
                };
                self.refuse(Command::Cancel(request), why);
            }
        }
    }

    /// `migrateBeginDelete`: the recheck, and the deleting state when it passes. In the
    /// deleting state, and when the original is gone on both sides, a change to the
    /// copy only blocks for a path the original still has ([`Worker::recheck`]), and
    /// in the deleting state `force` skips those it names under how they changed now
    /// (never one that could not be read, nor a copy that is gone). A source only the
    /// helper can't see (`source_gone` false) never passes: nothing of it was checked.
    fn begin_delete(&mut self, request: Id, source_gone: bool, force: Forced) {
        if let Some(why) = self.recheck_refusal() {
            let command = Command::BeginDelete {
                request,
                source_gone,
                force,
            };
            self.refuse(command, why);
            return;
        }
        let relax = self.deleting || (source_gone && self.source_missing());
        let recheck = self.recheck(relax);
        let hidden = recheck.source_missing && !source_gone;
        let forced = |path: &String| {
            let named = if recheck.copy_removed.contains(path) {
                &force.removed
            } else {
                &force.changed
            };
            self.deleting
                && !recheck.copy_missing
                && !recheck.copy_unreadable.contains(path)
                && named.contains(path)
        };
        let copy_ok = recheck.copy_changed.iter().all(forced);
        let ok = recheck.changed.is_empty()
            && recheck.unreadable.is_empty()
            && recheck.busy_agents.is_empty()
            && copy_ok
            && !hidden;
        if ok && !self.deleting {
            self.deleting = true;
            let saved = self.record.as_mut().map(|record| {
                record.deleting = true;
                store::save_record(record)
            });
            if let Some(Err(e)) = saved {
                // Not saved, a restart would forget it: refuse.
                self.deleting = false;
                if let Some(record) = &mut self.record {
                    record.deleting = false;
                }
                let command = Command::BeginDelete {
                    request,
                    source_gone,
                    force,
                };
                self.refuse(command, &e);
                return;
            }
            self.send(Update::Deleting {
                id: self.id.clone(),
            });
        }
        let reply = Reply::BeginDelete(BeginDelete { ok, recheck });
        self.answer(Response::ok(request, reply));
    }

    /// Re-copies what changed since the last check into the placed copy and checks it
    /// again, then sends a fresh report. Only paths verifiably gone from the source
    /// leave the copy. [`Record::running`] is already saved; what it writes is saved
    /// before each batch ([`Worker::note_written`]), and afterwards only that counts as
    /// its doing in the copy's stat, so an edit made in the copy meanwhile still shows
    /// as a change of the copy. A cancel stops it, with a report saying so.
    fn resync(&mut self) {
        let skipped = self.skipped();
        let Some(record) = self.record.clone() else {
            // Not expected once placed; the app still waits for a report, so it gets
            // the last one again.
            if let Some(report) = self.report.clone() {
                self.ready(report);
            }
            return;
        };
        self.written = Some(BTreeSet::new());
        // A panic is reported like any other stop, so the app is never left waiting.
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<Verdict, Stop> {
            let now = tree::stats(&self.source, &skipped, &*self.cancel)?;
            let unreadable = now.unreadable_paths();
            let changes = compare_readable(&record.final_stats, &now.entries, &unreadable);
            let before = carry_unreadable(&record.final_stats, &now.entries, &unreadable);
            self.progress.report(MigrateStep::Rechecking, 0, MAX_ROUNDS);
            let dest = self.dest.clone();
            self.recopy(&dest, &changes)?;
            self.verify_rounds(&dest, before, changes)
        }))
        .unwrap_or_else(|_| Err(Stop::Fatal("internal error".into())));
        let written = self.written.take().unwrap_or_default();
        let old = record.report.clone();
        let cut = |message: String| {
            let mut checks = not_run_checks();
            checks[3] = CheckResult::from_problems(
                Check::Unchanged,
                vec![Problem {
                    path: None,
                    message,
                }],
            );
            checks.to_vec()
        };
        let mut checks = match &result {
            Ok(verdict) => verdict.checks.to_vec(),
            Err(Stop::Fatal(message)) => {
                cut(format!("The re-sync stopped: {message}. {RESYNC_CUT}"))
            }
            Err(Stop::Cancelled) => cut(format!("You stopped the re-sync. {RESYNC_CUT}")),
        };
        checks.extend(
            old.checks
                .iter()
                .find(|c| c.check == Check::History)
                .cloned(),
        );
        let diff = result.as_ref().ok().map(|v| &v.diff);
        let mut report = self.build_report(checks, diff, old.install.clone(), old.history.clone());
        report.started_at = old.started_at;
        let copy_stats = self.resync_stats(&record.copy_stats, &written);
        let snapshot = self.snapshot.clone();
        if let Some(record) = &mut self.record {
            record.snapshot = snapshot.unwrap_or_else(|| record.snapshot.clone());
            if let Ok(verdict) = &result {
                record.final_stats = verdict.final_stats.clone();
            }
            record.copy_stats = copy_stats;
            record.running = None;
            record.resync_written.clear();
        }
        self.save_record_report(&report);
        self.save_report(&report);
        self.ready(report);
    }

    /// While a re-sync runs: adds `paths` to what it wrote, and saves that before they
    /// are written (normalising reports its rewrites after the fact, so a restart in the
    /// middle of it leaves them counted as changes to the copy: the safe side).
    fn note_written<'a>(&mut self, paths: impl IntoIterator<Item = &'a str>) {
        let Some(written) = &mut self.written else {
            return;
        };
        let before = written.len();
        written.extend(paths.into_iter().map(str::to_owned));
        if written.len() == before {
            return;
        }
        let all: Vec<String> = written.iter().cloned().collect();
        if let Some(record) = &mut self.record {
            record.resync_written = all;
        }
        self.save_record();
    }

    /// The copy's stat after a re-sync that wrote `written`, from its stat before
    /// (`old`): the re-sync's own paths as they are now, everything else as before,
    /// so that a change someone else made to the copy meanwhile still shows. The
    /// re-sync's own are the paths in `written` and under them, what normalising
    /// rewrites in each repo's git folder (the index, by its entries, and the config)
    /// and new loose objects.
    fn resync_stats(&self, old: &[StatEntry], written: &BTreeSet<String>) -> Vec<StatEntry> {
        let indexes: BTreeSet<String> = self
            .snapshot()
            .git
            .iter()
            .filter_map(|g| gitx::index_path(&self.dest, &g.path))
            .flat_map(|index| {
                let config = match index.rsplit_once('/') {
                    Some((dir, _)) => format!("{dir}/config"),
                    None => "config".to_owned(),
                };
                [index, config]
            })
            .collect();
        let ours = |path: &str| {
            let mut prefix = String::new();
            for part in path.split('/') {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(part);
                if written.contains(&prefix) {
                    return true;
                }
            }
            let parts: Vec<&str> = path.split('/').collect();
            indexes.contains(path)
                || parts
                    .iter()
                    .position(|p| *p == ".git")
                    .is_some_and(|i| parts[i + 1..].contains(&"objects"))
        };
        let now = self.copy_state(&self.dest);
        let mut stats: BTreeMap<RelPath, StatEntry> = old
            .iter()
            .filter(|e| !ours(e.path.as_str()))
            .map(|e| (e.path.clone(), e.clone()))
            .collect();
        for entry in now.entries {
            if ours(entry.path.as_str()) {
                stats.insert(entry.path.clone(), entry);
            }
        }
        stats.into_values().collect()
    }

    /// `migrateFinish`. Once it has done its part it answers through
    /// [`Update::Finished`]; the same action again (the config could not be written)
    /// only repeats that.
    fn finish(&mut self, request: Id, action: FinishAction) {
        if let Some(done) = self.finished {
            // Finishing a delete whose copy then went: End the move only forgets it.
            let forget = done == FinishAction::Delete
                && action == FinishAction::Discard
                && self.placed
                && self.copy_missing();
            if forget {
                if let Err(why) = self.finish_here(action) {
                    self.refuse(Command::Finish(request, action), &why);
                    return;
                }
            } else if done != action {
                let why = format!(
                    "the migration is finishing ({}); send that migrateFinish again",
                    action_name(done)
                );
                self.refuse(Command::Finish(request, action), &why);
                return;
            }
        } else if let Err(why) = self.finish_here(action) {
            self.refuse(Command::Finish(request, action), &why);
            return;
        }
        // Keep changed nothing here, so another answer may still follow it.
        if action != FinishAction::Keep {
            self.finished = Some(action);
        }
        self.send(Update::Finished {
            request,
            id: self.id.clone(),
            action,
            source: self.source.clone(),
            dest: self.dest.clone(),
        });
    }

    /// The worker's part of `migrateFinish`, or why it is refused.
    fn finish_here(&mut self, action: FinishAction) -> Result<(), String> {
        let copy_gone = self.placed && self.copy_missing();
        match action {
            FinishAction::Delete => {
                if !self.deleting {
                    return Err(
                        "send migrateBeginDelete first; the original may only go once it passes"
                            .into(),
                    );
                }
                if copy_gone {
                    return Err(format!(
                        "The copy at {} is gone, so the move can't finish; end it with discard.",
                        self.dest_str()
                    ));
                }
                let report = self.report.clone();
                let (source, dest) = (self.source_str(), self.dest_str());
                if let (Some(record), Some(report)) = (&mut self.record, &report) {
                    if report.history_ok() {
                        // Saved first: from here the copies may be the only transcripts
                        // left, so a later discard must not delete them.
                        if !record.originals_removed {
                            record.originals_removed = true;
                            if let Err(e) = store::save_record(record) {
                                record.originals_removed = false;
                                return Err(e);
                            }
                        }
                        let kept = history::remove_originals(
                            &self.ctx.claude_projects,
                            &record.history,
                            &source,
                            &dest,
                        );
                        if !kept.is_empty() {
                            let mut report = report.clone();
                            report.warnings.extend(kept.iter().map(|k| {
                                format!(
                                    "Kept the old transcript {}: {}",
                                    k.path.clone().unwrap_or_default(),
                                    k.message
                                )
                            }));
                            self.save_report(&report);
                        }
                    }
                }
            }
            FinishAction::Keep => {
                if self.deleting {
                    return Err(DELETING.into());
                }
                if !self.placed {
                    return Err("the checks failed, so there is no copy to keep".into());
                }
                if copy_gone {
                    return Err(format!(
                        "The copy at {} is gone, so there is nothing to keep; end the move with discard.",
                        self.dest_str()
                    ));
                }
            }
            FinishAction::Discard if copy_gone => {
                // Nothing left to delete: only forgotten. The transcript copies made for
                // it go (their originals stay), unless the delete began removing the
                // originals; the install backup stays unless the original is there
                // untouched, since it may hold the only version left of what the
                // install changed.
                let originals_removed = self.finished == Some(FinishAction::Delete)
                    || self.record.as_ref().is_some_and(|r| r.originals_removed);
                if !originals_removed {
                    self.remove_history_copies();
                }
                if self.install_backup.is_some() && !self.deleting && !self.source_missing() {
                    let _ = fsx::remove_any(&store::backup_dir(&self.report_path));
                }
                self.placed = false;
            }
            FinishAction::Discard => {
                if self.deleting {
                    return Err(DELETING.into());
                }
                if self.placed {
                    if self.source_missing() {
                        return Err(format!(
                            "{} It won't be deleted.",
                            self.source_gone_message()
                        ));
                    }
                    if let Some(why) = self.recovered_refusal("delete it") {
                        return Err(why);
                    }
                    let changed = self.copy_changes();
                    if !changed.is_empty() {
                        return Err(format!(
                            "The copy at {} changed since it was checked ({}), so it may hold work \
                             of yours; Refract won't delete it. Delete it yourself if you meant to, \
                             or keep both.",
                            self.dest_str(),
                            sample(&changed)
                        ));
                    }
                    self.discard_placed();
                }
            }
        }
        Ok(())
    }

    fn test_hook(&self, point: &str, round: u64, copy: &Path) {
        let Some(script) = std::env::var_os(TEST_HOOK).filter(|s| !s.is_empty()) else {
            return;
        };
        let _ = Process::new("sh")
            .arg(script)
            .arg(point)
            .arg(round.to_string())
            .arg(copy)
            .arg(&self.source)
            .status();
    }
}

fn action_name(action: FinishAction) -> &'static str {
    match action {
        FinishAction::Delete => "delete",
        FinishAction::Keep => "keep",
        FinishAction::Discard => "discard",
    }
}

/// With the checks and an unrun history check, the report of a copy just placed.
fn not_moved(checks: &[CheckResult; 4]) -> Vec<CheckResult> {
    let mut checks = checks.to_vec();
    checks.push(CheckResult::not_run(Check::History));
    checks
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into())
}

/// `check` with `extra` problems added (failed when there are any).
fn with_problems(check: &CheckResult, extra: Vec<Problem>) -> CheckResult {
    if extra.is_empty() {
        return check.clone();
    }
    let mut problems = check.problems.clone();
    problems.extend(extra);
    CheckResult::from_problems(check.check, problems)
}

/// A walk's relative path for the protocol: `"."` for the root.
fn dot(path: String) -> String {
    if path.is_empty() {
        ".".into()
    } else {
        path
    }
}

/// The first few of `paths`, for a message.
fn sample(paths: &[String]) -> String {
    let mut shown = paths.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    if paths.len() > 3 {
        shown.push_str(&format!(" and {} more", paths.len() - 3));
    }
    shown
}

/// True when `dest` lies on the Linux file system: no `.` or `..` in it, and with its
/// nearest existing folder resolved (symlinks followed), neither under `/mnt/` nor on
/// a 9p mount (how WSL 2 shows Windows drives).
fn on_linux_fs(dest: &Path) -> bool {
    if dest
        .components()
        .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return false;
    }
    let Some(existing) = dest.ancestors().find(|p| fs::symlink_metadata(p).is_ok()) else {
        return false;
    };
    let Ok(resolved) = fs::canonicalize(existing) else {
        return false;
    };
    let full = resolved.join(dest.strip_prefix(existing).unwrap_or(Path::new("")));
    !is_windows_mount(&state::path_str(&full))
        && !state::under_test_slow_prefix(dest)
        && !state::under_test_slow_prefix(&full)
        && !fsx::is_9p(&resolved)
}

fn not_run_checks() -> [CheckResult; 4] {
    [
        CheckResult::not_run(Check::Files),
        CheckResult::not_run(Check::GitState),
        CheckResult::not_run(Check::NothingSkipped),
        CheckResult::not_run(Check::Unchanged),
    ]
}

/// Every path in `changes`, sorted.
fn changed_paths(changes: &StatChanges) -> Vec<String> {
    let mut paths: Vec<String> = changes
        .changed
        .iter()
        .chain(&changes.added)
        .chain(&changes.removed)
        .map(|p| p.as_str().to_owned())
        .collect();
    paths.sort();
    paths
}

/// Symlinks in the snapshot whose target lies outside the repo: absolute (Linux or
/// Windows) or climbing above the root with `..`.
fn external_links(snapshot: &SourceSnapshot) -> Vec<String> {
    snapshot
        .manifest
        .entries
        .iter()
        .filter_map(|e| {
            let target = e.link_target.as_deref()?;
            let absolute = target.starts_with('/')
                || target.starts_with('\\')
                || target.as_bytes().get(1) == Some(&b':');
            let mut depth: i64 = e.path.as_str().matches('/').count() as i64;
            let mut escapes = false;
            for part in target.split(['/', '\\']) {
                match part {
                    "" | "." => {}
                    ".." => {
                        depth -= 1;
                        escapes |= depth < 0;
                    }
                    _ => depth += 1,
                }
            }
            (absolute || escapes).then(|| e.path.as_str().to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use refract_core::migrate::manifest::{Entry, EntryKind, Manifest};

    #[test]
    fn links_leaving_the_repo_are_external() {
        let link = |path: &str, target: &str| Entry {
            path: RelPath::new(path).unwrap(),
            kind: EntryKind::Symlink,
            size: 0,
            mtime_ns: 0,
            attributes: 0,
            sha256: None,
            blob: None,
            link_target: Some(target.into()),
        };
        let snapshot = SourceSnapshot {
            version: SOURCE_SNAPSHOT_VERSION,
            manifest: Manifest {
                entries: vec![
                    link("current", "src"),
                    link("docs/up", "../README.md"),
                    link("docs/out", "../../elsewhere"),
                    link("abs", "/etc/hosts"),
                    link("win", "C:\\Users\\dev"),
                    link("a/b/back", "../../c/../d"),
                ],
                ..Manifest::default()
            },
            git: vec![],
            linux_git: false,
        };
        assert_eq!(external_links(&snapshot), ["docs/out", "abs", "win"]);
    }
}
