//! Full repo snapshots over temporary repos and invented transcripts.

mod common;

use std::path::{Path, PathBuf};

use chrono::{TimeDelta, Utc};
use refract_agent::config::Config;
use refract_agent::state::{read_git_with, Agent, Refresh};
use refract_core::classify::{FinishKind, InterruptReason, SessionState};
use refract_core::preselect::{Preselect, ResumeReason};
use refract_core::protocol::{Event, LastCommit, RemoteId, RepoSnapshot};

use common::*;

const OPEN_ID: &str = "5e2a7c10-0000-4000-8000-000000000001";
const DONE_ID: &str = "5e2a7c10-0000-4000-8000-000000000002";
const OTHER_ID: &str = "5e2a7c10-0000-4000-8000-000000000003";

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    projects: PathBuf,
}

fn fixture() -> Fixture {
    hermetic_git();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_owned();
    let root = base.join("projects");
    let projects = base.join("home/.claude/projects");

    let (bare, seed) = remote(&base, "tidepool-api");

    let clean = root.join("tidepool-api");
    clone(&bare, &clean);
    git(
        &clean,
        &[
            "remote",
            "set-url",
            "origin",
            "git@github.com:dev/tidepool-api.git",
        ],
    );

    let ahead = root.join("aurora-notes");
    clone(&bare, &ahead);
    commit_file(&ahead, "src/notes.py", "print('a')\n", "Add notes");
    commit_file(&ahead, "src/more.py", "print('b')\n", "Add more notes");

    let behind = root.join("kelp-cli");
    clone(&bare, &behind);
    commit_file(&seed, "CHANGELOG.md", "- one\n", "Start changelog");
    git(&seed, &["push", "-q", "origin", "main"]);
    git(&behind, &["fetch", "-q"]);

    let local = root.join("work/coral-db");
    init_repo(&local);

    let detached = root.join("drift-log");
    clone(&bare, &detached);
    git(&detached, &["checkout", "-q", "--detach", "HEAD"]);

    let empty = root.join("shoal-empty");
    std::fs::create_dir_all(&empty).unwrap();
    git(&empty, &["init", "-q", "-b", "main"]);

    let dirty = root.join("reef-web");
    clone(&bare, &dirty);
    write(
        &dirty.join("src/main.rs"),
        "fn main() { println!(\"hi\"); }\n",
    );
    write(&dirty.join("notes.txt"), "todo\n");

    // Sessions for reef-web: an older finished one and a recent one that crashed mid-tool
    // after editing the now-dirty file. A third transcript shares the encoded directory
    // but belongs to `reef_web`, which must not count.
    let dir = project_dir(&projects, &dirty);
    let edited = dirty.join("src/main.rs");
    let now = Utc::now();
    write(
        &dir.join(format!("{DONE_ID}.jsonl")),
        &transcript(DONE_ID, &dirty, &edited, now - TimeDelta::hours(3), true),
    );
    write(
        &dir.join(format!("{OPEN_ID}.jsonl")),
        &transcript(OPEN_ID, &dirty, &edited, now - TimeDelta::hours(1), false),
    );
    let lookalike = root.join("reef_web");
    write(
        &dir.join(format!("{OTHER_ID}.jsonl")),
        &transcript(OTHER_ID, &lookalike, &lookalike.join("x.rs"), now, false),
    );

    Fixture {
        _tmp: tmp,
        root,
        projects,
    }
}

fn load(f: &Fixture) -> Vec<RepoSnapshot> {
    let mut agent = Agent::new(Config {
        roots: vec![f.root.clone()],
        extra: vec![],
        hidden: vec![],
        depth: 3,
        claude_projects: f.projects.clone(),
        limits_file: f.projects.join("usage-state.json"),
    });
    agent.load_all();
    agent.snapshots()
}

fn find<'a>(repos: &'a [RepoSnapshot], root: &Path, rel: &str) -> &'a RepoSnapshot {
    let path = root.join(rel);
    repos
        .iter()
        .find(|r| Path::new(&r.path) == path)
        .unwrap_or_else(|| panic!("{rel} not found"))
}

