//! One-button refactors (`startRefactor`, `resumeRefactor`, `mergeRefactor`,
//! `discardRefactor`; spec in `.plans/refactor-spec.md`).
//!
//! A refactor is a git worktree of a repo under `~/projects/.refract-worktrees/` on a
//! new `refactor/…` branch, with an agent working in it, and a record in
//! `<state dir>/refactors/<uuid>.json` that remembers it across restarts. Its state is
//! worked out from the record, the agent's process, transcript and hook status, the
//! branch's commits and the agent's result file, and goes out in the repo's snapshot.
//!
//! Data safety (spec §5): the user's working copy is only ever written by the final
//! fast-forward of a merge; only a discard uses `--force` or `-D`; nothing is pushed; and
//! a worktree is only deleted when its real path is under the worktrees directory and
//! the repo's `git worktree list` names it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, SubsecRound, TimeDelta, Utc};
use refract_core::classify::{classify_report, LiveState, SessionState};
use refract_core::launch::folder_uri;
use refract_core::protocol::{
    code, ErrorBody, Event, LaunchMode, LaunchTicket, MergeBlock, MergeBlockReason, MergeOutcome,
    RefactorCommit, RefactorInfo, RefactorState, Reply,
};
use refract_core::refactor::{self as rules, ResultFile};
use refract_core::sessions::{path_within, paths_match};
use refract_core::transcript::SessionSummary;
use serde::{Deserialize, Serialize};

use crate::git;
use crate::launch;
use crate::procs::{self, ClaudeProcess, LiveProcess};
use crate::scan;
use crate::state::{path_str, Agent, Refresh};
use crate::worktree;

/// A refactor with no agent and no transcript this long after its launch counts as
/// interrupted rather than starting.
pub const START_GRACE: TimeDelta = TimeDelta::minutes(2);

/// Without a hook status, a live agent whose transcript ends with a finished turn
/// counts as idle only once the transcript has not been written for this long.
pub const IDLE_QUIET: TimeDelta = TimeDelta::seconds(20);

/// Seconds to use instead of [`START_GRACE`], for tests.
pub const GRACE_ENV: &str = "REFRACT_REFACTOR_GRACE_SECS";

/// Seconds to use instead of [`IDLE_QUIET`], for tests.
pub const QUIET_ENV: &str = "REFRACT_REFACTOR_QUIET_SECS";

/// How often refactors are looked at again while one is starting or its agent works.
pub const ACTIVE_POLL: Duration = Duration::from_secs(3);

/// How often all of them are looked at.
pub const IDLE_POLL: Duration = Duration::from_secs(30);

/// What the helper remembers about a refactor, as `<uuid>.json` in the records
/// directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    /// The repo's path, as its snapshot names it.
    pub repo: String,
    pub worktree: String,
    pub branch: String,
    pub base_commit: String,
    pub target_branch: String,
    /// Empty when no session is known (an adopted worktree without transcripts).
    #[serde(default)]
    pub session_id: String,
    /// The last launch request written for it; empty when none is known.
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub focus: Option<String>,
    pub created_at: DateTime<Utc>,
    /// When an agent was last launched for it; `createdAt` until it is resumed.
    #[serde(default)]
    pub launched_at: Option<DateTime<Utc>>,
    /// Found on disk without a record, so never starting.
    #[serde(default)]
    pub adopted: bool,
    #[serde(default)]
    pub blocked: Option<MergeBlock>,
    /// The branch's commit when the merge stopped: a new commit clears `blocked`.
    #[serde(default)]
    pub blocked_head: Option<String>,
    /// Merged, but its worktree could not be removed; only a discard is left.
    #[serde(default)]
    pub leftover: bool,
    /// Why the worktree was kept.
    #[serde(default)]
    pub leftover_reason: Option<String>,
}

#[derive(Debug)]
struct Entry {
    /// The record's file name without `.json`.
    id: String,
    record: Record,
    /// The worktree's git directory, once known.
    git_dir: Option<PathBuf>,
    /// The state last reported.
    info: Option<RefactorInfo>,
    /// The sessions last seen live in the worktree, so their hook statuses refresh it.
    live_ids: Vec<String>,
    /// Its agent was last seen waiting on the user (hook `waitingOnUser`).
    waiting: bool,
}

/// Every refactor, by repo path.
#[derive(Debug)]
pub struct Refactors {
    records_dir: PathBuf,
    worktrees_dir: PathBuf,
    distro: Option<String>,
    grace: TimeDelta,
    quiet: TimeDelta,
    entries: BTreeMap<PathBuf, Entry>,
    /// Repos whose start, merge or discard runs on the worker (see [`Job`]).
    busy: BTreeSet<PathBuf>,
    /// When every refactor was last looked at by [`Refactors::poll`].
    last_full: Option<DateTime<Utc>>,
}

type Fail = ErrorBody;

fn fail(message: impl Into<String>) -> Fail {
    ErrorBody {
        code: code::REFACTOR_FAILED,
        message: message.into(),
    }
}

fn busy() -> Fail {
    fail("This repo's refactor is busy with another action; try again in a moment.")
}

/// Whole seconds from the environment variable `name`, if set and valid.
fn env_secs(name: &str) -> Option<TimeDelta> {
    let secs: i64 = std::env::var(name).ok()?.trim().parse().ok()?;
    (secs >= 0).then(|| TimeDelta::seconds(secs))
}

/// What a refactor request does next: run on the worker, or answer at once.
pub enum Step {
    Run(Job),
    Answer(Reply, Vec<Event>),
}

/// A start, merge or discard, which runs git and so runs on a worker thread, away from
/// the agent's state. [`Refactors::finish`] takes in its [`Done`].
#[derive(Debug)]
pub struct Job {
    repo: PathBuf,
    records_dir: PathBuf,
    worktrees_dir: PathBuf,
    distro: Option<String>,
    now: DateTime<Utc>,
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    Start {
        focus: Option<String>,
        launch_dir: PathBuf,
    },
    /// With the pids of the `claude` processes in the worktree when it was checked,
    /// which were idle.
    Merge(Record, Vec<u32>),
    Discard(Record),
    /// A linked worktree's removal, serialised with the refactor jobs of its main repo.
    RemoveWorktree(Box<worktree::Removal>),
}

/// What a [`Job`] did.
#[derive(Debug)]
pub struct Done {
    repo: PathBuf,
    kind: DoneKind,
}

#[derive(Debug)]
enum DoneKind {
    Started(Result<Box<Started>, Fail>),
    Merged(Result<Merged, Fail>),
    Discarded(Result<(), Fail>),
    WorktreeRemoved(Result<(), Fail>),
    Failed(Fail),
}

#[derive(Debug)]
struct Started {
    ticket: LaunchTicket,
    id: String,
    record: Record,
    git_dir: PathBuf,
}

#[derive(Debug)]
enum Merged {
    Blocked {
        reason: MergeBlockReason,
        files: Vec<String>,
    },
    Done {
        commits: u32,
        kept: Vec<String>,
        /// Why the worktree could not be removed.
        leftover: Option<String>,
    },
}

impl Job {
    /// The repo the job is for.
    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// Runs the job: git and files only, no agent state.
    pub fn run(self) -> Done {
        let kind = match &self.kind {
            Kind::Start { focus, launch_dir } => {
                DoneKind::Started(self.start(focus.as_deref(), launch_dir).map(Box::new))
            }
            Kind::Merge(record, idle) => DoneKind::Merged(self.merge(record, idle)),
            Kind::Discard(record) => DoneKind::Discarded(self.discard(record)),
            Kind::RemoveWorktree(removal) => DoneKind::WorktreeRemoved(worktree::remove(removal)),
        };
        Done {
            repo: self.repo,
            kind,
        }
    }

