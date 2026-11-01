//! Windows toasts for refactors (refactor spec §2, §4): one when the plan is first ready
//! for approval and one when the refactor finishes. WebView2 is suspended while the
//! window is hidden, so these come from here, watching the helper's `repoChanged`
//! events; while the window shows, the interface toasts instead.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use refract_core::protocol::{Event, RefactorInfo, RefactorState, RepoSnapshot};
use tauri::AppHandle;

/// What the app last saw of one repo's refactor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    /// The refactor's worktree, which tells one refactor from the next; `None` while the
    /// repo has none.
    worktree: Option<String>,
    state: Option<RefactorState>,
    /// The plan-ready toast was shown for this refactor, or it was first seen past it.
    plan_told: bool,
}

/// A toast to show for a repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alert {
    /// The refactor agent waits on the user for the first time: its plan.
    PlanReady,
    /// The refactor has commits to review and merge.
    Finished,
}

/// What `next` is after `prev` (`None` the first time the repo is seen), and the toast the
/// change calls for. A repo seen for the first time never toasts: the refactor may have
/// been there since before the app started.
pub fn step(prev: Option<&Seen>, next: Option<&RefactorInfo>) -> (Seen, Option<Alert>) {
    let worktree = next.map(|r| r.worktree.clone());
    let state = next.map(|r| r.state);
    let Some(prev) = prev else {
        // Planning happens while starting or working; any later state is past the plan.
        let plan_told =
            state.is_some_and(|s| !matches!(s, RefactorState::Starting | RefactorState::Working));
        return (
            Seen {
                worktree,
                state,
                plan_told,
            },
            None,
        );
    };
    let same = prev.worktree.is_some() && prev.worktree == worktree;
    let before = if same { prev.state } else { None };
    let mut plan_told = same && prev.plan_told;
    let alert = match state {
        Some(RefactorState::NeedsYou) if before != state && !plan_told => {
            plan_told = true;
            Some(Alert::PlanReady)
        }
        // From a state of the same refactor that was not done yet; a merge that stopped
        // and then cleared is not news.
        Some(RefactorState::Ready)
            if before.is_some_and(|b| {
                !matches!(b, RefactorState::Ready | RefactorState::MergeBlocked)
            }) =>
        {
            Some(Alert::Finished)
        }
        _ => None,
    };
    (
        Seen {
            worktree,
            state,
            plan_told,
        },
        alert,
    )
}

/// The title and body of a toast for the repo called `name`.
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

/// Every repo's last seen refactor, by path.
static SEEN: Mutex<Option<HashMap<String, Seen>>> = Mutex::new(None);

/// Records `repo`'s refactor and returns the toast its change calls for.
fn observe(repo: &RepoSnapshot) -> Option<Alert> {
    let mut seen = SEEN.lock().unwrap_or_else(PoisonError::into_inner);
    let seen = seen.get_or_insert_with(HashMap::new);
    let (next, alert) = step(seen.get(&repo.path), repo.refactor.as_ref());
    seen.insert(repo.path.clone(), next);
    alert
}

/// Records the repos of a `listRepos` answer, so a later change of theirs can toast.
pub fn on_list(repos: &[RepoSnapshot]) {
    for repo in repos {
        observe(repo);
    }
}