#[test]
fn snapshots_of_every_repo_kind() {
    let f = fixture();
    let repos = load(&f);
    let names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "aurora-notes",
            "drift-log",
            "kelp-cli",
            "reef-web",
            "shoal-empty",
            "tidepool-api",
            "coral-db"
        ]
    );

    let clean = find(&repos, &f.root, "tidepool-api");
    assert_eq!(
        clean.origin_url.as_deref(),
        Some("git@github.com:dev/tidepool-api.git")
    );
    assert_eq!(
        clean.remote,
        Some(RemoteId {
            host: "github.com".into(),
            owner: "dev".into(),
            name: "tidepool-api".into()
        })
    );
    assert_eq!(clean.branch.as_deref(), Some("main"));
    assert_eq!((clean.ahead, clean.behind), (Some(0), Some(0)));
    assert_eq!(clean.changed_files, 0);
    assert!(clean.dirty_paths.is_empty());
    assert_eq!(clean.language.as_deref(), Some("Rust"));
    assert_eq!(
        clean.tracked_bytes,
        ("fn main() {}\n".len() + "# demo\n".len()) as u64
    );
    assert_eq!(clean.commits_30d, 1);
    let commit = clean.last_commit.as_ref().unwrap();
    assert_eq!(commit.summary, "Initial commit");
    assert!((Utc::now().timestamp() - commit.time).abs() < 600);
    assert!(clean.sessions.is_empty());
    assert_eq!(clean.preselect, Preselect::NoPicker);

    let ahead = find(&repos, &f.root, "aurora-notes");
    assert_eq!((ahead.ahead, ahead.behind), (Some(2), Some(0)));
    assert_eq!(ahead.commits_30d, 3);
    assert_eq!(ahead.language.as_deref(), Some("Python"));
    assert_eq!(
        ahead.last_commit.as_ref().unwrap().summary,
        "Add more notes"
    );

    let behind = find(&repos, &f.root, "kelp-cli");
    assert_eq!((behind.ahead, behind.behind), (Some(0), Some(1)));
    assert!(
        behind.remote.is_none(),
        "a local path is not a hosted remote"
    );
    assert!(behind.origin_url.is_some());

    let local = find(&repos, &f.root, "work/coral-db");
    assert_eq!(local.origin_url, None);
    assert_eq!((local.ahead, local.behind), (None, None));
    assert_eq!(local.branch.as_deref(), Some("main"));

    let detached = find(&repos, &f.root, "drift-log");
    assert_eq!(detached.branch, None);
    assert_eq!((detached.ahead, detached.behind), (None, None));
    assert!(detached.last_commit.is_some());

    let empty = find(&repos, &f.root, "shoal-empty");
    assert_eq!(empty.branch.as_deref(), Some("main"));
    assert_eq!(empty.last_commit, None::<LastCommit>);
    assert_eq!((empty.tracked_bytes, empty.commits_30d), (0, 0));
    assert_eq!(empty.language, None);
    assert_eq!(empty.mass, 0.0);

    for repo in &repos {
        assert!(
            (0.0..=1.0).contains(&repo.mass),
            "{}: {}",
            repo.name,
            repo.mass
        );
    }
    assert!(repos.iter().any(|r| r.mass == 1.0));
    assert!(ahead.mass > clean.mass);
}