    fn start(&self, focus: Option<&str>, launch_dir: &Path) -> Result<Started, Fail> {
        let repo = &self.repo;
        let Some(base) = commit_of(repo, "HEAD") else {
            return Err(fail("The repo has no commits yet."));
        };
        let Some(target) = current_branch(repo) else {
            return Err(fail("Check out a branch first."));
        };
        fs::create_dir_all(&self.worktrees_dir).map_err(|e| {
            fail(format!(
                "cannot create {}: {e}",
                self.worktrees_dir.display()
            ))
        })?;
        let stamp = rules::stamp(chrono::Local::now().naive_local());
        let repo_name = repo
            .file_name()
            .map_or_else(|| "repo".into(), |n| n.to_string_lossy().into_owned());
        // Both are free now, so whatever is there after a failure was made by this call.
        let worktree = (1..)
            .map(|n| {
                self.worktrees_dir.join(rules::numbered(
                    &rules::worktree_name(&repo_name, &stamp),
                    n,
                ))
            })
            .find(|p| fs::symlink_metadata(p).is_err())
            .expect("a free name");
        let branch = (1..)
            .map(|n| rules::numbered(&rules::branch_name(&stamp), n))
            .find(|b| !branch_exists(repo, b))
            .expect("a free branch");
        let wt = path_str(&worktree);
        // A failure takes back the worktree and branch this call made.
        let undo = |why: Fail| {
            if fs::symlink_metadata(&worktree).is_ok()
                && check_safe(&self.worktrees_dir, repo, &worktree).is_ok()
            {
                let _ = run_git(repo, &["worktree", "remove", "--force", &wt]);
            }
            if branch_exists(repo, &branch) {
                let _ = run_git(repo, &["branch", "-q", "-D", &branch]);
            }
            why
        };
        let added = run_git(repo, &["worktree", "add", "-q", "-b", &branch, &wt, &base])
            .map_err(|e| undo(fail(format!("cannot run git: {e}"))))?;
        if !added.ok {
            return Err(undo(fail(format!(
                "git worktree add failed: {}",
                added.stderr.trim()
            ))));
        }
        let git_dir = absolute_git_dir(&worktree)
            .ok_or_else(|| undo(fail("cannot find the new worktree's git directory")))?;
        let result_path = path_str(&git_dir.join(rules::RESULT_FILE));
        let focus = focus.and_then(rules::clean_focus);
        let mode = LaunchMode::New {
            prompt: Some(rules::prompt(&branch, &result_path, focus.as_deref())),
            plan_mode: true,
        };
        let ids = launch::Ids::new(&mode);
        let record = Record {
            repo: path_str(repo),
            worktree: wt.clone(),
            branch: branch.clone(),
            base_commit: base,
            target_branch: target,
            session_id: ids.session_id.clone(),
            request_id: ids.request_id.clone(),
            focus,
            created_at: self.now.trunc_subsecs(0),
            launched_at: None,
            adopted: false,
            blocked: None,
            blocked_head: None,
            leftover: false,
            leftover_reason: None,
        };
        // The record first, so a launched agent always has one.
        let id = uuid::Uuid::new_v4().to_string();
        write_record(&self.records_dir, &id, &record)
            .map_err(|e| undo(fail(format!("cannot save the refactor's record: {e}"))))?;
        let ticket = launch::prepare_planned(
            &wt,
            &mode,
            rules::TITLE,
            &ids,
            launch_dir,
            self.distro.as_deref(),
            self.now,
        )
        .map_err(|e| {
            let _ = fs::remove_file(self.records_dir.join(format!("{id}.json")));
            undo(e)
        })?;
        Ok(Started {
            ticket,
            id,
            record,
            git_dir,
        })
    }

    /// Spec §5.2–3: checks, then the rebase in the worktree, then the fast-forward of
    /// the target, then the clean-up. Anything that stops it before the fast-forward
    /// leaves the repo, the worktree and the branch as they were (a finished rebase
    /// aside).
    fn merge(&self, record: &Record, idle: &[u32]) -> Result<Merged, Fail> {
        let repo = &self.repo;
        let worktree = Path::new(&record.worktree);
        let blocked = |reason, files| Ok(Merged::Blocked { reason, files });
        check_safe(&self.worktrees_dir, repo, worktree)?;
        let target_ref = format!("refs/heads/{}", record.target_branch);
        let branch_ref = format!("refs/heads/{}", record.branch);
        if commit_of(repo, &branch_ref).is_none() {
            return Err(fail(format!("The branch {} is gone.", record.branch)));
        }
        if commit_of(repo, &target_ref).is_none() {
            return blocked(MergeBlockReason::TargetMissing, vec![]);
        }
        if !is_ancestor(repo, &record.base_commit, &target_ref) {
            return blocked(MergeBlockReason::TargetRewound, vec![]);
        }
        // The worktree must be on its branch, with nothing under way and nothing
        // uncommitted.
        let wt_git = absolute_git_dir(worktree)
            .ok_or_else(|| fail("cannot find the worktree's git directory"))?;
        let under_way = [
            "rebase-merge",
            "rebase-apply",
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
        ]
        .iter()
        .any(|f| fs::symlink_metadata(wt_git.join(f)).is_ok());
        if under_way || symbolic_head(worktree).as_deref() != Some(branch_ref.as_str()) {
            return blocked(MergeBlockReason::WorktreeBusy, vec![]);
        }
        let dirty = changed_in_worktree(worktree)?;
        if !dirty.is_empty() {
            return blocked(MergeBlockReason::WorktreeDirty, dirty);
        }
        let range = format!("{target_ref}..{branch_ref}");
        if count(repo, &range) == 0 {
            return Err(fail("The refactor has no commits to merge."));
        }
        let holders = target_holders(repo, worktree, &target_ref)?;
        if !holders.is_empty() {
            return blocked(MergeBlockReason::TargetBusy, holders);
        }
        // Where the target is checked out in the repo itself, its uncommitted files must
        // not be ones the refactor changed.
        let checked_out = symbolic_head(repo).as_deref() == Some(target_ref.as_str());
        if checked_out {
            let clash = dirty_overlap(repo, &target_ref, &branch_ref)?;
            if !clash.is_empty() {
                return blocked(MergeBlockReason::DirtyOverlap, clash);
            }
        }
        // An agent that started in the worktree while the job waited would race it.
        if newcomer(worktree, idle) {
            return blocked(MergeBlockReason::Live, vec![]);
        }
        // The branch onto the target, in the worktree; undone on conflict.
        if !is_ancestor(repo, &target_ref, &branch_ref) {
            let mut args = REBASE.to_vec();
            args.push(&target_ref);
            let rebase =
                run_git(worktree, &args).map_err(|e| fail(format!("cannot run git: {e}")))?;
            if !rebase.ok {
                let conflicted = run_git(worktree, &["diff", "--name-only", "--diff-filter=U"])
                    .map(|o| lines(&o.stdout))
                    .unwrap_or_default();
                // None was under way before, so this one is Refract's.
                if rebase_under_way(&wt_git) {
                    let _ = run_git(worktree, &["rebase", "--abort"]);
                }
                if conflicted.is_empty() {
                    return Err(fail(format!("git rebase failed: {}", rebase.stderr.trim())));
                }
                return blocked(MergeBlockReason::Conflict, conflicted);
            }
        }
        let commits = count(repo, &range);
        let Some(head) = commit_of(repo, &branch_ref) else {
            return Err(fail(format!("The branch {} is gone.", record.branch)));
        };
        // HEAD may have moved since: then the target moves as a ref, if no worktree has
        // it now.
        let mut checked_out = checked_out;
        if checked_out && symbolic_head(repo).as_deref() != Some(target_ref.as_str()) {
            let holders = target_holders(repo, worktree, &target_ref)?;
            if !holders.is_empty() {
                return blocked(MergeBlockReason::TargetBusy, holders);
            }
            checked_out = false;
        }
        let merged = if checked_out {
            let mut args = MERGE.to_vec();
            args.push(&branch_ref);
            run_git(repo, &args)
        } else {
            let refspec = format!("{branch_ref}:{target_ref}");
            let mut args = FETCH.to_vec();
            args.push(&refspec);
            run_git(repo, &args)
        }
        .map_err(|e| fail(format!("cannot run git: {e}")))?;
        if !merged.ok {
            return Err(fail(format!(
                "Moving {} failed, so everything is kept: {}",
                record.target_branch,
                merged.stderr.trim()
            )));
        }
        if commit_of(repo, &target_ref).as_deref() != Some(head.as_str()) {
            return Err(fail(format!(
                "{} is not at the refactor's last commit after the merge, so the worktree \
                 and branch are kept.",
                record.target_branch
            )));
        }
        // Merged: the worktree and branch go, unless git refuses.
        let mut kept = Vec::new();
        let mut leftover = None;
        let removed = run_git(repo, &["worktree", "remove", &record.worktree]);
        match removed {
            Ok(out) if out.ok => {
                let deleted = run_git(repo, &["branch", "-q", "-d", &record.branch]);
                if !deleted.as_ref().is_ok_and(|o| o.ok) {
                    // `-d` wants the branch merged into HEAD, which is another branch
                    // when the target is not checked out. It is merged into the target,
                    // so delete it only if it still points where it was merged.
                    let merged_in = is_ancestor(repo, &branch_ref, &target_ref);
                    let gone = merged_in
                        && run_git(repo, &["update-ref", "-d", &branch_ref, &head])
                            .is_ok_and(|o| o.ok);
                    if !gone {
                        let why = deleted.map(|o| o.stderr).unwrap_or_else(|e| e.to_string());
                        kept.push(format!("Kept the branch {}: {}", record.branch, why.trim()));
                    }
                }
            }
            Ok(out) => {
                let why = out.stderr.trim().to_owned();
                kept.push(format!(
                    "Kept the worktree at {} and its branch {}: {why}",
                    record.worktree, record.branch,
                ));
                leftover = Some(why);
            }
            Err(e) => {
                kept.push(format!("Kept the worktree at {}: {e}", record.worktree));
                leftover = Some(e.to_string());
            }
        }
        Ok(Merged::Done {
            commits,
            kept,
            leftover,
        })
    }

