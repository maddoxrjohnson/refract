//! Alerts for refactors (refactor spec §2, §4): one when the plan is first ready for
//! approval and one when the refactor finishes. One state machine decides each alert
//! from the helper's `repoChanged` events; it then goes to the interface as a
//! [`NOTICE_EVENT`] while the window shows (unless the repo is the zoomed one, whose
//! panel shows it), or as a Windows toast while it is hidden (WebView2 is suspended then).

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use refract_core::protocol::{Event, RefactorInfo, RefactorState, RepoSnapshot};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

/// Emitted with a [`Notice`] for the interface's toast.
pub const NOTICE_EVENT: &str = "refactor://notice";

/// A refactor alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Alert {
    /// The refactor agent waits on the user for the first time: its plan.
    PlanReady,
    /// The refactor has commits to review and merge.
    Finished,
}

/// What the interface shows, sent as `{"path", "repoName", "alert"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    pub path: String,
    pub repo_name: String,
    pub alert: Alert,
}

/// Where an alert goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The interface's toast: the window shows and the repo is not zoomed.
    Notice,
    /// A Windows toast: the window is hidden.
    Toast,
    /// Nothing: the zoomed repo's panel shows it.
    Panel,
}

/// What was told about one refactor.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Told {
    state: RefactorState,
    /// The plan alert was given, or the refactor was first seen past planning.
    plan: bool,
    /// The finished alert was given, or the refactor was first seen done.
    finished: bool,
}

/// One refactor: its worktree and when it was made, so a later refactor that reuses the
/// worktree's name is a new one.
fn key(info: &RefactorInfo) -> String {
    format!("{}\n{}", info.worktree, info.created_at.to_rfc3339())
}

/// The alert state machine. Refactors are kept by worktree, never by repo, so a snapshot
/// that briefly has no refactor (the helper's first load sends the repos before it has
/// reconciled their refactors) forgets nothing.
#[derive(Debug, Default)]
pub struct Tracker {
    refactors: HashMap<String, Told>,
    /// Each repo's current refactor, by repo path.
    current: HashMap<String, String>,
    /// The zoomed repo's path, as the interface last said.
    zoomed: Option<String>,
}

impl Tracker {
    /// Records `repo`'s refactor and returns the alert its change calls for. A refactor
    /// seen for the first time never alerts: it may have been there since before the app
    /// started. A new one first shows as starting, so nothing is lost.
    pub fn observe(&mut self, repo: &RepoSnapshot) -> Option<Alert> {
        let Some(info) = &repo.refactor else {
            self.current.remove(&repo.path);
            return None;
        };
        let key = key(info);
        self.current.insert(repo.path.clone(), key.clone());
        let state = info.state;
        let Some(told) = self.refactors.get_mut(&key) else {
            use RefactorState::*;
            self.refactors.insert(
                key,
                Told {
                    state,
                    plan: !matches!(state, Starting | Working),
                    finished: matches!(state, Ready | NothingToDo | MergeBlocked),
                },
            );
            return None;
        };
        let before = std::mem::replace(&mut told.state, state);
        match state {
            RefactorState::NeedsYou if before != state && !told.plan => {
                told.plan = true;
                Some(Alert::PlanReady)
            }
            RefactorState::Ready if before != state && !told.finished => {
                told.finished = true;
                Some(Alert::Finished)
            }
            // Its next finish is news again.
            RefactorState::Interrupted => {
                told.finished = false;
                None
            }
            _ => None,
        }
    }

    /// Records the repos of a `listRepos` answer whose refactors are not known yet. Known
    /// ones are left alone: events keep them current, and the answer may be older.
    pub fn seed(&mut self, repos: &[RepoSnapshot]) {
        for repo in repos {
            let known = repo
                .refactor
                .as_ref()
                .is_none_or(|info| self.refactors.contains_key(&key(info)));
            if !known {
                self.observe(repo);
            }
        }
    }

    /// The user resumed the repo's refactor from Refract: its next finish is news again.
    pub fn resumed(&mut self, path: &str) {
        if let Some(told) = self
            .current
            .get(path)
            .and_then(|k| self.refactors.get_mut(k))
        {
            told.finished = false;
        }
    }

    pub fn set_zoomed(&mut self, path: Option<String>) {
        self.zoomed = path;
    }

    /// Where an alert for the repo at `path` goes, with the window `visible` or not.
    pub fn route(&self, path: &str, visible: bool) -> Delivery {
        if !visible {
            Delivery::Toast
        } else if self.zoomed.as_deref() == Some(path) {
            Delivery::Panel
        } else {
            Delivery::Notice
        }
    }
}

/// The title and body of a Windows toast for the repo called `name`.
pub fn text(alert: Alert, name: &str) -> (String, &'static str) {
    match alert {
        Alert::PlanReady => (
            format!("The refactor plan for {name} is ready"),
            "Open Refract to go to it and approve it.",
        ),
        Alert::Finished => (
            format!("The refactor of {name} is ready"),
            "Open Refract to review and merge it.",
        ),
    }
}