#[test]
fn dirty_repo_with_sessions_preselects_resume() {
    let f = fixture();
    let repos = load(&f);
    let dirty = find(&repos, &f.root, "reef-web");
    let root = f.root.join("reef-web");
    assert_eq!(dirty.changed_files, 2);
    let mut paths = dirty.dirty_paths.clone();
    paths.sort();
    assert_eq!(
        paths,
        [
            root.join("notes.txt").to_str().unwrap(),
            root.join("src/main.rs").to_str().unwrap()
        ]
    );
    assert_eq!((dirty.ahead, dirty.behind), (Some(0), Some(0)));

    let ids: Vec<&str> = dirty.sessions.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, [OPEN_ID, DONE_ID]);
    let open = &dirty.sessions[0];
    assert_eq!(
        open.state,
        SessionState::Interrupted(InterruptReason::ToolCallWithoutResult)
    );
    assert_eq!(open.resume_reason, Some(ResumeReason::Interrupted));
    assert!(open.byte_size > 0);
    assert!(open.last_timestamp.is_some());
    let done = &dirty.sessions[1];
    assert_eq!(done.state, SessionState::Finished(FinishKind::Completed));
    assert_eq!(
        done.last_assistant_text.as_deref(),
        Some("The entry point is tidy.")
    );
    assert_eq!(done.resume_reason, None);

    assert_eq!(dirty.preselect, Preselect::Resume(OPEN_ID.into()));
}

/// One cloned repo under `projects/` with an empty Claude projects directory.
fn single_repo(name: &str) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    hermetic_git();
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_owned();
    let root = base.join("projects");
    let projects = base.join("home/.claude/projects");
    let (bare, _seed) = remote(&base, name);
    let repo = root.join(name);
    clone(&bare, &repo);
    (tmp, root, projects, repo)
}

fn agent_for(root: &Path, projects: &Path) -> Agent {
    let mut agent = Agent::new(Config {
        roots: vec![root.to_owned()],
        extra: vec![],
        hidden: vec![],
        depth: 3,
        claude_projects: projects.to_owned(),
        limits_file: projects.join("usage-state.json"),
    });
    agent.load_all();
    agent
}

/// Writes a finished transcript for `repo` ending `minutes` after now (negative for
/// before), with `reply` as its last text reply.
fn finished_session(projects: &Path, repo: &Path, minutes: i64, reply: &str) {
    let text = transcript(
        DONE_ID,
        repo,
        &repo.join("src/main.rs"),
        Utc::now() + TimeDelta::minutes(minutes),
        true,
    )
    .replace("The entry point is tidy.", reply);
    write(
        &project_dir(projects, repo).join(format!("{DONE_ID}.jsonl")),
        &text,
    );
}

/// The one repo's resume reasons and preselect.
fn decision(root: &Path, projects: &Path) -> (Vec<Option<ResumeReason>>, Preselect) {
    let agent = agent_for(root, projects);
    let snapshot = &agent.snapshots()[0];
    let reasons = snapshot.sessions.iter().map(|s| s.resume_reason).collect();
    (reasons, snapshot.preselect.clone())
}

#[test]
fn a_finished_session_asking_a_question_preselects_resume() {
    let (_tmp, root, projects, repo) = single_repo("tidepool-api");
    finished_session(&projects, &repo, -60, "Shall I add a test for it too?");
    assert_eq!(
        decision(&root, &projects),
        (
            vec![Some(ResumeReason::AskedQuestion)],
            Preselect::Resume(DONE_ID.into())
        )
    );
}

#[test]
fn uncommitted_changes_after_the_last_commit_preselect_resume() {
    let (_tmp, root, projects, repo) = single_repo("tidepool-api");
    write(
        &repo.join("src/main.rs"),
        "fn main() { todo!() }
",
    );
    finished_session(&projects, &repo, 1, "Done.");
    assert_eq!(
        decision(&root, &projects),
        (
            vec![Some(ResumeReason::UncommittedChanges)],
            Preselect::Resume(DONE_ID.into())
        )
    );
    // A session from before the last commit leaves New agent preselected.
    finished_session(&projects, &repo, -60, "Done.");
    assert_eq!(
        decision(&root, &projects),
        (vec![None], Preselect::NewAgent)
    );
}

#[test]
fn unpushed_commits_made_in_the_session_preselect_resume() {
    let (_tmp, root, projects, repo) = single_repo("tidepool-api");
    commit_file(&repo, "src/tides.rs", "pub fn tides() {}\n", "Add tides");
    finished_session(&projects, &repo, 1, "Committed.");
    assert_eq!(
        decision(&root, &projects),
        (
            vec![Some(ResumeReason::UnpushedCommits)],
            Preselect::Resume(DONE_ID.into())
        )
    );
    // Commits made after the session ended are not its business.
    finished_session(&projects, &repo, -60, "Committed.");
    assert_eq!(
        decision(&root, &projects),
        (vec![None], Preselect::NoPicker)
    );
}