    /// Spec §5.4.
    fn discard(&self, record: &Record) -> Result<(), Fail> {
        let repo = &self.repo;
        if !record.branch.starts_with(rules::BRANCH_PREFIX) {
            return Err(fail(format!(
                "{} is not a refactor branch, so it is not deleted.",
                record.branch
            )));
        }
        let worktree = PathBuf::from(&record.worktree);
        let listed = worktrees(repo)
            .map_err(|e| fail(format!("cannot list the repo's worktrees: {e}")))?
            .iter()
            .any(|w| same_dir(&w.path, &worktree));
        if newcomer(&worktree, &[]) {
            return Err(fail("The refactor's agent is still running."));
        }
        if listed || fs::symlink_metadata(&worktree).is_ok() {
            check_safe(&self.worktrees_dir, repo, &worktree)?;
            let out = run_git(repo, &["worktree", "remove", "--force", &record.worktree])
                .map_err(|e| fail(format!("cannot run git: {e}")))?;
            if !out.ok {
                return Err(fail(format!(
                    "git worktree remove failed: {}",
                    out.stderr.trim()
                )));
            }
        }
        if branch_exists(repo, &record.branch) {
            let out = run_git(repo, &["branch", "-q", "-D", &record.branch])
                .map_err(|e| fail(format!("cannot run git: {e}")))?;
            if !out.ok {
                return Err(fail(format!("git branch -D failed: {}", out.stderr.trim())));
            }
        }
        Ok(())
    }
}

impl Done {
    /// True for a worktree removal that worked, after which the repos should be searched
    /// again so the worktree goes.
    pub fn removed_worktree(&self) -> bool {
        matches!(self.kind, DoneKind::WorktreeRemoved(Ok(())))
    }

    /// A job for `repo` that could not run, or panicked.
    pub fn failed(repo: PathBuf, why: impl Into<String>) -> Done {
        Done {
            repo,
            kind: DoneKind::Failed(fail(why)),
        }
    }
}

/// The git commands below work on git 2.25 and later (Ubuntu 20.04): newer settings
/// are given as `-c` overrides, which older git ignores, rather than as flags, which
/// it rejects.
///
/// The rebase of a merge, whatever the user's config says: no stash, no squash or
/// fixup reordering, no other branches moved (`rebase.updateRefs`, 2.38), no fork
/// point, merges flattened (`rebase.rebaseMerges`, 2.42; flattening is the default).
const REBASE: &[&str] = &[
    "-c",
    "rebase.autoStash=false",
    "-c",
    "rebase.autoSquash=false",
    "-c",
    "rebase.updateRefs=false",
    "-c",
    "rebase.forkPoint=false",
    "-c",
    "rebase.rebaseMerges=false",
    "rebase",
    "-q",
    "--no-autostash",
    "--no-autosquash",
    "--no-fork-point",
];

/// The fast-forward of a checked-out target, whatever the user's config says
/// (`merge.autoStash` is 2.27; before that a merge never stashes).
const MERGE: &[&str] = &[
    "-c",
    "merge.autoStash=false",
    "-c",
    "merge.ff=only",
    "-c",
    "submodule.recurse=false",
    "merge",
    "-q",
    "--ff-only",
    "--no-squash",
    "--no-verify-signatures",
    "--no-edit",
];

/// The fast-forward of a target that is not checked out: a ref update that git
/// refuses unless it is a fast-forward, or while a worktree has the target.
const FETCH: &[&str] = &[
    "-c",
    "submodule.recurse=false",
    "fetch",
    "-q",
    "--no-tags",
    "--no-recurse-submodules",
    ".",
];

impl Refactors {
    /// Records in `<state dir>/refactors`, worktrees in `$HOME/projects/.refract-worktrees`,
    /// folder URIs for `WSL_DISTRO_NAME`; [`GRACE_ENV`] and [`QUIET_ENV`] override the
    /// times.
    pub fn from_env() -> Refactors {
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let mut refactors = Refactors::new(
            launch::state_dir().join("refactors"),
            home.join(rules::WORKTREES_DIR),
            std::env::var("WSL_DISTRO_NAME")
                .ok()
                .filter(|d| !d.is_empty()),
        );
        if let Some(grace) = env_secs(GRACE_ENV) {
            refactors.grace = grace;
        }
        if let Some(quiet) = env_secs(QUIET_ENV) {
            refactors.quiet = quiet;
        }
        refactors
    }

