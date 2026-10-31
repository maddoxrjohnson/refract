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

use std::collections::BTreeMap;
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
    RefactorCommit, RefactorInfo, RefactorState,
};
use refract_core::refactor::{self as rules, ResultFile};
use refract_core::sessions::paths_match;
use refract_core::transcript::SessionSummary;
use serde::{Deserialize, Serialize};

use crate::git;
use crate::launch;
use crate::procs::{self, ClaudeProcess};
use crate::scan;
use crate::state::{path_str, Agent, Refresh};

/// A refactor with no agent and no transcript this long after its launch counts as
/// interrupted rather than starting.
pub const START_GRACE: TimeDelta = TimeDelta::minutes(2);

/// How often refactors are looked at again while one is starting or its agent runs.
pub const ACTIVE_POLL: Duration = Duration::from_secs(3);

/// How often they are looked at otherwise.
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
    #[serde(default)]
    pub focus: Option<String>,
    pub created_at: DateTime<Utc>,
    /// When an agent was last launched for it; `createdAt` until it is resumed.
    #[serde(default)]
    pub launched_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub blocked: Option<MergeBlock>,
    /// The branch's commit when the merge stopped: a new commit clears `blocked`.
    #[serde(default)]
    pub blocked_head: Option<String>,
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
}

/// Every refactor, by repo path.
#[derive(Debug)]
pub struct Refactors {
    records_dir: PathBuf,
    worktrees_dir: PathBuf,
    distro: Option<String>,
    entries: BTreeMap<PathBuf, Entry>,
}

type Fail = ErrorBody;

fn fail(message: impl Into<String>) -> Fail {
    ErrorBody {
        code: code::REFACTOR_FAILED,
        message: message.into(),
    }
}

