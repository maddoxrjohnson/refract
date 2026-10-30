//! One migration on its own thread: preflight, copy, normalise, verify until the source
//! holds still, place, install, move history, report; then the follow-up requests
//! (`migrateRecheck`, `migrateResync`, `migrateFinish`, `migrateCancel`) until it ends.
//!
//! The main thread talks to it through a [`Handle`]: commands in, [`Update`]s out
//! through the sink. Cancelling while it runs sets a flag the long steps check between
//! files, then sends [`Command::Cancel`], which the worker answers once it has deleted
//! what it made.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use refract_core::migrate::gitstate::{self, StatusKind};
use refract_core::migrate::manifest::{
    compare_stats, diff, ManifestDiff, RelPath, StatChanges, StatEntry,
};
use refract_core::migrate::preflight::{self, GitOperation, PreflightFacts};
use refract_core::migrate::report::{
    manifest_checks, Check, CheckResult, CheckStatus, InstallResult, Problem, Report,
    REPORT_VERSION,
};
use refract_core::migrate::{SourceSnapshot, SOURCE_SNAPSHOT_VERSION};
use refract_core::protocol::{
    code, BusyAgent, Event, FinishAction, Id, MigrateParams, MigrateProgress, MigrateStep,
    MigrationTicket, Recheck, Reply, Response,
};
use refract_core::sessions::{is_windows_mount, path_within};

use super::store::{self, Record};
use super::tree::{self, Cancel, Cancelled};
use super::{fsx, gitx, history, install, source};
use crate::{git, procs, state};

/// Tests set this to a shell script the worker runs at fixed points, with the point,
/// the round, the copy and the source as arguments: `copied` after the first copy,
/// before normalising; `verified` after each round's checks, before the source is
/// statted again.
pub const TEST_HOOK: &str = "REFRACT_AGENT_TEST_MIGRATE_HOOK";

/// Rounds of re-copying a changing source before check 4 fails (spec §5).
pub const MAX_ROUNDS: u64 = 3;