    /// Reads the records in `records_dir`. One that cannot be read is skipped; of two
    /// for one repo the newer is kept.
    pub fn new(records_dir: PathBuf, worktrees_dir: PathBuf, distro: Option<String>) -> Refactors {
        let mut entries: BTreeMap<PathBuf, Entry> = BTreeMap::new();
        for (id, record) in read_records(&records_dir) {
            let repo = PathBuf::from(&record.repo);
            if entries
                .get(&repo)
                .is_some_and(|e| e.record.created_at >= record.created_at)
            {
                continue;
            }
            let entry = Entry {
                id,
                record,
                git_dir: None,
                info: None,
                live_ids: Vec::new(),
                waiting: false,
            };
            entries.insert(repo, entry);
        }
        Refactors {
            records_dir,
            worktrees_dir,
            distro,
            grace: START_GRACE,
            quiet: IDLE_QUIET,
            entries,
            busy: BTreeSet::new(),
            last_full: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The record of the repo at `path`, if it has a refactor.
    pub fn record(&self, path: &str) -> Option<&Record> {
        Some(&self.entries.get(&self.key(path)?)?.record)
    }

    /// True when the refactor of `repo` needs the fast poll: its repo is known, no job
    /// runs for it, and its agent was just launched, works, or waits on the user by its
    /// hook (an idle open agent with nothing to show does not).
    fn active(&self, agent: &Agent, repo: &Path, entry: &Entry, now: DateTime<Utc>) -> bool {
        if agent.repo(repo).is_none() || self.busy.contains(repo) || entry.record.leftover {
            return false;
        }
        let record = &entry.record;
        let launched = record.launched_at.unwrap_or(record.created_at);
        (!record.adopted && now - launched < self.grace)
            || entry.info.as_ref().is_none_or(|i| match i.state {
                RefactorState::Starting | RefactorState::Working => true,
                RefactorState::NeedsYou => entry.waiting,
                _ => false,
            })
    }

    /// How long until the refactors should be looked at again; `None` without any of a
    /// known repo. Often while one is active (see [`Refactors::poll`]).
    pub fn poll_interval(&self, agent: &Agent, now: DateTime<Utc>) -> Option<Duration> {
        if !self.entries.keys().any(|r| agent.repo(r).is_some()) {
            return None;
        }
        let active = self
            .entries
            .iter()
            .any(|(repo, e)| self.active(agent, repo, e, now));
        Some(if active { ACTIVE_POLL } else { IDLE_POLL })
    }

    /// Looks at the active refactors again, and at all of them every [`IDLE_POLL`].
    pub fn poll(&mut self, agent: &mut Agent, now: DateTime<Utc>) -> Vec<Event> {
        let every = TimeDelta::from_std(IDLE_POLL).expect("small");
        if self.last_full.is_none_or(|at| now - at >= every) {
            self.last_full = Some(now);
            return self.refresh(agent, None, now);
        }
        let active: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|(repo, e)| self.active(agent, repo, e, now))
            .map(|(repo, _)| repo.clone())
            .collect();
        self.refresh(agent, Some(&active), now)
    }

    fn key(&self, path: &str) -> Option<PathBuf> {
        self.entries
            .keys()
            .find(|k| paths_match(&path_str(k), path))
            .cloned()
    }

    /// The repo of the refactor at `path`, refusing one of an unknown repo or with a job
    /// running.
    fn known(&self, agent: &Agent, path: &str) -> Result<PathBuf, Fail> {
        let repo = self
            .key(path)
            .filter(|r| agent.repo(r).is_some())
            .ok_or_else(no_refactor)?;
        if self.busy.contains(&repo) {
            return Err(busy());
        }
        Ok(repo)
    }

    fn job(&mut self, repo: PathBuf, now: DateTime<Utc>, kind: Kind) -> Job {
        self.busy.insert(repo.clone());
        Job {
            repo,
            records_dir: self.records_dir.clone(),
            worktrees_dir: self.worktrees_dir.clone(),
            distro: self.distro.clone(),
            now,
            kind,
        }
    }

    /// Checks a start of a refactor of the repo at `path`, and returns the job that makes
    /// its worktree, branch, record and launch request.
    pub fn start(
        &mut self,
        agent: &Agent,
        path: &str,
        focus: Option<&str>,
        launch_dir: &Path,
        now: DateTime<Utc>,
    ) -> Result<Job, Fail> {
        let Some(repo) = agent
            .repo_paths()
            .find(|p| paths_match(&path_str(p), path))
            .cloned()
        else {
            return Err(ErrorBody {
                code: code::REPO_NOT_FOUND,
                message: format!("no repo at {path}"),
            });
        };
        if self.busy.contains(&repo) {
            return Err(busy());
        }
        if self.entries.contains_key(&repo) {
            return Err(fail("This repo already has a refactor."));
        }
        if self.distro.is_none() {
            return Err(ErrorBody {
                code: code::INTERNAL_ERROR,
                message: "WSL_DISTRO_NAME is not set, so the folder URI cannot name the distro"
                    .into(),
            });
        }
        let kind = Kind::Start {
            focus: focus.map(str::to_owned),
            launch_dir: launch_dir.to_owned(),
        };
        Ok(self.job(repo, now, kind))
    }

    /// Launches the refactor's agent again: resumed with [`rules::CONTINUE_PROMPT`], or
    /// with the fix prompt when `fix`; started anew with the original prompt (or the fix
    /// prompt) when its session never wrote a transcript, after deleting its launch
    /// request if that was never picked up.
    pub fn resume(
        &mut self,
        agent: &mut Agent,
        path: &str,
        fix: bool,
        launch_dir: &Path,
        now: DateTime<Utc>,
    ) -> Result<(LaunchTicket, Vec<Event>), Fail> {
        let repo = self.known(agent, path)?;
        let procs = procs::scan();
        let quiet = self.quiet;
        let entry = self.entries.get_mut(&repo).expect("known");
        if entry.record.leftover {
            return Err(fail("This refactor is merged already."));
        }
        let worktree = PathBuf::from(&entry.record.worktree);
        if !worktree.is_dir() {
            return Err(fail("The refactor's worktree is gone."));
        }
        let look = look(entry, agent, &procs, quiet, now);
        if look.live {
            return Err(fail("The refactor's agent is still running."));
        }
        let git_dir = entry
            .git_dir
            .clone()
            .or_else(|| absolute_git_dir(&worktree))
            .ok_or_else(|| fail("cannot find the worktree's git directory"))?;
        entry.git_dir = Some(git_dir.clone());
        let result_path = path_str(&git_dir.join(rules::RESULT_FILE));
        let old = entry.record.clone();
        let fix_prompt = || rules::fix_prompt(&old.branch, &old.target_branch, &result_path);
        let mode = match (&look.summary, fix) {
            (Some(_), false) => LaunchMode::Resume {
                session_id: old.session_id.clone(),
                prompt: Some(rules::CONTINUE_PROMPT.into()),
            },
            (Some(_), true) => LaunchMode::Resume {
                session_id: old.session_id.clone(),
                prompt: Some(fix_prompt()),
            },
            (None, false) => LaunchMode::New {
                prompt: Some(rules::prompt(
                    &old.branch,
                    &result_path,
                    old.focus.as_deref(),
                )),
                plan_mode: true,
            },
            (None, true) => LaunchMode::New {
                prompt: Some(fix_prompt()),
                plan_mode: false,
            },
        };
        if matches!(mode, LaunchMode::New { .. }) {
            // The old launch was never picked up, or its agent never wrote a word: so
            // that it cannot start late beside the new one, its request goes.
            if uuid::Uuid::try_parse(&old.request_id).is_ok() {
                for ext in ["json", "claimed"] {
                    let _ = fs::remove_file(launch_dir.join(format!("{}.{ext}", old.request_id)));
                }
            }
        }
        let ids = launch::Ids::new(&mode);
        let mut record = old.clone();
        record.session_id = ids.session_id.clone();
        record.request_id = ids.request_id.clone();
        record.launched_at = Some(now.trunc_subsecs(0));
        record.adopted = false;
        record.blocked = None;
        record.blocked_head = None;
        write_record(&self.records_dir, &entry.id, &record)
            .map_err(|e| fail(format!("cannot save the refactor's record: {e}")))?;
        let ticket = match launch::prepare_planned(
            &record.worktree,
            &mode,
            rules::TITLE,
            &ids,
            launch_dir,
            self.distro.as_deref(),
            now,
        ) {
            Ok(ticket) => ticket,
            Err(e) => {
                let _ = write_record(&self.records_dir, &entry.id, &old);
                return Err(e);
            }
        };
        entry.record = record;
        let events = self.refresh(agent, Some(&[repo]), now);
        Ok((ticket, events))
    }

    /// Checks a merge of the refactor into its target branch: blocked at once while its
    /// agent works, or the job that merges it (spec §5.2–3).
    pub fn merge(
        &mut self,
        agent: &mut Agent,
        path: &str,
        now: DateTime<Utc>,
    ) -> Result<Step, Fail> {
        let repo = self.known(agent, path)?;
        let procs = procs::scan();
        let quiet = self.quiet;
        let entry = self.entries.get_mut(&repo).expect("known");
        if entry.record.leftover {
            return Err(fail("This refactor is merged already."));
        }
        if look(entry, agent, &procs, quiet, now).busy() {
            let outcome = MergeOutcome::Blocked {
                reason: MergeBlockReason::Live,
                files: Vec::new(),
            };
            return Ok(Step::Answer(Reply::RefactorMerge(outcome), Vec::new()));
        }
        let record = entry.record.clone();
        let idle = inside(&procs, &record.worktree);
        Ok(Step::Run(self.job(repo, now, Kind::Merge(record, idle))))
    }

    /// Checks a discard of the refactor, and returns the job that deletes its worktree
    /// and branch (spec §5.4).
    pub fn discard(
        &mut self,
        agent: &mut Agent,
        path: &str,
        now: DateTime<Utc>,
    ) -> Result<Job, Fail> {
        let repo = self.known(agent, path)?;
        let procs = procs::scan();
        let quiet = self.quiet;
        let entry = self.entries.get_mut(&repo).expect("known");
        if look(entry, agent, &procs, quiet, now).live {
            return Err(fail("The refactor's agent is still running."));
        }
        let record = entry.record.clone();
        Ok(self.job(repo, now, Kind::Discard(record)))
    }

    /// Checks a removal of the linked worktree at `path` (`removeWorktree`), and returns
    /// the job that checks every rule again and removes it (spec §5 in
    /// `.plans/worktrees-spec.md`). The job is keyed by the worktree's main repo, so it
    /// waits for no other job and none starts for that repo while it runs (rule 10).
    pub fn remove_worktree(
        &mut self,
        agent: &Agent,
        path: &str,
        now: DateTime<Utc>,
    ) -> Result<Job, Fail> {
        let given = PathBuf::from(path);
        let main = worktree::main_of(&given)?;
        let repo = agent.known_form(&main);
        if self.busy.contains(&repo) {
            return Err(worktree::refuse(
                "This repo is busy with another action; try again in a moment.",
            ));
        }
        let agent_live = agent
            .repo_paths()
            .find(|p| paths_match(&path_str(p), path))
            .and_then(|p| agent.repo(p))
            .is_some_and(|r| {
                r.snapshot
                    .sessions
                    .iter()
                    .any(|s| matches!(s.state, SessionState::Live(_)))
            });
        let removal = worktree::Removal {
            path: given,
            refactor_worktrees: self
                .entries
                .values()
                .map(|e| PathBuf::from(&e.record.worktree))
                .collect(),
            refactor_repos: self
                .entries
                .iter()
                .flat_map(|(repo, e)| [repo.clone(), PathBuf::from(&e.record.repo)])
                .collect(),
            known_repos: agent.repo_paths().cloned().collect(),
            agent_live,
            refactor_dir: Some(self.worktrees_dir.clone()),
        };
        Ok(self.job(repo, now, Kind::RemoveWorktree(Box::new(removal))))
    }

    /// Takes in what a job did, and returns the reply and the events for the repo.
    pub fn finish(
        &mut self,
        agent: &mut Agent,
        done: Done,
        now: DateTime<Utc>,
    ) -> Result<(Reply, Vec<Event>), Fail> {
        let repo = done.repo;
        self.busy.remove(&repo);
        match done.kind {
            DoneKind::Started(Ok(started)) => {
                self.entries.insert(
                    repo.clone(),
                    Entry {
                        id: started.id,
                        record: started.record,
                        git_dir: Some(started.git_dir),
                        info: None,
                        live_ids: Vec::new(),
                        waiting: false,
                    },
                );
                let events = self.refresh(agent, Some(&[repo]), now);
                Ok((Reply::Launch(started.ticket), events))
            }
            DoneKind::Merged(Ok(Merged::Blocked {
                reason: MergeBlockReason::Live,
                files,
            })) => {
                // Not stored: the agent's state shows it.
                let outcome = MergeOutcome::Blocked {
                    reason: MergeBlockReason::Live,
                    files,
                };
                let events = self.refresh(agent, Some(std::slice::from_ref(&repo)), now);
                Ok((Reply::RefactorMerge(outcome), events))
            }
            DoneKind::Merged(Ok(Merged::Blocked { reason, files })) => {
                let (outcome, events) = self.blocked(agent, &repo, reason, files, now)?;
                Ok((Reply::RefactorMerge(outcome), events))
            }
            DoneKind::Merged(Ok(Merged::Done {
                commits,
                kept,
                leftover: None,
            })) => {
                self.forget(&repo);
                let mut events = agent.set_refactor(&repo, None);
                events.extend(agent.refresh(&repo, Refresh::ALL));
                Ok((
                    Reply::RefactorMerge(MergeOutcome::Merged { commits, kept }),
                    events,
                ))
            }
            DoneKind::Merged(Ok(Merged::Done {
                commits,
                kept,
                leftover: Some(why),
            })) => {
                // Kept, so only a discard is offered and adoption leaves it alone.
                if let Some(entry) = self.entries.get_mut(&repo) {
                    let mut record = entry.record.clone();
                    record.leftover = true;
                    record.leftover_reason = Some(why);
                    record.blocked = None;
                    record.blocked_head = None;
                    if let Err(e) = write_record(&self.records_dir, &entry.id, &record) {
                        eprintln!("refract-agent: cannot save the refactor's record: {e}");
                    }
                    entry.record = record;
                }
                let mut events = self.refresh(agent, Some(std::slice::from_ref(&repo)), now);
                events.extend(agent.refresh(&repo, Refresh::ALL));
                Ok((
                    Reply::RefactorMerge(MergeOutcome::Merged { commits, kept }),
                    events,
                ))
            }
            DoneKind::Discarded(Ok(())) => {
                self.forget(&repo);
                let mut events = agent.set_refactor(&repo, None);
                events.extend(agent.refresh(&repo, Refresh::ALL));
                Ok((Reply::Done, events))
            }
            // The main repo, when known, is sent again whether or not its snapshot moved;
            // the caller searches the repos again, which drops the worktree.
            DoneKind::WorktreeRemoved(Ok(())) => {
                let mut events = agent.refresh(&repo, Refresh::ALL);
                let main = path_str(&repo);
                let sent = events
                    .iter()
                    .any(|e| matches!(e, Event::RepoChanged(s) if s.path == main));
                if let Some(state) = agent.repo(&repo).filter(|_| !sent) {
                    events.push(Event::RepoChanged(Box::new(state.snapshot.clone())));
                }
                Ok((Reply::Done, events))
            }
            DoneKind::Started(Err(e))
            | DoneKind::Merged(Err(e))
            | DoneKind::Discarded(Err(e))
            | DoneKind::WorktreeRemoved(Err(e))
            | DoneKind::Failed(e) => Err(e),
        }
    }

    /// Brings the records in line with git, once the repos are loaded: a record whose
    /// repo is gone (or no longer a repo), or whose worktree is gone (not listed, or its
    /// folder deleted), is dropped, with nothing on disk deleted; one of a repo that is
    /// there but not known is kept, inactive. A refactor worktree of a known repo without
    /// a record is adopted. Returns the events for the repos' snapshots.
    pub fn reconcile(&mut self, agent: &mut Agent, now: DateTime<Utc>) -> Vec<Event> {
        let repos: Vec<PathBuf> = self.entries.keys().cloned().collect();
        for repo in repos {
            if self.busy.contains(&repo) {
                continue;
            }
            if !repo.is_dir() || scan::git_dir(&repo).is_none() {
                eprintln!(
                    "refract-agent: the repo {} of a refactor is gone; forgetting it",
                    repo.display()
                );
                self.forget(&repo);
                continue;
            }
            if agent.repo(&repo).is_none() {
                continue;
            }
            let worktree = PathBuf::from(&self.entries[&repo].record.worktree);
            let Ok(list) = worktrees(&repo) else {
                continue;
            };
            let listed = list.iter().any(|w| same_dir(&w.path, &worktree));
            if !listed || !worktree.is_dir() {
                eprintln!(
                    "refract-agent: the refactor worktree {} is gone; forgetting it",
                    worktree.display()
                );
                self.forget(&repo);
                agent.set_refactor(&repo, None);
            }
        }
        let root = fs::canonicalize(&self.worktrees_dir).ok();
        let known: Vec<PathBuf> = agent.repo_paths().cloned().collect();
        for repo in known {
            if self.entries.contains_key(&repo) || self.busy.contains(&repo) {
                continue;
            }
            let Some(root) = &root else {
                break;
            };
            if !has_linked_worktrees(&repo) {
                continue;
            }
            let Ok(list) = worktrees(&repo) else {
                continue;
            };
            let found = list.iter().skip(1).find(|w| {
                w.branch
                    .as_deref()
                    .is_some_and(|b| b.starts_with(rules::BRANCH_PREFIX))
                    && fs::canonicalize(&w.path).is_ok_and(|p| p.starts_with(root) && &p != root)
                    && !self
                        .entries
                        .values()
                        .any(|e| same_dir(Path::new(&e.record.worktree), &w.path))
            });
            if let Some(w) = found {
                self.adopt(agent, &repo, w, now);
            }
        }
        self.refresh(agent, None, now)
    }

    fn adopt(&mut self, agent: &mut Agent, repo: &Path, w: &Worktree, now: DateTime<Utc>) {
        let (Some(branch), Some(target)) = (w.branch.clone(), current_branch(repo)) else {
            return;
        };
        let base = run_git(
            repo,
            &[
                "merge-base",
                &format!("refs/heads/{target}"),
                &format!("refs/heads/{branch}"),
            ],
        )
        .ok()
        .filter(|o| o.ok)
        .map(|o| o.stdout.trim().to_owned());
        let Some(base) = base else {
            return;
        };
        let worktree = path_str(&w.path);
        let session_id = agent
            .reader_mut()
            .read_repo(&worktree)
            .into_iter()
            .find_map(|s| s.session_id)
            .unwrap_or_default();
        let record = Record {
            repo: path_str(repo),
            worktree,
            branch,
            base_commit: base,
            target_branch: target,
            session_id,
            request_id: String::new(),
            focus: None,
            created_at: now.trunc_subsecs(0),
            launched_at: None,
            adopted: true,
            blocked: None,
            blocked_head: None,
            leftover: false,
            leftover_reason: None,
        };
        let id = uuid::Uuid::new_v4().to_string();
        if let Err(e) = write_record(&self.records_dir, &id, &record) {
            eprintln!("refract-agent: cannot save an adopted refactor's record: {e}");
            return;
        }
        eprintln!(
            "refract-agent: adopted the refactor worktree {}",
            record.worktree
        );
        self.entries.insert(
            repo.to_owned(),
            Entry {
                id,
                record,
                git_dir: None,
                info: None,
                live_ids: Vec::new(),
                waiting: false,
            },
        );
    }

    /// Works out again the refactors of `repos` (all of them for `None`) whose repo is
    /// known and whose job is not running, and returns the events for the snapshots
    /// that changed.
    pub fn refresh(
        &mut self,
        agent: &mut Agent,
        repos: Option<&[PathBuf]>,
        now: DateTime<Utc>,
    ) -> Vec<Event> {
        let paths: Vec<PathBuf> = match repos {
            Some(repos) => repos
                .iter()
                .filter(|r| self.entries.contains_key(*r))
                .cloned()
                .collect(),
            None => self.entries.keys().cloned().collect(),
        };
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|r| !self.busy.contains(r) && agent.repo(r).is_some())
            .collect();
        if paths.is_empty() {
            return Vec::new();
        }
        let procs = procs::scan();
        let times = (self.grace, self.quiet);
        let mut events = Vec::new();
        for repo in paths {
            let entry = self.entries.get_mut(&repo).expect("known");
            let info = work_out(entry, agent, &procs, self.distro.as_deref(), times, now);
            if let Some(cleared) = info.1 {
                let _ = write_record(&self.records_dir, &entry.id, &cleared);
                entry.record = cleared;
            }
            entry.info = Some(info.0.clone());
            events.extend(agent.set_refactor(&repo, Some(info.0)));
        }
        events
    }