static TRACKER: Mutex<Option<Tracker>> = Mutex::new(None);

fn with_tracker<T>(f: impl FnOnce(&mut Tracker) -> T) -> T {
    let mut tracker = TRACKER.lock().unwrap_or_else(PoisonError::into_inner);
    f(tracker.get_or_insert_with(Tracker::default))
}

/// Records the repos of a `listRepos` answer (see [`Tracker::seed`]).
pub fn on_list(repos: &[RepoSnapshot]) {
    with_tracker(|t| t.seed(repos));
}

/// Called when a resume of the repo's refactor starts (see [`Tracker::resumed`]).
pub fn on_resume(path: &str) {
    with_tracker(|t| t.resumed(path));
}

/// Called with every helper event before it is forwarded to the interface.
pub fn on_helper_event(app: &AppHandle, event: &Event) {
    let Event::RepoChanged(repo) = event else {
        return;
    };
    let visible = crate::window::window_visible(app);
    let Some((alert, delivery)) = with_tracker(|t| {
        t.observe(repo)
            .map(|alert| (alert, t.route(&repo.path, visible)))
    }) else {
        return;
    };
    match delivery {
        Delivery::Toast => {
            let (title, body) = text(alert, &repo.name);
            crate::migrate::notify(app, &title, body);
        }
        Delivery::Notice => {
            let notice = Notice {
                path: repo.path.clone(),
                repo_name: repo.name.clone(),
                alert,
            };
            if let Err(err) = app.emit(NOTICE_EVENT, notice) {
                eprintln!("refract: sending a refactor notice: {err}");
            }
        }
        Delivery::Panel => {}
    }
}