#[test]
fn a_branch_without_upstream_counts_only_commits_origin_lacks() {
    let (_tmp, root, projects, repo) = single_repo("tidepool-api");
    git(&repo, &["checkout", "-q", "-b", "tide-tables"]);
    finished_session(&projects, &repo, 1, "Branch ready.");
    // A new branch with nothing on it has nothing to push.
    assert_eq!(
        decision(&root, &projects),
        (vec![None], Preselect::NoPicker)
    );

    commit_file(&repo, "src/tides.rs", "pub fn tides() {}\n", "Add tides");
    finished_session(&projects, &repo, 1, "Committed.");
    assert_eq!(
        decision(&root, &projects),
        (
            vec![Some(ResumeReason::UnpushedCommits)],
            Preselect::Resume(DONE_ID.into())
        )
    );

    // Pushed without setting an upstream: origin has the commit.
    git(&repo, &["push", "-q", "origin", "HEAD"]);
    assert_eq!(
        decision(&root, &projects),
        (vec![None], Preselect::NoPicker)
    );
}

#[test]
fn a_gone_upstream_has_nothing_to_push() {
    let (_tmp, root, projects, repo) = single_repo("tidepool-api");
    git(&repo, &["checkout", "-q", "-b", "tide-tables"]);
    commit_file(&repo, "src/tides.rs", "pub fn tides() {}\n", "Add tides");
    git(&repo, &["push", "-q", "-u", "origin", "tide-tables"]);
    git(&repo, &["push", "-q", "origin", "--delete", "tide-tables"]);
    git(&repo, &["fetch", "-q", "--prune"]);
    finished_session(&projects, &repo, 1, "Committed.");
    assert_eq!(
        decision(&root, &projects),
        (vec![None], Preselect::NoPicker)
    );
    let agent = agent_for(&root, &projects);
    assert_eq!(agent.snapshots()[0].ahead, None);
}

const GIT_ONLY: Refresh = Refresh {
    status: true,
    head: true,
    sessions: false,
};

#[test]
fn pushing_clears_the_reason_in_the_change_event() {
    let (_tmp, root, projects, repo) = single_repo("tidepool-api");
    commit_file(&repo, "src/tides.rs", "pub fn tides() {}\n", "Add tides");
    finished_session(&projects, &repo, 1, "Committed.");
    let mut agent = agent_for(&root, &projects);
    assert_eq!(
        agent.snapshots()[0].sessions[0].resume_reason,
        Some(ResumeReason::UnpushedCommits)
    );

    git(&repo, &["push", "-q", "origin", "main"]);
    let events = agent.refresh(&repo, GIT_ONLY);
    let [Event::RepoChanged(snapshot)] = &events[..] else {
        panic!("expected one repoChanged, got {events:?}");
    };
    assert_eq!(snapshot.ahead, Some(0));
    assert_eq!(snapshot.sessions[0].resume_reason, None);
    assert_eq!(snapshot.preselect, Preselect::NoPicker);
}

#[test]
fn an_upstream_going_away_sends_the_new_preselect_with_the_session() {
    let (_tmp, root, projects, repo) = single_repo("tidepool-api");
    git(&repo, &["checkout", "-q", "-b", "tide-tables"]);
    commit_file(&repo, "src/tides.rs", "pub fn tides() {}\n", "Add tides");
    finished_session(&projects, &repo, 1, "Committed.");
    let mut agent = agent_for(&root, &projects);
    assert_eq!(
        agent.snapshots()[0].preselect,
        Preselect::Resume(DONE_ID.into())
    );

    // From no upstream to a gone one: ahead and behind stay unknown, so only the
    // session's reason changes.
    git(&repo, &["push", "-q", "-u", "origin", "tide-tables"]);
    git(&repo, &["push", "-q", "origin", "--delete", "tide-tables"]);
    git(&repo, &["fetch", "-q", "--prune"]);
    let events = agent.refresh(&repo, GIT_ONLY);
    let [Event::SessionChanged(change)] = &events[..] else {
        panic!("expected one sessionChanged, got {events:?}");
    };
    assert_eq!(change.session.id, DONE_ID);
    assert_eq!(change.session.resume_reason, None);
    assert_eq!(change.preselect, Preselect::NoPicker);
}