    /// [`Refactors::refresh`] for the refactors of `repos`, and those whose agent is, or
    /// was last seen live as, one of `sessions` (whose hook statuses changed).
    pub fn refresh_touched(
        &mut self,
        agent: &mut Agent,
        repos: &[PathBuf],
        sessions: &[&str],
        now: DateTime<Utc>,
    ) -> Vec<Event> {
        let touched: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|(repo, e)| {
                repos.contains(repo)
                    || sessions.iter().any(|s| {
                        (!s.is_empty() && e.record.session_id == *s)
                            || e.live_ids.iter().any(|l| l == s)
                    })
            })
            .map(|(repo, _)| repo.clone())
            .collect();
        self.refresh(agent, Some(&touched), now)
    }

    /// Stores a blocked merge in the record and answers with it.
    fn blocked(
        &mut self,
        agent: &mut Agent,
        repo: &Path,
        reason: MergeBlockReason,
        files: Vec<String>,
        now: DateTime<Utc>,
    ) -> Result<(MergeOutcome, Vec<Event>), Fail> {
        let entry = self.entries.get_mut(repo).expect("known");
        let mut record = entry.record.clone();
        record.blocked = Some(MergeBlock {
            reason,
            files: files.clone(),
        });
        record.blocked_head = commit_of(repo, &format!("refs/heads/{}", record.branch));
        write_record(&self.records_dir, &entry.id, &record)
            .map_err(|e| fail(format!("cannot save the refactor's record: {e}")))?;
        entry.record = record;
        let events = self.refresh(agent, Some(&[repo.to_owned()]), now);
        Ok((MergeOutcome::Blocked { reason, files }, events))
    }

    /// Drops the refactor of `repo` and its record file.
    fn forget(&mut self, repo: &Path) {
        if let Some(entry) = self.entries.remove(repo) {
            let _ = fs::remove_file(self.records_dir.join(format!("{}.json", entry.id)));
        }
    }
}