/// Called with every helper event before it is forwarded to the interface.
pub fn on_helper_event(app: &AppHandle, event: &Event) {
    match event {
        Event::RepoChanged(repo) => {
            if let Some(alert) = observe(repo) {
                if !crate::window::window_visible(app) {
                    let (title, body) = text(alert, &repo.name);
                    crate::migrate::notify(app, &title, body);
                }
            }
        }
        Event::RepoRemoved { path } => {
            let mut seen = SEEN.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(seen) = seen.as_mut() {
                seen.remove(path);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info(worktree: &str, state: RefactorState) -> RefactorInfo {
        let mut info: RefactorInfo = serde_json::from_value(json!({
            "state": "starting",
            "worktree": worktree,
            "folderUri": format!("vscode-remote://wsl+Ubuntu{worktree}"),
            "branch": "refactor/20261031-1405",
            "targetBranch": "main",
            "baseCommit": "0123456789abcdef0123456789abcdef01234567",
            "sessionId": "5f0c2a1e-0000-4000-8000-000000000001",
            "createdAt": "2026-10-31T14:05:00Z",
            "live": true,
            "commits": [],
            "filesChanged": 0,
            "insertions": 0,
            "deletions": 0,
            "checks": null,
            "summary": null,
            "blocked": null
        }))
        .unwrap();
        info.state = state;
        info
    }

    const A: &str = "/home/dev/projects/.refract-worktrees/reef-maps-20261031-1405";
    const B: &str = "/home/dev/projects/.refract-worktrees/reef-maps-20261031-1610";

    /// Runs `states` (None = no refactor) through `step` from a repo already seen
    /// without a refactor, collecting the toasts.
    fn run(states: &[Option<(&str, RefactorState)>]) -> Vec<Option<Alert>> {
        let (mut seen, _) = step(None, None);
        states
            .iter()
            .map(|s| {
                let next = s.map(|(w, st)| info(w, st));
                let (after, alert) = step(Some(&seen), next.as_ref());
                seen = after;
                alert
            })
            .collect()
    }

    use RefactorState::*;

    #[test]
    fn a_refactor_toasts_once_for_its_plan_and_once_when_it_is_ready() {
        assert_eq!(
            run(&[
                Some((A, Starting)),
                Some((A, Working)),
                Some((A, NeedsYou)),
                Some((A, NeedsYou)),
                Some((A, Working)),
                // A later question is not the plan.
                Some((A, NeedsYou)),
                Some((A, Working)),
                Some((A, Ready)),
                Some((A, Ready)),
                None,
            ]),
            vec![
                None,
                None,
                Some(Alert::PlanReady),
                None,
                None,
                None,
                None,
                Some(Alert::Finished),
                None,
                None
            ]
        );
    }

    #[test]
    fn the_next_refactor_toasts_again() {
        assert_eq!(
            run(&[
                Some((A, NeedsYou)),
                Some((A, Ready)),
                None,
                Some((B, Starting)),
                Some((B, NeedsYou)),
                Some((B, Ready)),
            ]),
            vec![
                Some(Alert::PlanReady),
                Some(Alert::Finished),
                None,
                None,
                Some(Alert::PlanReady),
                Some(Alert::Finished)
            ]
        );
        // Straight from one refactor to the next, with no event in between.
        assert_eq!(
            run(&[Some((A, NeedsYou)), Some((B, NeedsYou))]),
            vec![Some(Alert::PlanReady), Some(Alert::PlanReady)]
        );
    }

    #[test]
    fn a_merge_that_stopped_and_cleared_is_not_news() {
        assert_eq!(
            run(&[
                Some((A, Working)),
                Some((A, Ready)),
                Some((A, MergeBlocked)),
                Some((A, Ready)),
                Some((A, Interrupted)),
                Some((A, Ready)),
            ]),
            vec![
                None,
                Some(Alert::Finished),
                None,
                None,
                None,
                Some(Alert::Finished)
            ]
        );
    }

    #[test]
    fn the_first_sighting_never_toasts() {
        for state in [
            Starting,
            Working,
            NeedsYou,
            Interrupted,
            Ready,
            NothingToDo,
            MergeBlocked,
        ] {
            let (seen, alert) = step(None, Some(&info(A, state)));
            assert_eq!(alert, None, "{state:?}");
            // Seen past planning: a later question is not the plan.
            let (_, later) = step(Some(&seen), Some(&info(A, NeedsYou)));
            let planning = matches!(state, Starting | Working);
            assert_eq!(later.is_some(), planning, "{state:?}");
        }
        // A refactor first seen as ready does not toast when it is seen again.
        let (seen, _) = step(None, Some(&info(A, Ready)));
        assert_eq!(step(Some(&seen), Some(&info(A, Ready))).1, None);
    }

    #[test]
    fn a_refactor_that_appears_ready_does_not_toast() {
        // Adopted from a worktree the helper found, already done.
        assert_eq!(run(&[Some((A, Ready))]), vec![None]);
    }

    #[test]
    fn toasts_name_the_repo() {
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