#[test]
fn a_repo_without_origin_or_on_a_detached_head_has_nothing_to_push() {
    let (_tmp, root, projects, repo) = single_repo("tidepool-api");
    git(&repo, &["remote", "remove", "origin"]);
    finished_session(&projects, &repo, 1, "Done.");
    assert_eq!(
        decision(&root, &projects),
        (vec![None], Preselect::NoPicker)
    );

    let (_tmp, root, projects, repo) = single_repo("aurora-notes");
    commit_file(&repo, "notes.md", "- one\n", "Add notes");
    git(&repo, &["checkout", "-q", "--detach", "HEAD"]);
    finished_session(&projects, &repo, 1, "Done.");
    assert_eq!(
        decision(&root, &projects),
        (vec![None], Preselect::NoPicker)
    );
}

#[test]
fn credentials_in_the_origin_url_stay_off_the_wire() {
    let (_tmp, root, projects, repo) = single_repo("tidepool-api");
    git(
        &repo,
        &[
            "remote",
            "set-url",
            "origin",
            "https://dev:ghp_inventedtoken123@github.com/dev/tidepool-api.git",
        ],
    );
    let agent = agent_for(&root, &projects);
    let snapshot = &agent.snapshots()[0];
    assert_eq!(
        snapshot.origin_url.as_deref(),
        Some("https://github.com/dev/tidepool-api.git")
    );
    let wire = serde_json::to_string(snapshot).unwrap();
    assert!(!wire.contains("ghp_inventedtoken123"));
}

#[test]
fn preselect_is_recomputed_as_time_passes() {
    let (_tmp, root, projects, repo) = single_repo("tidepool-api");
    let edited = repo.join("src/main.rs");
    write(&edited, "fn main() { println!(\"tide\"); }\n");
    write(
        &project_dir(&projects, &repo).join(format!("{OPEN_ID}.jsonl")),
        &transcript(
            OPEN_ID,
            &repo,
            &edited,
            Utc::now() - TimeDelta::hours(1),
            false,
        ),
    );
    let mut agent = agent_for(&root, &projects);
    assert_eq!(
        agent.snapshots()[0].preselect,
        Preselect::Resume(OPEN_ID.into())
    );
    assert!(agent.refresh_preselects(Utc::now()).is_empty());
    // Three days on, the same session is too old to preselect.
    assert_eq!(
        agent.refresh_preselects(Utc::now() + TimeDelta::days(3)),
        std::slice::from_ref(&repo)
    );
    assert_eq!(agent.snapshots()[0].preselect, Preselect::NewAgent);
    let state = agent.repo(&repo).unwrap();
    assert_eq!(state.snapshot.preselect, Preselect::NewAgent);
}

#[test]
fn status_and_head_read_together_or_in_turn_agree() {
    hermetic_git();
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("kelp-cli");
    init_repo(&repo);
    write(&repo.join("README.md"), "# kelp\n");
    let together = read_git_with(&repo, true).unwrap();
    let in_turn = read_git_with(&repo, false).unwrap();
    assert_eq!(together, in_turn);
    assert_eq!(together.0.paths, ["README.md"]);
    assert!(together.1.last_commit.is_some());

    let not_a_repo = tmp.path().join("aurora-notes");
    std::fs::create_dir_all(not_a_repo.join(".git")).unwrap();
    assert!(read_git_with(&not_a_repo, true).is_err());
    assert!(read_git_with(&not_a_repo, false).is_err());
}