/// The pids of the `claude` processes running in `worktree`.
fn inside(procs: &[ClaudeProcess], worktree: &str) -> Vec<u32> {
    procs
        .iter()
        .filter(|p| path_within(&p.cwd, worktree))
        .map(|p| p.pid)
        .collect()
}

/// True when a `claude` process other than `known` runs in `worktree` now.
fn newcomer(worktree: &Path, known: &[u32]) -> bool {
    inside(&procs::scan(), &path_str(worktree))
        .iter()
        .any(|pid| !known.contains(pid))
}

/// Refuses unless `worktree` really lies under `worktrees_dir` and the repo's `git
/// worktree list` names it as a linked worktree.
fn check_safe(worktrees_dir: &Path, repo: &Path, worktree: &Path) -> Result<(), Fail> {
    let refuse = || {
        fail(format!(
            "{} is not a worktree Refract made for this repo, so it is left alone.",
            worktree.display()
        ))
    };
    let root = fs::canonicalize(worktrees_dir).map_err(|_| refuse())?;
    let real = fs::canonicalize(worktree).map_err(|_| refuse())?;
    if !real.starts_with(&root) || real == root {
        return Err(refuse());
    }
    let list = worktrees(repo).map_err(|e| fail(format!("cannot list worktrees: {e}")))?;
    if !list.iter().skip(1).any(|w| same_dir(&w.path, &real)) {
        return Err(refuse());
    }
    Ok(())
}

fn no_refactor() -> Fail {
    fail("This repo has no refactor.")
}

/// What the refactor's agent is doing.
struct Look {
    /// The recorded session's process runs, or some other `claude` runs in the worktree.
    live: bool,
    /// Live, only as the recorded session, and waiting for a prompt: its hook says its
    /// turn finished or, without a hook status, its transcript ends with a finished turn
    /// and has been quiet for a while.
    idle: bool,
    /// The recorded session's agent waits on the user.
    waiting: bool,
    /// The agent's transcript, if it wrote one.
    summary: Option<SessionSummary>,
    /// The recorded session's state.
    state: Option<SessionState>,
    /// The sessions live in the worktree.
    live_ids: Vec<String>,
}

impl Look {
    /// Live and not idle: a merge would race the agent.
    fn busy(&self) -> bool {
        self.live && !self.idle
    }
}

fn look(
    entry: &Entry,
    agent: &mut Agent,
    procs: &[ClaudeProcess],
    quiet: TimeDelta,
    now: DateTime<Utc>,
) -> Look {
    let id = entry.record.session_id.as_str();
    let worktree = entry.record.worktree.as_str();
    let sessions = agent.reader_mut().read_repo(worktree);
    let pairs: Vec<(&str, Option<DateTime<Utc>>)> = sessions
        .iter()
        .filter_map(|s| Some((s.session_id.as_deref()?, s.modified.or(s.last_timestamp))))
        .collect();
    let claimed = procs::live_sessions(procs, worktree, &pairs);
    // A fresh `--session-id` process has no transcript yet, but names the session.
    let named = !id.is_empty() && procs.iter().any(|p| p.session_id.as_deref() == Some(id));
    let ours = !id.is_empty() && (named || claimed.contains_key(id));
    // Any other `claude` in the worktree: what it does is not known, so it is busy.
    let inside: Vec<&ClaudeProcess> = procs
        .iter()
        .filter(|p| path_within(&p.cwd, worktree))
        .collect();
    let mut others = inside
        .iter()
        .filter(|p| id.is_empty() || p.session_id.as_deref() != Some(id))
        .count();
    if ours && !named && others > 0 {
        // The recorded session runs in a process that names no session.
        others -= 1;
    }
    let mut live_ids: Vec<String> = claimed.keys().cloned().collect();
    live_ids.extend(inside.iter().filter_map(|p| p.session_id.clone()));
    if ours {
        live_ids.push(id.to_owned());
    }
    live_ids.sort();
    live_ids.dedup();
    let summary = (!id.is_empty())
        .then(|| {
            sessions
                .iter()
                .find(|s| s.session_id.as_deref() == Some(id))
                .cloned()
        })
        .flatten();
    let hook = (!id.is_empty())
        .then(|| agent.hooks().get(id).copied())
        .flatten();
    // Background agents of the refactor's agent work in the worktree too.
    let background = if ours {
        let process = procs
            .iter()
            .find(|p| p.session_id.as_deref() == Some(id))
            .map(|p| LiveProcess {
                pid: p.pid,
                started: p.started,
                refract_folder: p.refract_folder.clone(),
            })
            .or_else(|| claimed.get(id).cloned().flatten());
        agent.background_count(procs, worktree, id, process.as_ref())
    } else {
        0
    };
    let state = (!id.is_empty()).then(|| {
        classify_report(
            summary.as_ref().unwrap_or(&SessionSummary::default()),
            ours,
            hook,
            background,
        )
    });
    let finished = |s: &SessionSummary| {
        matches!(
            classify_report(s, false, None, 0),
            SessionState::Finished(_)
        )
    };
    let idle = others == 0
        && match state {
            Some(SessionState::Live(LiveState::Idle)) => true,
            Some(SessionState::Live(LiveState::Unknown)) => summary.as_ref().is_some_and(|s| {
                background == 0
                    && finished(s)
                    && (hook.is_some() || s.modified.is_some_and(|m| now - m >= quiet))
            }),
            _ => false,
        };
    Look {
        live: ours || others > 0,
        idle,
        waiting: state == Some(SessionState::Live(LiveState::WaitingOnUser)),
        summary,
        state,
        live_ids,
    }
}

/// The refactor's info, and its record with `blocked` cleared when the branch moved on
/// since the merge stopped. `times` are the start grace and the idle quiet time.
fn work_out(
    entry: &mut Entry,
    agent: &mut Agent,
    procs: &[ClaudeProcess],
    distro: Option<&str>,
    times: (TimeDelta, TimeDelta),
    now: DateTime<Utc>,
) -> (RefactorInfo, Option<Record>) {
    let (grace, quiet) = times;
    let look = look(entry, agent, procs, quiet, now);
    entry.live_ids = look.live_ids.clone();
    entry.waiting = look.waiting;
    let record = &entry.record;
    let repo = Path::new(&record.repo);
    let worktree = Path::new(&record.worktree);
    let branch_ref = format!("refs/heads/{}", record.branch);
    let target_ref = format!("refs/heads/{}", record.target_branch);
    let from = if commit_of(repo, &target_ref).is_some() {
        target_ref
    } else {
        record.base_commit.clone()
    };
    let head = commit_of(repo, &branch_ref);
    let commits = match &head {
        Some(_) => commits(repo, &from, &branch_ref),
        None => Vec::new(),
    };
    let (files_changed, insertions, deletions) = match &head {
        Some(_) => shortstat(repo, &from, &branch_ref),
        None => (0, 0, 0),
    };
    if entry.git_dir.is_none() && worktree.is_dir() {
        entry.git_dir = absolute_git_dir(worktree);
    }
    let result = entry
        .git_dir
        .as_deref()
        .and_then(|d| read_result(&d.join(rules::RESULT_FILE)));
    let mut cleared = None;
    let mut blocked = record.blocked.clone();
    if blocked.is_some() && record.blocked_head != head {
        blocked = None;
        let mut fresh = record.clone();
        fresh.blocked = None;
        fresh.blocked_head = None;
        cleared = Some(fresh);
    }
    let interrupted = !look.live && look.state.is_some_and(SessionState::is_unfinished);
    let launched = record.launched_at.unwrap_or(record.created_at);
    let state = if record.leftover {
        RefactorState::NothingToDo
    } else {
        derive_state(&Facts {
            live: look.live,
            idle: look.idle,
            waiting: look.waiting,
            blocked: blocked.is_some(),
            commits: commits.len(),
            transcript: look.summary.is_some(),
            fresh: !record.adopted && now - launched < grace,
            nothing_reported: result.as_ref().is_some_and(|r| r.refactors.is_empty()),
        })
    };
    let summary = if record.leftover {
        Some(format!(
            "Merged; the worktree could not be removed: {}",
            record.leftover_reason.as_deref().unwrap_or("git refused")
        ))
    } else {
        result.as_ref().and_then(|r| r.summary.clone())
    };
    let info = RefactorInfo {
        state,
        worktree: record.worktree.clone(),
        folder_uri: folder_uri(distro.unwrap_or_default(), &record.worktree),
        branch: record.branch.clone(),
        target_branch: record.target_branch.clone(),
        base_commit: record.base_commit.clone(),
        session_id: record.session_id.clone(),
        created_at: record.created_at,
        live: look.live,
        interrupted: interrupted && !record.leftover,
        commits,
        files_changed,
        insertions,
        deletions,
        checks: result.map(|r| r.checks),
        summary,
        blocked: blocked.filter(|_| !record.leftover),
    };
    (info, cleared)
}