impl Refactors {
    /// Records in `<state dir>/refactors`, worktrees in `$HOME/projects/.refract-worktrees`,
    /// folder URIs for `WSL_DISTRO_NAME`.
    pub fn from_env() -> Refactors {
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        Refactors::new(
            launch::state_dir().join("refactors"),
            home.join(rules::WORKTREES_DIR),
            std::env::var("WSL_DISTRO_NAME")
                .ok()
                .filter(|d| !d.is_empty()),
        )
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
            };
            entries.insert(repo, entry);
        }
        Refactors {
            records_dir,
            worktrees_dir,
            distro,
            entries,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The record of the repo at `path`, if it has a refactor.
    pub fn record(&self, path: &str) -> Option<&Record> {
        Some(&self.entries.get(&self.key(path)?)?.record)
    }

    /// How long until the refactors should be looked at again; `None` without any. Often
    /// while an agent was just launched, runs, or is starting.
    pub fn poll_interval(&self) -> Option<Duration> {
        if self.entries.is_empty() {
            return None;
        }
        let now = Utc::now();
        let active = self.entries.values().any(|e| {
            let launched = e.record.launched_at.unwrap_or(e.record.created_at);
            now - launched < START_GRACE
                || e.info.as_ref().is_none_or(|i| {
                    i.live
                        || matches!(
                            i.state,
                            RefactorState::Starting
                                | RefactorState::Working
                                | RefactorState::NeedsYou
                        )
                })
        });
        Some(if active { ACTIVE_POLL } else { IDLE_POLL })
    }

    fn key(&self, path: &str) -> Option<PathBuf> {
        self.entries
            .keys()
            .find(|k| paths_match(&path_str(k), path))
            .cloned()
    }

    /// Starts a refactor of the repo at `path`: worktree, branch, launch request, record.
    /// Returns the ticket and the events for the repo's new snapshot.
    pub fn start(
        &mut self,
        agent: &mut Agent,
        path: &str,
        focus: Option<&str>,
        launch_dir: &Path,
        now: DateTime<Utc>,
    ) -> Result<(LaunchTicket, Vec<Event>), Fail> {
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
        let Some(base) = commit_of(&repo, "HEAD") else {
            return Err(fail("The repo has no commits yet."));
        };
        let Some(target) = current_branch(&repo) else {
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
            .find(|b| !branch_exists(&repo, b))
            .expect("a free branch");
        let wt = path_str(&worktree);
        let added = run_git(&repo, &["worktree", "add", "-q", "-b", &branch, &wt, &base])
            .map_err(|e| fail(format!("cannot run git: {e}")))?;
        if !added.ok {
            return Err(fail(format!(
                "git worktree add failed: {}",
                added.stderr.trim()
            )));
        }
        // From here on, a failure takes back the worktree and branch just made.
        let undo = |why: Fail| {
            let _ = run_git(&repo, &["worktree", "remove", "--force", &wt]);
            let _ = run_git(&repo, &["branch", "-D", &branch]);
            why
        };
        let git_dir = absolute_git_dir(&worktree)
            .ok_or_else(|| undo(fail("cannot find the new worktree's git directory")))?;
        let result_path = path_str(&git_dir.join(rules::RESULT_FILE));
        let focus = focus.and_then(rules::clean_focus);
        let mode = LaunchMode::New {
            prompt: Some(rules::prompt(&branch, &result_path, focus.as_deref())),
            plan_mode: true,
        };
        let ticket = launch::prepare_titled(
            &wt,
            &mode,
            rules::TITLE,
            launch_dir,
            self.distro.as_deref(),
            now,
        )
        .map_err(undo)?;
        let record = Record {
            repo: path_str(&repo),
            worktree: wt.clone(),
            branch: branch.clone(),
            base_commit: base,
            target_branch: target,
            session_id: ticket.session_id.clone(),
            focus,
            created_at: now.trunc_subsecs(0),
            launched_at: None,
            blocked: None,
            blocked_head: None,
        };
        let id = uuid::Uuid::new_v4().to_string();
        write_record(&self.records_dir, &id, &record)
            .map_err(|e| undo(fail(format!("cannot save the refactor's record: {e}"))))?;
        self.entries.insert(
            repo.clone(),
            Entry {
                id,
                record,
                git_dir: Some(git_dir),
                info: None,
            },
        );
        let events = self.refresh(agent, Some(&[repo]), now);
        Ok((ticket, events))
    }

    /// Launches the refactor's agent again: resumed with [`rules::CONTINUE_PROMPT`], or
    /// with the fix prompt when `fix`; started anew with the original prompt (or the fix
    /// prompt) when its session never wrote a transcript.
    pub fn resume(
        &mut self,
        agent: &mut Agent,
        path: &str,
        fix: bool,
        launch_dir: &Path,
        now: DateTime<Utc>,
    ) -> Result<(LaunchTicket, Vec<Event>), Fail> {
        let repo = self.key(path).ok_or_else(no_refactor)?;
        let procs = procs::scan();
        let entry = self.entries.get_mut(&repo).expect("known");
        let worktree = PathBuf::from(&entry.record.worktree);
        if !worktree.is_dir() {
            return Err(fail("The refactor's worktree is gone."));
        }
        let look = look(entry, agent, &procs);
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
        let record = &entry.record;
        let fix_prompt = || rules::fix_prompt(&record.branch, &record.target_branch, &result_path);
        let mode = match (&look.summary, fix) {
            (Some(_), false) => LaunchMode::Resume {
                session_id: record.session_id.clone(),
                prompt: Some(rules::CONTINUE_PROMPT.into()),
            },
            (Some(_), true) => LaunchMode::Resume {
                session_id: record.session_id.clone(),
                prompt: Some(fix_prompt()),
            },
            (None, false) => LaunchMode::New {
                prompt: Some(rules::prompt(
                    &record.branch,
                    &result_path,
                    record.focus.as_deref(),
                )),
                plan_mode: true,
            },
            (None, true) => LaunchMode::New {
                prompt: Some(fix_prompt()),
                plan_mode: false,
            },
        };
        let ticket = launch::prepare_titled(
            &record.worktree,
            &mode,
            rules::TITLE,
            launch_dir,
            self.distro.as_deref(),
            now,
        )?;
        let mut record = record.clone();
        record.session_id = ticket.session_id.clone();
        record.launched_at = Some(now.trunc_subsecs(0));
        record.blocked = None;
        record.blocked_head = None;
        write_record(&self.records_dir, &entry.id, &record)
            .map_err(|e| fail(format!("cannot save the refactor's record: {e}")))?;
        entry.record = record;
        let events = self.refresh(agent, Some(&[repo]), now);
        Ok((ticket, events))
    }

    /// Merges the refactor into its target branch (spec §5.2–3). Returns the outcome
    /// and the events for the repo.
    pub fn merge(
        &mut self,
        agent: &mut Agent,
        path: &str,
        now: DateTime<Utc>,
    ) -> Result<(MergeOutcome, Vec<Event>), Fail> {
        let repo = self.key(path).ok_or_else(no_refactor)?;
        let procs = procs::scan();
        let entry = self.entries.get_mut(&repo).expect("known");
        if look(entry, agent, &procs).busy() {
            let outcome = MergeOutcome::Blocked {
                reason: MergeBlockReason::Live,
                files: Vec::new(),
            };
            return Ok((outcome, Vec::new()));
        }
        let record = entry.record.clone();
        let worktree = PathBuf::from(&record.worktree);
        self.check_safe(&repo, &worktree)?;
        let target_ref = format!("refs/heads/{}", record.target_branch);
        let branch_ref = format!("refs/heads/{}", record.branch);
        let Some(head) = commit_of(&repo, &branch_ref) else {
            return Err(fail(format!("The branch {} is gone.", record.branch)));
        };
        if commit_of(&repo, &target_ref).is_none() {
            return self.blocked(
                agent,
                &repo,
                MergeBlockReason::TargetMissing,
                vec![],
                &head,
                now,
            );
        }
        let dirty = changed_in_worktree(&worktree)?;
        if !dirty.is_empty() {
            return Err(fail(format!(
                "The refactor's worktree has uncommitted changes ({}). Commit or undo them there first.",
                dirty.join(", ")
            )));
        }
        let range = format!("{target_ref}..{branch_ref}");
        if count(&repo, &range) == 0 {
            return Err(fail("The refactor has no commits to merge."));
        }
        // Where the target is checked out in the repo itself, its uncommitted files must
        // not be ones the refactor changed.
        let checked_out = checked_out_here(&repo, &target_ref)?;
        if checked_out {
            let status = git::read_status(&repo)
                .map_err(|e| fail(format!("cannot read the repo's status: {e}")))?;
            let changed = changed_files(&repo, &target_ref, &branch_ref)?;
            let clash = overlap(&status.paths, &changed);
            if !clash.is_empty() {
                return self.blocked(
                    agent,
                    &repo,
                    MergeBlockReason::DirtyOverlap,
                    clash,
                    &head,
                    now,
                );
            }
        }
        // The branch onto the target, in the worktree; undone on conflict.
        if !is_ancestor(&repo, &target_ref, &branch_ref) {
            let rebase = run_git(&worktree, &["rebase", "-q", &target_ref])
                .map_err(|e| fail(format!("cannot run git: {e}")))?;
            if !rebase.ok {
                let conflicted = run_git(&worktree, &["diff", "--name-only", "--diff-filter=U"])
                    .map(|o| lines(&o.stdout))
                    .unwrap_or_default();
                let _ = run_git(&worktree, &["rebase", "--abort"]);
                if conflicted.is_empty() {
                    return Err(fail(format!("git rebase failed: {}", rebase.stderr.trim())));
                }
                return self.blocked(
                    agent,
                    &repo,
                    MergeBlockReason::Conflict,
                    conflicted,
                    &head,
                    now,
                );
            }
        }
        let commits = count(&repo, &range);
        let merged = if checked_out {
            run_git(&repo, &["merge", "--ff-only", "-q", &branch_ref])
        } else {
            let refspec = format!("{branch_ref}:{target_ref}");
            run_git(&repo, &["fetch", "-q", ".", &refspec])
        }
        .map_err(|e| fail(format!("cannot run git: {e}")))?;
        if !merged.ok {
            return Err(fail(format!(
                "Moving {} failed: {}",
                record.target_branch,
                merged.stderr.trim()
            )));
        }
        // Merged: the worktree and branch go, unless git refuses.
        let mut kept = Vec::new();
        let removed = run_git(&repo, &["worktree", "remove", &record.worktree]);
        match removed {
            Ok(out) if out.ok => {
                let branch_head = commit_of(&repo, &branch_ref);
                let deleted = run_git(&repo, &["branch", "-q", "-d", &record.branch]);
                if !deleted.as_ref().is_ok_and(|o| o.ok) {
                    // `-d` wants the branch merged into HEAD, which is another branch
                    // when the target is not checked out. It is merged into the target,
                    // so delete it only if it still points where it was merged.
                    let merged_in = is_ancestor(&repo, &branch_ref, &target_ref);
                    let gone = merged_in
                        && branch_head.as_deref().is_some_and(|sha| {
                            run_git(&repo, &["update-ref", "-d", &branch_ref, sha])
                                .is_ok_and(|o| o.ok)
                        });
                    if !gone {
                        let why = deleted.map(|o| o.stderr).unwrap_or_else(|e| e.to_string());
                        kept.push(format!("Kept the branch {}: {}", record.branch, why.trim()));
                    }
                }
            }
            Ok(out) => kept.push(format!(
                "Kept the worktree at {} and its branch {}: {}",
                record.worktree,
                record.branch,
                out.stderr.trim()
            )),
            Err(e) => kept.push(format!("Kept the worktree at {}: {e}", record.worktree)),
        }
        self.forget(&repo);
        let mut events = agent.set_refactor(&repo, None);
        events.extend(agent.refresh(&repo, Refresh::ALL));
        Ok((MergeOutcome::Merged { commits, kept }, events))
    }

    /// Deletes the refactor's worktree and branch (spec §5.4).
    pub fn discard(&mut self, agent: &mut Agent, path: &str) -> Result<Vec<Event>, Fail> {
        let repo = self.key(path).ok_or_else(no_refactor)?;
        let procs = procs::scan();
        let entry = self.entries.get_mut(&repo).expect("known");
        if look(entry, agent, &procs).live {
            return Err(fail("The refactor's agent is still running."));
        }
        let record = entry.record.clone();
        if !record.branch.starts_with(rules::BRANCH_PREFIX) {
            return Err(fail(format!(
                "{} is not a refactor branch, so it is not deleted.",
                record.branch
            )));
        }
        let worktree = PathBuf::from(&record.worktree);
        let listed = worktrees(&repo)
            .map_err(|e| fail(format!("cannot list the repo's worktrees: {e}")))?
            .iter()
            .any(|w| same_dir(&w.path, &worktree));
        if listed || fs::symlink_metadata(&worktree).is_ok() {
            self.check_safe(&repo, &worktree)?;
            let out = run_git(&repo, &["worktree", "remove", "--force", &record.worktree])
                .map_err(|e| fail(format!("cannot run git: {e}")))?;
            if !out.ok {
                return Err(fail(format!(
                    "git worktree remove failed: {}",
                    out.stderr.trim()
                )));
            }
        }
        if branch_exists(&repo, &record.branch) {
            let out = run_git(&repo, &["branch", "-q", "-D", &record.branch])
                .map_err(|e| fail(format!("cannot run git: {e}")))?;
            if !out.ok {
                return Err(fail(format!("git branch -D failed: {}", out.stderr.trim())));
            }
        }
        self.forget(&repo);
        let mut events = agent.set_refactor(&repo, None);
        events.extend(agent.refresh(&repo, Refresh::ALL));
        Ok(events)
    }

    /// Brings the records in line with git, once the repos are loaded: a record whose
    /// worktree is gone (not listed, or its folder deleted) is dropped, its branch kept;
    /// a refactor worktree of a known repo without a record is adopted. Returns the
    /// events for the repos' snapshots.
    pub fn reconcile(&mut self, agent: &mut Agent, now: DateTime<Utc>) -> Vec<Event> {
        let repos: Vec<PathBuf> = self.entries.keys().cloned().collect();
        for repo in repos {
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
            if self.entries.contains_key(&repo) {
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
            focus: None,
            created_at: now.trunc_subsecs(0),
            launched_at: None,
            blocked: None,
            blocked_head: None,
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
            },
        );
    }

    /// Works out again the refactors of `repos` (all of them for `None`) whose repo is
    /// known, and returns the events for the snapshots that changed.
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
        if paths.is_empty() {
            return Vec::new();
        }
        let procs = procs::scan();
        let mut events = Vec::new();
        for repo in paths {
            if agent.repo(&repo).is_none() {
                continue;
            }
            let entry = self.entries.get_mut(&repo).expect("known");
            let info = work_out(entry, agent, &procs, self.distro.as_deref(), now);
            if let Some(cleared) = info.1 {
                let _ = write_record(&self.records_dir, &entry.id, &cleared);
                entry.record = cleared;
            }
            entry.info = Some(info.0.clone());
            events.extend(agent.set_refactor(&repo, Some(info.0)));
        }
        events
    }

    /// Stores a blocked merge in the record and answers with it.
    fn blocked(
        &mut self,
        agent: &mut Agent,
        repo: &Path,
        reason: MergeBlockReason,
        files: Vec<String>,
        head: &str,
        now: DateTime<Utc>,
    ) -> Result<(MergeOutcome, Vec<Event>), Fail> {
        let entry = self.entries.get_mut(repo).expect("known");
        let mut record = entry.record.clone();
        record.blocked = Some(MergeBlock {
            reason,
            files: files.clone(),
        });
        record.blocked_head = commit_of(repo, &format!("refs/heads/{}", record.branch))
            .or_else(|| Some(head.to_owned()));
        write_record(&self.records_dir, &entry.id, &record)
            .map_err(|e| fail(format!("cannot save the refactor's record: {e}")))?;
        entry.record = record;
        let events = self.refresh(agent, Some(&[repo.to_owned()]), now);
        Ok((MergeOutcome::Blocked { reason, files }, events))
    }

    /// Refuses unless `worktree` really lies under the worktrees directory and the
    /// repo's `git worktree list` names it as a linked worktree.
    fn check_safe(&self, repo: &Path, worktree: &Path) -> Result<(), Fail> {
        let refuse = || {
            fail(format!(
                "{} is not a worktree Refract made for this repo, so it is left alone.",
                worktree.display()
            ))
        };
        let root = fs::canonicalize(&self.worktrees_dir).map_err(|_| refuse())?;
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

    /// Drops the refactor of `repo` and its record file.
    fn forget(&mut self, repo: &Path) {
        if let Some(entry) = self.entries.remove(repo) {
            let _ = fs::remove_file(self.records_dir.join(format!("{}.json", entry.id)));
        }
    }
}

fn no_refactor() -> Fail {
    fail("This repo has no refactor.")
}

/// What the refactor's agent is doing.
struct Look {
    live: bool,
    /// Live, and waiting for a prompt: its hook says its turn finished or, without a
    /// hook status, its transcript ends with a finished turn.
    idle: bool,
    /// The agent's transcript, if it wrote one.
    summary: Option<SessionSummary>,
    state: Option<SessionState>,
}

impl Look {
    /// Live and not idle: a merge would race the agent.
    fn busy(&self) -> bool {
        self.live && !self.idle
    }
}

fn look(entry: &Entry, agent: &mut Agent, procs: &[ClaudeProcess]) -> Look {
    let id = entry.record.session_id.as_str();
    if id.is_empty() {
        return Look {
            live: false,
            idle: false,
            summary: None,
            state: None,
        };
    }
    let worktree = entry.record.worktree.as_str();
    let sessions = agent.reader_mut().read_repo(worktree);
    let pairs: Vec<(&str, Option<DateTime<Utc>>)> = sessions
        .iter()
        .filter_map(|s| Some((s.session_id.as_deref()?, s.modified.or(s.last_timestamp))))
        .collect();
    // A fresh `--session-id` process has no transcript yet, but names the session.
    let live = procs.iter().any(|p| p.session_id.as_deref() == Some(id))
        || procs::live_sessions(procs, worktree, &pairs).contains_key(id);
    let summary = sessions
        .iter()
        .find(|s| s.session_id.as_deref() == Some(id))
        .cloned();
    let hook = agent.hooks().get(id).copied();
    let state = classify_report(
        summary.as_ref().unwrap_or(&SessionSummary::default()),
        live,
        hook,
    );
    let idle = match state {
        SessionState::Live(LiveState::Idle) => true,
        SessionState::Live(LiveState::Unknown) => summary
            .as_ref()
            .is_some_and(|s| matches!(classify_report(s, false, None), SessionState::Finished(_))),
        _ => false,
    };
    Look {
        live,
        idle,
        summary,
        state: Some(state),
    }
}

/// The refactor's info, and its record with `blocked` cleared when the branch moved on
/// since the merge stopped.
fn work_out(
    entry: &mut Entry,
    agent: &mut Agent,
    procs: &[ClaudeProcess],
    distro: Option<&str>,
    now: DateTime<Utc>,
) -> (RefactorInfo, Option<Record>) {
    let look = look(entry, agent, procs);
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
    let waiting = look.state == Some(SessionState::Live(LiveState::WaitingOnUser));
    let interrupted = !look.live && look.state.is_some_and(SessionState::is_unfinished);
    let launched = record.launched_at.unwrap_or(record.created_at);
    let state = derive_state(&Facts {
        live: look.live,
        idle: look.idle,
        waiting,
        blocked: blocked.is_some(),
        commits: commits.len(),
        transcript: look.summary.is_some(),
        fresh: now - launched < START_GRACE,
        nothing_reported: result.as_ref().is_some_and(|r| r.refactors.is_empty()),
    });
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
        interrupted,
        commits,
        files_changed,
        insertions,
        deletions,
        checks: result.as_ref().map(|r| r.checks.clone()),
        summary: result.and_then(|r| r.summary),
        blocked,
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
struct Out {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Runs git in `dir`, looking no higher than it for a repo, never prompting and never
/// opening an editor.
fn run_git(dir: &Path, args: &[&str]) -> io::Result<Out> {
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

/// The branch checked out in `repo`; `None` on a detached HEAD.
fn current_branch(repo: &Path) -> Option<String> {
    read_git(repo, &["symbolic-ref", "-q", "--short", "HEAD"])
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
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

/// Tracked files with changes in the worktree.
fn changed_in_worktree(worktree: &Path) -> Result<Vec<String>, Fail> {
    read_git(
        worktree,
        &["status", "--porcelain", "-z", "--untracked-files=no"],
    )
    .map(|out| {
        out.split('\0')
            .filter_map(|r| r.get(3..))
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect()
    })
    .ok_or_else(|| fail("cannot read the worktree's status"))
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
struct Worktree {
    path: PathBuf,
    /// The branch checked out, without `refs/heads/`.
    branch: Option<String>,
}

/// The repo's worktrees, the main one first.
fn worktrees(repo: &Path) -> io::Result<Vec<Worktree>> {
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
            });
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/") {
            if let Some(last) = list.last_mut() {
                last.branch = Some(branch.to_owned());
            }
        }
    }
    Ok(list)
}

/// True when `target` (a full ref) is checked out in the worktree at `repo` itself.
fn checked_out_here(repo: &Path, target: &str) -> Result<bool, Fail> {
    let list = worktrees(repo).map_err(|e| fail(format!("cannot list worktrees: {e}")))?;
    let branch = target.strip_prefix("refs/heads/").unwrap_or(target);
    Ok(list
        .iter()
        .any(|w| same_dir(&w.path, repo) && w.branch.as_deref() == Some(branch)))
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
fn same_dir(a: &Path, b: &Path) -> bool {
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