/// Least time between two progress events of one step.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// What the main thread asks of a migration; each carries the request to answer.
#[derive(Debug)]
pub enum Command {
    Cancel(Id),
    Recheck(Id),
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
    /// `migrateFinish` (or `migrateCancel` of a ready migration, as `Discard`) is done
    /// on the worker's side; the main thread updates the config, then answers `request`.
    Finished {
        request: Id,
        id: String,
        action: FinishAction,
        source: PathBuf,
        dest: PathBuf,
    },
    /// The worker is gone; forget the migration.
    Ended { id: String },
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
/// report again as `migrateReady` and waits for the app.
pub fn resume(record: Record, ctx: Context, sink: Sink) -> io::Result<Handle> {
    let (commands, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let handle = Handle {
        id: record.id.clone(),
        source: record.source.clone(),
        dest: record.dest.clone(),
        phase: Phase::Ready,
        report: Some(Box::new(record.report.clone())),
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
            let report = record.report.clone();
            worker.record = Some(record);
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
    sink: Sink,
    step: Option<MigrateStep>,
    last: Instant,
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
            step,
            done,
            total,
        })));
    }
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
            sink: sink.clone(),
            step: None,
            last: Instant::now(),
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
                self.answer(Response::err(request, code::MIGRATE_FAILED, message));
                self.send(Update::Ended {
                    id: self.id.clone(),
                });
                return;
            }
        }
        let outcome = catch_unwind(AssertUnwindSafe(|| self.pipeline()))
            .unwrap_or_else(|_| Err(Stop::Fatal("internal error".into())));
        match outcome {
            Ok(()) => {}
            Err(Stop::Cancelled) => {
                self.discard();
                // The cancel request follows the flag.
                for command in rx.iter() {
                    match command {
                        Command::Cancel(request) => {
                            self.answer(Response::ok(request, Reply::Done));
                            break;
                        }
                        other => self.refuse(other, "the migration was cancelled"),
                    }
                }
                self.send(Update::Ended {
                    id: self.id.clone(),
                });
                return;
            }
            Err(Stop::Fatal(message)) => {
                self.discard();
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
        if !self.source.is_absolute() || !self.dest.is_absolute() {
            return Err("The source and destination must be absolute Linux paths.".into());
        }
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
        let dest = self.dest_str();
        let dest_on_linux = !is_windows_mount(&dest) && !state::under_test_slow_prefix(&self.dest);
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
        let pending = compare_stats(&tree::manifest_stats(&self.snapshot().manifest), &before);
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
            self.discard();
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
        self.temp = None;
        let mut checks = verdict.checks.to_vec();
        checks.push(CheckResult::not_run(Check::History));
        let report = self.build_report(checks, Some(&verdict.diff), vec![], Default::default());
        let record = Record::new(
            &self.id,
            report,
            self.report_path.clone(),
            self.snapshot().clone(),
            verdict.final_stats,
        );
        if let Err(e) = store::save_record(&record) {
            eprintln!("refract-agent: {e}");
        }
        self.record = Some(record);
        store::unmark_temp(&self.id);

        let install = self.install()?;
        self.check_cancel()?;
        let history = self.move_history();
        self.check_cancel()?;
        let history_check = CheckResult::from_problems(Check::History, history.problems.clone());
        let mut checks = verdict.checks.to_vec();
        checks.push(history_check);
        let report = self.build_report(checks, Some(&verdict.diff), install, history.moved);
        if let Some(record) = &mut self.record {
            record.history = history.files;
            record.report = report.clone();
            if let Err(e) = store::save_record(record) {
                eprintln!("refract-agent: {e}");
            }
        }
        self.save_report(&report);
        self.ready(report);
        Ok(())
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
    /// files (paths relative to the top). Files that fail count as copy problems.
    fn normalise(&mut self, copy: &Path, only: Option<&BTreeSet<String>>) -> Result<(), Stop> {
        self.check_cancel()?;
        let repos: Vec<(String, Vec<String>)> = self
            .snapshot()
            .git
            .iter()
            .map(|g| (g.path.clone(), gitx::windows_config(&g.snapshot.config)))
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
            let progress = &mut self.progress;
            let problems = gitx::normalise(&dir, &windows, only_here.as_ref(), &mut || {
                done += 1;
                progress.report(MigrateStep::Normalising, done.min(total), total);
            })
            .map_err(Stop::Fatal)?;
            for (file, message) in problems {
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
            let changes = compare_stats(&previous, &now);
            previous = now;
            if changes.is_empty() {
                let unchanged = CheckResult::from_problems(Check::Unchanged, self.busy_problems());
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
                problems.extend(self.busy_problems());
                let unchanged = CheckResult::from_problems(Check::Unchanged, problems);
                return Ok(Verdict {
                    checks: [files, git, skipped_check, unchanged],
                    diff,
                    final_stats: previous,
                });
            }
            let errors = tree::recopy(&self.source, copy, &changes, &*self.cancel)?;
            self.add_copy_errors(&changes, errors);
            let touched: BTreeSet<String> = changes
                .added
                .iter()
                .chain(&changes.changed)
                .map(|p| p.as_str().to_owned())
                .collect();
            self.normalise(copy, Some(&touched))?;
            pending = changes;
        }
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
        let (diff, manifest_error) = match built {
            Ok(manifest) => (diff(&snapshot.manifest, &manifest), None),
            Err(e) => (ManifestDiff::default(), Some(e)),
        };
        let copy_errors: Vec<Problem> = self.copy_errors.values().cloned().collect();
        let [mut files, nothing_skipped] = manifest_checks(&diff, &copy_errors);
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
            .map(|agent| Problem {
                path: None,
                message: format!(
                    "An agent (pid {}) is running in {}. Stop it first.",
                    agent.pid, agent.cwd
                ),
            })
            .collect()
    }

    fn busy_agents(&self) -> Vec<BusyAgent> {
        let source = self.source_str();
        procs::scan()
            .into_iter()
            .filter(|p| path_within(&p.cwd, &source))
            .map(|p| BusyAgent {
                pid: p.pid,
                cwd: p.cwd,
                session_id: p.session_id,
            })
            .collect()
    }

    /// Runs the install steps in the placed copy.
    fn install(&mut self) -> Result<Vec<InstallResult>, Stop> {
        let skipped = self.snapshot().manifest.skipped.clone();
        let steps = install::plan(&self.dest, &skipped);
        if steps.is_empty() {
            return Ok(Vec::new());
        }
        let total = steps.len() as u64;
        let (_, log) = store::report_paths(self.started_at, &file_name(&self.source));
        self.progress.report(MigrateStep::Installing, 0, total);
        let progress = &mut self.progress;
        let results = install::run(
            &self.dest,
            steps,
            &log,
            install::INSTALL_TIMEOUT,
            &*self.cancel,
            &mut |done| progress.report(MigrateStep::Installing, done, total),
        );
        self.check_cancel()?;
        Ok(results)
    }

    fn move_history(&mut self) -> history::Copied {
        let (source, dest) = (self.source_str(), self.dest_str());
        let progress = &mut self.progress;
        history::copy(
            &self.ctx.claude_projects,
            &source,
            &dest,
            &mut |done, total| progress.report(MigrateStep::MovingHistory, done, total),
        )
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
            history,
            linux_git: snapshot.linux_git,
            warnings,
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

    /// Deletes what this migration made: the temporary copy, or the placed copy and
    /// the transcripts copied for it, and its state.
    fn discard(&mut self) {
        if let Some(temp) = self.temp.take() {
            let _ = fsx::remove_any(&temp);
        }
        if self.placed {
            let _ = fsx::remove_any(&self.dest);
            if let Some(record) = &self.record {
                history::remove(&self.ctx.claude_projects, &record.history, false);
            }
            self.placed = false;
        }
        store::unmark_temp(&self.id);
        store::remove_record(&self.id);
    }

    fn refuse(&self, command: Command, why: &str) {
        let request = match command {
            Command::Cancel(id)
            | Command::Recheck(id)
            | Command::Resync(id)
            | Command::Finish(id, _) => id,
        };
        self.answer(Response::err(request, code::MIGRATE_FAILED, why));
    }

    /// Answers follow-up requests until the migration ends.
    fn serve(mut self, rx: Receiver<Command>) {
        for command in rx.iter() {
            let done = catch_unwind(AssertUnwindSafe(|| self.follow_up(command)));
            match done {
                Ok(true) => return,
                Ok(false) => {}
                Err(_) => eprintln!("refract-agent: a migration request failed"),
            }
        }
    }

    /// Handles one follow-up. True when the migration has ended.
    fn follow_up(&mut self, command: Command) -> bool {
        match command {
            Command::Recheck(request) => {
                if !self.placed {
                    self.refuse(
                        Command::Recheck(request),
                        "the copy was not placed, so there is nothing to recheck",
                    );
                    return false;
                }
                let skipped = self.skipped();
                let reply = match tree::stats(&self.source, &skipped, &AtomicBool::new(false)) {
                    Ok(now) => {
                        let before = self
                            .record
                            .as_ref()
                            .map(|r| r.final_stats.clone())
                            .unwrap_or_default();
                        Reply::Recheck(Recheck {
                            changed: changed_paths(&compare_stats(&before, &now)),
                            busy_agents: self.busy_agents(),
                        })
                    }
                    Err(Cancelled) => unreachable!("never cancelled"),
                };
                self.answer(Response::ok(request, reply));
                false
            }
            Command::Resync(request) => {
                if !self.placed {
                    self.refuse(
                        Command::Resync(request),
                        "the copy was not placed, so there is nothing to re-sync",
                    );
                    return false;
                }
                self.answer(Response::ok(request, Reply::Done));
                self.resync();
                false
            }
            Command::Finish(request, action) => self.finish(request, action),
            Command::Cancel(request) => {
                // Cancelled just as the report went out: as discarding it.
                self.finish(request, FinishAction::Discard)
            }
        }
    }

    /// Re-copies what changed since the last check into the placed copy and checks it
    /// again, then sends a fresh report.
    fn resync(&mut self) {
        let skipped = self.skipped();
        let Some(record) = self.record.clone() else {
            return;
        };
        let result = (|| -> Result<Verdict, Stop> {
            let now = tree::stats(&self.source, &skipped, &*self.cancel)?;
            let changes = compare_stats(&record.final_stats, &now);
            self.progress.report(MigrateStep::Rechecking, 0, MAX_ROUNDS);
            let errors = tree::recopy(&self.source, &self.dest, &changes, &*self.cancel)?;
            self.add_copy_errors(&changes, errors);
            let touched: BTreeSet<String> = changes
                .added
                .iter()
                .chain(&changes.changed)
                .map(|p| p.as_str().to_owned())
                .collect();
            let dest = self.dest.clone();
            self.normalise(&dest, Some(&touched))?;
            self.verify_rounds(&dest, now, changes)
        })();
        let old = record.report.clone();
        let mut checks = match &result {
            Ok(verdict) => verdict.checks.to_vec(),
            Err(Stop::Fatal(message)) => {
                let mut checks = not_run_checks();
                checks[2] = CheckResult::from_problems(
                    Check::NothingSkipped,
                    vec![Problem {
                        path: None,
                        message: format!("The re-sync stopped: {message}"),
                    }],
                );
                checks.to_vec()
            }
            Err(Stop::Cancelled) => not_run_checks().to_vec(),
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
        if let Some(record) = &mut self.record {
            record.report = report.clone();
            record.snapshot = self
                .snapshot
                .clone()
                .unwrap_or_else(|| record.snapshot.clone());
            if let Ok(verdict) = &result {
                record.final_stats = verdict.final_stats.clone();
            }
            if let Err(e) = store::save_record(record) {
                eprintln!("refract-agent: {e}");
            }
        }
        self.save_report(&report);
        self.ready(report);
    }

    /// `migrateFinish`. True when the migration has ended.
    fn finish(&mut self, request: Id, action: FinishAction) -> bool {
        let report = self.report.clone();
        match action {
            FinishAction::Delete => {
                let Some(report) = report.filter(|r| r.can_delete()) else {
                    self.refuse(
                        Command::Finish(request, action),
                        "the report does not allow deleting the original",
                    );
                    return false;
                };
                if report.history_ok() {
                    if let Some(record) = &self.record {
                        history::remove(&self.ctx.claude_projects, &record.history, true);
                    }
                }
            }
            FinishAction::Keep => {
                if !self.placed {
                    self.refuse(
                        Command::Finish(request, action),
                        "the checks failed, so there is no copy to keep",
                    );
                    return false;
                }
            }
            FinishAction::Discard => self.discard(),
        }
        store::remove_record(&self.id);
        store::unmark_temp(&self.id);
        self.send(Update::Finished {
            request,
            id: self.id.clone(),
            action,
            source: self.source.clone(),
            dest: self.dest.clone(),
        });
        true
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

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into())
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