/// What a refactor's state is decided from.
#[derive(Debug, Clone, Copy, Default)]
pub struct Facts {
    /// The agent's process runs.
    pub live: bool,
    /// It runs and waits for a prompt, its turn over.
    pub idle: bool,
    /// It waits on the user (hook `waitingOnUser`).
    pub waiting: bool,
    /// The last merge stopped and nothing moved since.
    pub blocked: bool,
    /// Commits on the branch not on the target.
    pub commits: usize,
    /// The agent's session wrote a transcript.
    pub transcript: bool,
    /// Launched less than [`START_GRACE`] ago.
    pub fresh: bool,
    /// The result file is valid and lists no refactors.
    pub nothing_reported: bool,
}

/// The state (spec §4): an agent at work is working or waits on you; then a stopped
/// merge, commits to merge, a launch still pending, nothing to do, or interrupted. An
/// agent that is still open but idle (its turn over) counts as stopped, except that
/// with nothing to show it waits for you.
pub fn derive_state(f: &Facts) -> RefactorState {
    if f.live && !f.idle {
        return if f.waiting {
            RefactorState::NeedsYou
        } else {
            RefactorState::Working
        };
    }
    if f.blocked {
        return RefactorState::MergeBlocked;
    }
    if f.commits > 0 {
        return RefactorState::Ready;
    }
    if f.live {
        return if f.nothing_reported {
            RefactorState::NothingToDo
        } else {
            RefactorState::NeedsYou
        };
    }
    if !f.transcript {
        return if f.fresh {
            RefactorState::Starting
        } else {
            RefactorState::Interrupted
        };
    }
    if f.nothing_reported {
        RefactorState::NothingToDo
    } else {
        RefactorState::Interrupted
    }
}

/// The result file, if it is there, small enough and valid.
fn read_result(path: &Path) -> Option<ResultFile> {
    let file = fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > rules::MAX_RESULT_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(rules::MAX_RESULT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    rules::parse_result(&bytes)
}

/// Every `<uuid>.json` record in `dir`.
fn read_records(dir: &Path) -> Vec<(String, Record)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if uuid::Uuid::try_parse(id).is_err() {
            continue;
        }
        match fs::read(entry.path()).map(|b| serde_json::from_slice::<Record>(&b)) {
            Ok(Ok(record)) => records.push((id.to_owned(), record)),
            Ok(Err(e)) => eprintln!("refract-agent: bad refactor record {name}: {e}"),
            Err(e) => eprintln!("refract-agent: cannot read refactor record {name}: {e}"),
        }
    }
    records
}

/// Writes `<id>.json` through a temporary file, so a reader never sees half of it. A new
/// records directory is private to the user (mode 0700), since records hold the focus.
fn write_record(dir: &Path, id: &str, record: &Record) -> io::Result<()> {
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let tmp = dir.join(format!(".{id}.tmp"));
    let mut file = fs::File::create(&tmp)?;
    let written = serde_json::to_writer_pretty(&mut file, record)
        .map_err(io::Error::from)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all());
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, dir.join(format!("{id}.json"))).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })
}

