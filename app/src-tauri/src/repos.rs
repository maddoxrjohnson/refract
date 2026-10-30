//! Bringing repos into view: cloning a remote repo into `~/projects` in WSL, and adding
//! a folder outside the helper's roots. The helper does the work (`cloneRepo`,
//! `addRepo`) and reports the repo with `repoChanged`; these commands only ask it.

use std::time::Duration;

use refract_client::RequestError;
use refract_core::protocol::{check_clone, code, Method, Reply};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::agent::Agent;

/// How long `clone_repo` waits. A little longer than the helper gives `git clone`
/// (ten minutes), so the helper's own timeout error arrives first.
const CLONE_TIMEOUT: Duration = Duration::from_secs(11 * 60);
/// How long `add_repo` waits: git runs once and the roots are searched again, which is
/// slow under `/mnt/`.
const ADD_TIMEOUT: Duration = Duration::from_secs(30);

/// The parts of the interface's `RemoteRepo` a clone needs; the rest is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteRepoArg {
    /// HTTPS clone URL.
    pub clone_url: String,
    /// The folder name under `~/projects`.
    pub name: String,
}

/// Why a clone or add failed, sent as `{"kind": "clone", "message": "..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoError {
    pub kind: RepoErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RepoErrorKind {
    /// The URL is not `https://`, or the name is not a plain folder name.
    Invalid,
    /// The folder exists and holds something else, or `git clone` failed.
    Clone,
    /// The folder to add is not in a git working tree.
    NotARepo,
    /// The helper failed, is not running or did not answer.
    Agent,
    /// The command's task itself failed.
    Internal,
}

impl RepoError {
    fn new(kind: RepoErrorKind, message: impl Into<String>) -> RepoError {
        RepoError {
            kind,
            message: message.into(),
        }
    }

    /// A failed helper request, made while `doing` something. The helper's own
    /// message is kept as it is for the errors a person can act on.
    fn agent(doing: &str, err: RequestError) -> RepoError {
        match err {
            RequestError::Agent { code, message } => {
                let kind = match code {
                    code::INVALID_PARAMS => RepoErrorKind::Invalid,
                    code::CLONE_FAILED => RepoErrorKind::Clone,
                    code::REPO_NOT_FOUND => RepoErrorKind::NotARepo,
                    _ => RepoErrorKind::Agent,
                };
                RepoError::new(kind, format!("{doing}: {message}"))
            }
            err => RepoError::new(RepoErrorKind::Agent, format!("{doing}: {err}")),
        }
    }

    fn unexpected(method: &str, reply: &Reply) -> RepoError {
        RepoError::new(
            RepoErrorKind::Agent,
            format!("The helper sent an unexpected answer to {method}: {reply:?}"),
        )
    }
}

/// The `cloneRepo` request for `repo`, checked the way the helper checks it.
fn clone_method(repo: RemoteRepoArg) -> Result<Method, RepoError> {
    check_clone(&repo.clone_url, &repo.name)
        .map_err(|message| RepoError::new(RepoErrorKind::Invalid, message))?;
    Ok(Method::CloneRepo {
        url: repo.clone_url,
        name: repo.name,
    })
}

/// Sends `method` to the helper off the main thread and waits up to `timeout`.
async fn request(
    app: AppHandle,
    method: Method,
    timeout: Duration,
) -> Result<Result<Reply, RequestError>, RepoError> {
    tauri::async_runtime::spawn_blocking(move || app.state::<Agent>().request(method, timeout))
        .await
        .map_err(|err| RepoError::new(RepoErrorKind::Internal, format!("The task failed: {err}")))
}

/// Clones `repo` into `~/projects/<name>` in WSL and returns the clone's Linux path.
/// When that folder is already a clone of the same URL, its path is returned as it is.
#[tauri::command]
pub async fn clone_repo(app: AppHandle, repo: RemoteRepoArg) -> Result<String, RepoError> {
    let method = clone_method(repo)?;
    match request(app, method, CLONE_TIMEOUT).await? {
        Ok(Reply::Path(path)) => Ok(path),
        Ok(other) => Err(RepoError::unexpected("cloneRepo", &other)),
        Err(err) => Err(RepoError::agent("Cannot clone the repo", err)),
    }
}

/// Adds the repo whose working tree holds `path` (a Linux, Windows drive or
/// `\\wsl.localhost` path), so the helper reports it from now on.
#[tauri::command]
pub async fn add_repo(app: AppHandle, path: String) -> Result<(), RepoError> {
    match request(app, Method::AddRepo { path }, ADD_TIMEOUT).await? {
        Ok(Reply::Done) => Ok(()),
        Ok(other) => Err(RepoError::unexpected("addRepo", &other)),
        Err(err) => Err(RepoError::agent("Cannot add the folder", err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_the_interface_remote_repo() {
        let repo: RemoteRepoArg = serde_json::from_value(json!({
            "kind": "github",
            "host": "example.com",
            "owner": "dev",
            "name": "reef-maps",
            "webUrl": "https://example.com/dev/reef-maps",
            "cloneUrl": "https://example.com/dev/reef-maps.git",
            "defaultBranch": "main",
            "language": null,
            "sizeKb": 12,
            "pushedAt": null,
            "fork": false,
            "archived": false,
            "private": true,
            "description": null
        }))
        .unwrap();
        assert_eq!(
            clone_method(repo),
            Ok(Method::CloneRepo {
                url: "https://example.com/dev/reef-maps.git".into(),
                name: "reef-maps".into(),
            })
        );
    }

    #[test]
    fn bad_clone_params_are_refused_before_the_helper() {
        for (url, name) in [
            ("git@example.com:dev/reef-maps.git", "reef-maps"),
            ("https://example.com/dev/reef-maps.git", "../reef-maps"),
            ("https://example.com/dev/reef-maps.git", "-reef-maps"),
        ] {
            let repo = RemoteRepoArg {
                clone_url: url.into(),
                name: name.into(),
            };
            assert_eq!(clone_method(repo).unwrap_err().kind, RepoErrorKind::Invalid);
        }
    }

    #[test]
    fn helper_errors_keep_their_meaning() {
        let agent = |code| RequestError::Agent {
            code,
            message: "/home/dev/projects/reef-maps already exists".into(),
        };
        let err = RepoError::agent("Cannot clone the repo", agent(code::CLONE_FAILED));
        assert_eq!(
            serde_json::to_value(&err).unwrap(),
            json!({"kind": "clone",
                "message": "Cannot clone the repo: /home/dev/projects/reef-maps already exists"})
        );
        assert_eq!(
            RepoError::agent("x", agent(code::REPO_NOT_FOUND)).kind,
            RepoErrorKind::NotARepo
        );
        assert_eq!(
            RepoError::agent("x", agent(code::INVALID_PARAMS)).kind,
            RepoErrorKind::Invalid
        );
        assert_eq!(
            RepoError::agent("x", agent(code::INTERNAL_ERROR)).kind,
            RepoErrorKind::Agent
        );
        let timeout = RepoError::agent("x", RequestError::Timeout);
        assert_eq!(
            serde_json::to_value(&timeout).unwrap()["kind"],
            json!("agent")
        );
        assert_eq!(timeout.message, "x: the helper did not answer in time");
    }
}