/// The interface's zoomed repo, `None` when none is; its alerts show in its panel.
#[tauri::command]
pub fn set_zoomed(path: Option<String>) {
    with_tracker(|t| t.set_zoomed(path));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use RefactorState::*;

    const PATH: &str = "/home/dev/projects/reef-maps";
    const A: &str = "/home/dev/projects/.refract-worktrees/reef-maps-20261031-1405";
    const B: &str = "/home/dev/projects/.refract-worktrees/reef-maps-20261031-1610";

    fn repo(refactor: Option<(&str, RefactorState)>) -> RepoSnapshot {
        let refactor = refactor.map(|(worktree, state)| {
            json!({
                "state": state,
                "worktree": worktree,
                "folderUri": format!("vscode-remote://wsl+Ubuntu{worktree}"),
                "branch": "refactor/20261031-1405",
                "targetBranch": "main",
                "baseCommit": "0123456789abcdef0123456789abcdef01234567",
                "sessionId": "5f0c2a1e-0000-4000-8000-000000000001",
                "createdAt": if worktree == A { "2026-10-31T14:05:00Z" } else { "2026-10-31T16:10:00Z" },
                "live": true,
                "commits": [],
                "filesChanged": 0,
                "insertions": 0,
                "deletions": 0,
                "checks": null,
                "summary": null,
                "blocked": null
            })
        });
        serde_json::from_value(json!({
            "path": PATH,
            "name": "reef-maps",
            "originUrl": null,
            "remote": null,
            "branch": "main",
            "ahead": null,
            "behind": null,
            "changedFiles": 0,
            "dirtyPaths": [],
            "lastCommit": null,
            "language": null,
            "trackedBytes": 0,
            "commits30d": 0,
            "mass": 0.5,
            "sessions": [],
            "preselect": {"kind": "noPicker"},
            "refactor": refactor
        }))
        .unwrap()
    }

    /// Runs `states` (None = no refactor) through one tracker, collecting the alerts.
    fn run(tracker: &mut Tracker, states: &[Option<(&str, RefactorState)>]) -> Vec<Option<Alert>> {
        states.iter().map(|s| tracker.observe(&repo(*s))).collect()
    }

    #[test]
    fn a_refactor_alerts_once_for_its_plan_and_once_when_it_is_ready() {
        let mut t = Tracker::default();
        assert_eq!(
            run(
                &mut t,
                &[
                    None,
                    Some((A, Starting)),
                    Some((A, Working)),
                    Some((A, NeedsYou)),
                    Some((A, NeedsYou)),
                    Some((A, Working)),
                    // A later question is not the plan.
                    Some((A, NeedsYou)),
                    Some((A, Ready)),
                    Some((A, Ready)),
                    // A merge that stopped and cleared is not news.
                    Some((A, MergeBlocked)),
                    Some((A, Ready)),
                    // Nor is an idle agent that starts and ends another turn.
                    Some((A, Working)),
                    Some((A, Ready)),
                    None,
                ]
            ),
            vec![
                None,
                None,
                None,
                Some(Alert::PlanReady),
                None,
                None,
                None,
                Some(Alert::Finished),
                None,
                None,
                None,
                None,
                None,
                None
            ]
        );
    }

    #[test]
    fn the_next_refactor_alerts_again() {
        let mut t = Tracker::default();
        assert_eq!(
            run(
                &mut t,
                &[
                    Some((A, Starting)),
                    Some((A, NeedsYou)),
                    Some((A, Ready)),
                    None,
                    Some((B, Starting)),
                    Some((B, NeedsYou)),
                    Some((B, Ready)),
                ]
            ),
            vec![
                None,
                Some(Alert::PlanReady),
                Some(Alert::Finished),
                None,
                None,
                Some(Alert::PlanReady),
                Some(Alert::Finished)
            ]
        );
    }

    #[test]
    fn a_refactor_seen_first_past_starting_never_alerts() {
        // (none) -> (A, needsYou): the app started while the plan waited.
        let mut t = Tracker::default();
        assert_eq!(run(&mut t, &[None, Some((A, NeedsYou))]), vec![None, None]);
        for state in [
            Working,
            NeedsYou,
            Interrupted,
            Ready,
            NothingToDo,
            MergeBlocked,
        ] {
            let mut t = Tracker::default();
            let alerts = run(
                &mut t,
                &[Some((A, state)), Some((A, NeedsYou)), Some((A, Ready))],
            );
            assert_eq!(alerts[0], None, "{state:?}");
            // Seen while working, the plan may still come; seen later, it has been.
            assert_eq!(
                alerts[1].is_some(),
                state == Working,
                "{state:?}: {alerts:?}"
            );
        }
    }

    #[test]
    fn the_helpers_two_phase_load_and_restarts_tell_nothing_again() {
        let mut t = Tracker::default();
        run(&mut t, &[Some((A, Starting)), Some((A, NeedsYou))]);
        // The helper restarts: repos first without refactors, then reconciled.
        assert_eq!(
            run(
                &mut t,
                &[None, Some((A, NeedsYou)), None, Some((A, NeedsYou))]
            ),
            vec![None, None, None, None]
        );
        assert_eq!(
            run(&mut t, &[Some((A, Ready))]),
            vec![Some(Alert::Finished)]
        );
        assert_eq!(run(&mut t, &[None, Some((A, Ready))]), vec![None, None]);
    }

    #[test]
    fn finished_is_news_again_after_an_interruption_or_a_resume() {
        let mut t = Tracker::default();
        run(&mut t, &[Some((A, Starting)), Some((A, Ready))]);
        assert_eq!(
            run(
                &mut t,
                &[Some((A, Interrupted)), Some((A, Working)), Some((A, Ready))]
            ),
            vec![None, None, Some(Alert::Finished)]
        );
        t.resumed(PATH);
        assert_eq!(
            run(&mut t, &[Some((A, Working)), Some((A, Ready))]),
            vec![None, Some(Alert::Finished)]
        );
    }

    #[test]
    fn a_list_only_adds_refactors_not_seen_yet() {
        let mut t = Tracker::default();
        run(&mut t, &[Some((A, Starting)), Some((A, Working))]);
        // An answer older than the events: it must not wind the state back.
        t.seed(&[repo(Some((A, Starting)))]);
        assert_eq!(
            run(&mut t, &[Some((A, NeedsYou))]),
            vec![Some(Alert::PlanReady)]
        );
        // A refactor first seen in a list is a first sighting.
        let mut t = Tracker::default();
        t.seed(&[repo(Some((B, NeedsYou)))]);
        assert_eq!(
            run(&mut t, &[Some((B, Working)), Some((B, NeedsYou))]),
            vec![None, None]
        );
    }

    #[test]
    fn alerts_go_to_the_panel_the_interface_or_windows() {
        let mut t = Tracker::default();
        assert_eq!(t.route(PATH, false), Delivery::Toast);
        assert_eq!(t.route(PATH, true), Delivery::Notice);
        t.set_zoomed(Some(PATH.into()));
        assert_eq!(t.route(PATH, true), Delivery::Panel);
        assert_eq!(t.route("/home/dev/projects/other", true), Delivery::Notice);
        assert_eq!(t.route(PATH, false), Delivery::Toast);
    }

    #[test]
    fn notices_and_toasts_name_the_repo() {
        assert_eq!(
            serde_json::to_value(Notice {
                path: PATH.into(),
                repo_name: "reef-maps".into(),
                alert: Alert::PlanReady
            })
            .unwrap(),
            json!({"path": PATH, "repoName": "reef-maps", "alert": "planReady"})
        );
        assert_eq!(
            text(Alert::PlanReady, "reef-maps").0,
            "The refactor plan for reef-maps is ready"
        );
        assert_eq!(
            text(Alert::Finished, "reef-maps").0,
            "The refactor of reef-maps is ready"
        );
    }
}