/// What git printed, and whether it succeeded.
pub(crate) struct Out {
    pub(crate) ok: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// Runs git in `dir`, looking no higher than it for a repo, never prompting and never
/// opening an editor.
pub(crate) fn run_git(dir: &Path, args: &[&str]) -> io::Result<Out> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .stdin(Stdio::null());
    if let Some(parent) = dir.parent() {
        command.env("GIT_CEILING_DIRECTORIES", parent);
    }
    let out = command.output()?;
    Ok(Out {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Read-only git, without optional locks.
fn read_git(dir: &Path, args: &[&str]) -> Option<String> {
    let mut all = vec!["--no-optional-locks"];
    all.extend_from_slice(args);
    run_git(dir, &all).ok().filter(|o| o.ok).map(|o| o.stdout)
}

fn lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The full id of the commit `rev` names.
fn commit_of(repo: &Path, rev: &str) -> Option<String> {
    let spec = format!("{rev}^{{commit}}");
    read_git(repo, &["rev-parse", "--verify", "-q", &spec]).map(|s| s.trim().to_owned())
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    commit_of(repo, &format!("refs/heads/{branch}")).is_some()
}

/// The full ref HEAD names in `dir`; `None` on a detached HEAD.
fn symbolic_head(dir: &Path) -> Option<String> {
    read_git(dir, &["symbolic-ref", "-q", "HEAD"])
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// The branch checked out in `repo`, without `refs/heads/`; `None` on a detached HEAD.
fn current_branch(repo: &Path) -> Option<String> {
    symbolic_head(repo)?
        .strip_prefix("refs/heads/")
        .filter(|b| !b.is_empty())
        .map(str::to_owned)
}

/// True when a rebase is under way in the worktree whose git directory is `git_dir`.
fn rebase_under_way(git_dir: &Path) -> bool {
    ["rebase-merge", "rebase-apply"]
        .iter()
        .any(|d| fs::symlink_metadata(git_dir.join(d)).is_ok())
}

fn absolute_git_dir(worktree: &Path) -> Option<PathBuf> {
    read_git(worktree, &["rev-parse", "--absolute-git-dir"]).map(|s| PathBuf::from(s.trim()))
}

fn is_ancestor(repo: &Path, ancestor: &str, of: &str) -> bool {
    run_git(repo, &["merge-base", "--is-ancestor", ancestor, of]).is_ok_and(|o| o.ok)
}

fn count(repo: &Path, range: &str) -> u32 {
    read_git(repo, &["rev-list", "--count", range])
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Commits in `from..to`, oldest first.
fn commits(repo: &Path, from: &str, to: &str) -> Vec<RefactorCommit> {
    let range = format!("{from}..{to}");
    read_git(repo, &["log", "--reverse", "--format=%h%x00%s", &range])
        .map(|out| {
            out.lines()
                .filter_map(|l| {
                    let (sha, summary) = l.split_once('\0')?;
                    Some(RefactorCommit {
                        sha: sha.to_owned(),
                        summary: summary.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Files changed, insertions and deletions from where `from` and `to` part to `to`.
fn shortstat(repo: &Path, from: &str, to: &str) -> (u32, u32, u32) {
    let range = format!("{from}...{to}");
    read_git(repo, &["diff", "--shortstat", &range])
        .map(|out| parse_shortstat(&out))
        .unwrap_or_default()
}

/// Parses `git diff --shortstat`: ` 3 files changed, 10 insertions(+), 2 deletions(-)`.
pub fn parse_shortstat(out: &str) -> (u32, u32, u32) {
    let mut stat = (0, 0, 0);
    for part in out.trim().split(',') {
        let mut words = part.split_whitespace();
        let (Some(n), Some(what)) = (words.next(), words.next()) else {
            continue;
        };
        let Ok(n) = n.parse() else {
            continue;
        };
        if what.starts_with("file") {
            stat.0 = n;
        } else if what.starts_with("insertion") {
            stat.1 = n;
        } else if what.starts_with("deletion") {
            stat.2 = n;
        }
    }
    stat
}

/// Paths the refactor changed (both sides of a rename), relative to the repo.
fn changed_files(repo: &Path, target: &str, branch: &str) -> Result<Vec<String>, Fail> {
    let range = format!("{target}...{branch}");
    read_git(repo, &["diff", "--name-only", "--no-renames", "-z", &range])
        .map(|out| {
            out.split('\0')
                .filter(|p| !p.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .ok_or_else(|| fail("cannot list the files the refactor changed"))
}

/// Files with changes in the worktree, both paths of a rename or copy, and untracked
/// files that are not ignored.
fn changed_in_worktree(worktree: &Path) -> Result<Vec<String>, Fail> {
    let out = read_git(
        worktree,
        &[
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=dirty",
        ],
    )
    .ok_or_else(|| fail("cannot read the worktree's status"))?;
    Ok(parse_porcelain(&out))
}

/// The paths in `git status --porcelain -z`, with the original path of each rename or
/// copy (the record after it).
pub fn parse_porcelain(out: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut records = out.split('\0').filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        let (Some(xy), Some(path)) = (record.get(..2), record.get(3..)) else {
            continue;
        };
        paths.push(path.to_owned());
        if xy.contains(['R', 'C']) {
            if let Some(origin) = records.next() {
                paths.push(origin.to_owned());
            }
        }
    }
    paths
}

/// The uncommitted paths in `repo` that are, or hold, files the refactor changed:
/// changed, staged (both paths of a rename) and untracked ones. Where `git status`
/// leaves untracked files out (a Windows drive), they are looked for among the
/// refactor's paths only.
fn dirty_overlap(repo: &Path, target: &str, branch: &str) -> Result<Vec<String>, Fail> {
    let status =
        git::read_status(repo).map_err(|e| fail(format!("cannot read the repo's status: {e}")))?;
    let changed = changed_files(repo, target, branch)?;
    let mut dirty = status.paths;
    dirty.extend(status.origins);
    if git::untracked_arg(repo) == "-uno" && !changed.is_empty() {
        let mut args = vec![
            "--literal-pathspecs",
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--",
        ];
        args.extend(changed.iter().map(String::as_str));
        let out =
            read_git(repo, &args).ok_or_else(|| fail("cannot list the repo's untracked files"))?;
        dirty.extend(out.split('\0').filter(|p| !p.is_empty()).map(str::to_owned));
    }
    Ok(overlap(&dirty, &changed))
}

/// The uncommitted paths that are, or hold, files the refactor changed, sorted.
pub fn overlap(dirty: &[String], changed: &[String]) -> Vec<String> {
    let mut clash: Vec<String> = dirty
        .iter()
        .filter(|d| {
            changed
                .iter()
                .any(|c| c == *d || (d.ends_with('/') && c.starts_with(d.as_str())))
        })
        .cloned()
        .collect();
    clash.sort();
    clash.dedup();
    clash
}

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone)]
pub(crate) struct Worktree {
    pub(crate) path: PathBuf,
    /// The branch checked out, without `refs/heads/`.
    pub(crate) branch: Option<String>,
    /// `git worktree lock` was used on it.
    pub(crate) locked: bool,
    /// Git would prune it: its folder or `.git` file is gone.
    pub(crate) prunable: bool,
}

/// The repo's worktrees, the main one first.
pub(crate) fn worktrees(repo: &Path) -> io::Result<Vec<Worktree>> {
    let out = run_git(
        repo,
        &["--no-optional-locks", "worktree", "list", "--porcelain"],
    )?;
    if !out.ok {
        return Err(io::Error::other(out.stderr.trim().to_owned()));
    }
    let mut list: Vec<Worktree> = Vec::new();
    for line in out.stdout.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            list.push(Worktree {
                path: PathBuf::from(path),
                branch: None,
                locked: false,
                prunable: false,
            });
        } else if let Some(last) = list.last_mut() {
            if let Some(branch) = line.strip_prefix("branch refs/heads/") {
                last.branch = Some(branch.to_owned());
            } else if line == "locked" || line.starts_with("locked ") {
                last.locked = true;
            } else if line == "prunable" || line.starts_with("prunable ") {
                last.prunable = true;
            }
        }
    }
    Ok(list)
}

/// The worktrees of `repo`, other than the repo itself and `own`, that have `target`
/// (a full ref) checked out, and any worktree, the repo's own too, that is rebasing it.
fn target_holders(repo: &Path, own: &Path, target: &str) -> Result<Vec<String>, Fail> {
    let list = worktrees(repo).map_err(|e| fail(format!("cannot list worktrees: {e}")))?;
    let branch = target.strip_prefix("refs/heads/").unwrap_or(target);
    let mut holders: Vec<String> = list
        .iter()
        .filter(|w| w.branch.as_deref() == Some(branch))
        .filter(|w| !same_dir(&w.path, repo) && !same_dir(&w.path, own))
        .map(|w| path_str(&w.path))
        .collect();
    // Each worktree's git directory: the common one for the main worktree, and
    // `worktrees/<name>` (whose `gitdir` names `<worktree>/.git`) for linked ones.
    let git_dir =
        scan::git_dir(repo).ok_or_else(|| fail("cannot find the repo's git directory"))?;
    let common = scan::common_dir(&git_dir).unwrap_or(git_dir);
    let mut dirs: Vec<(PathBuf, Option<PathBuf>)> =
        vec![(common.clone(), list.first().map(|w| w.path.clone()))];
    if let Ok(entries) = fs::read_dir(common.join("worktrees")) {
        for admin in entries.flatten().map(|e| e.path()) {
            let path = fs::read_to_string(admin.join("gitdir"))
                .ok()
                .and_then(|t| Path::new(t.trim()).parent().map(Path::to_owned));
            dirs.push((admin, path));
        }
    }
    for (dir, path) in dirs {
        if path.as_deref().is_some_and(|p| same_dir(p, own)) {
            continue;
        }
        let rebasing = ["rebase-merge", "rebase-apply"].iter().any(|d| {
            fs::read_to_string(dir.join(d).join("head-name")).is_ok_and(|n| n.trim() == target)
        });
        if rebasing {
            holders.push(path.map_or_else(|| path_str(&dir), |p| path_str(&p)));
        }
    }
    holders.sort();
    holders.dedup();
    Ok(holders)
}

/// True when the repo's git directory has linked worktrees, without running git.
fn has_linked_worktrees(repo: &Path) -> bool {
    let Some(git_dir) = scan::git_dir(repo) else {
        return false;
    };
    let common = scan::common_dir(&git_dir).unwrap_or(git_dir);
    fs::read_dir(common.join("worktrees")).is_ok_and(|mut d| d.next().is_some())
}

/// True when two paths name the same directory, by their real paths when both exist.
pub(crate) fn same_dir(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => paths_match(&path_str(a), &path_str(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_follow_the_spec_table() {
        let f = Facts::default();
        let state = |f: Facts| derive_state(&f);
        assert_eq!(state(Facts { fresh: true, ..f }), RefactorState::Starting);
        assert_eq!(state(f), RefactorState::Interrupted);
        assert_eq!(state(Facts { live: true, ..f }), RefactorState::Working);
        let idle = Facts {
            live: true,
            idle: true,
            transcript: true,
            ..f
        };
        assert_eq!(state(Facts { commits: 2, ..idle }), RefactorState::Ready);
        assert_eq!(state(idle), RefactorState::NeedsYou);
        assert_eq!(
            state(Facts {
                nothing_reported: true,
                ..idle
            }),
            RefactorState::NothingToDo
        );
        assert_eq!(
            state(Facts {
                live: true,
                waiting: true,
                commits: 2,
                blocked: true,
                ..f
            }),
            RefactorState::NeedsYou
        );
        assert_eq!(
            state(Facts {
                blocked: true,
                commits: 2,
                ..f
            }),
            RefactorState::MergeBlocked
        );
        assert_eq!(
            state(Facts {
                commits: 1,
                transcript: true,
                ..f
            }),
            RefactorState::Ready
        );
        assert_eq!(state(Facts { commits: 1, ..f }), RefactorState::Ready);
        assert_eq!(
            state(Facts {
                transcript: true,
                nothing_reported: true,
                ..f
            }),
            RefactorState::NothingToDo
        );
        assert_eq!(
            state(Facts {
                transcript: true,
                fresh: true,
                ..f
            }),
            RefactorState::Interrupted
        );
    }

    #[test]
    fn shortstats_parse() {
        assert_eq!(
            parse_shortstat(" 3 files changed, 10 insertions(+), 2 deletions(-)\n"),
            (3, 10, 2)
        );
        assert_eq!(parse_shortstat(" 1 file changed, 1 deletion(-)"), (1, 0, 1));
        assert_eq!(parse_shortstat(""), (0, 0, 0));
    }

    #[test]
    fn porcelain_names_both_paths_of_a_rename() {
        let out = "R  src/new.rs\0src/old.rs\0 M README.md\0C  b.rs\0a.rs\0";
        assert_eq!(
            parse_porcelain(out),
            ["src/new.rs", "src/old.rs", "README.md", "b.rs", "a.rs"]
        );
    }

    #[test]
    fn overlap_names_dirty_files_the_refactor_changed() {
        let dirty = [
            "src/lib.rs".to_owned(),
            "notes.txt".to_owned(),
            "gen/".to_owned(),
        ];
        let changed = [
            "src/lib.rs".to_owned(),
            "gen/out.rs".to_owned(),
            "README.md".to_owned(),
        ];
        assert_eq!(overlap(&dirty, &changed), ["gen/", "src/lib.rs"]);
        assert!(overlap(&dirty, &["src/main.rs".to_owned()]).is_empty());
    }
}
