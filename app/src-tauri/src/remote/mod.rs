//! Repos on GitHub and GitLab: listing them, and one repo's pull requests and CI.
//! Answers are cached about 15 minutes (see `cache`); tokens come from `creds`.

mod cache;
pub mod github;
pub mod gitlab;
mod http;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::agent::Agent;
use crate::creds;
use crate::settings;
use cache::Slot;
use http::Client;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteKind {
    Github,
    Gitlab,
}

/// A repo the user owns, collaborates on or is a member of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteRepo {
    pub kind: RemoteKind,
    /// `github.com` or the GitLab host name, lower case and without a port.
    pub host: String,
    /// Owner login, or the GitLab namespace path (may contain `/`).
    pub owner: String,
    pub name: String,
    pub web_url: String,
    /// HTTPS clone URL.
    pub clone_url: String,
    pub default_branch: Option<String>,
    pub language: Option<String>,
    pub size_kb: u64,
    /// RFC 3339.
    pub pushed_at: Option<String>,
    pub fork: bool,
    pub archived: bool,
    pub private: bool,
    pub description: Option<String>,
}

/// How listing one source went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSource {
    pub kind: RemoteKind,
    /// The host name, as in [`RemoteRepo::host`].
    pub host: String,
    /// False when there is no URL or token for it, so it was not asked.
    pub configured: bool,
    /// Why listing failed, for a person.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteList {
    pub repos: Vec<RemoteRepo>,
    pub sources: Vec<RemoteSource>,
    /// RFC 3339: when the oldest of the listed data was fetched.
    pub fetched_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CiStatus {
    Success,
    Failure,
    Pending,
    None,
}

/// What GitHub or GitLab said about one repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Details {
    pub open_pull_requests: Option<u32>,
    /// `None` when there is no default branch to ask about.
    pub ci: Option<CiStatus>,
    pub ci_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDetails {
    pub open_pull_requests: Option<u32>,
    pub ci: Option<CiStatus>,
    pub ci_url: Option<String>,
    /// RFC 3339.
    pub fetched_at: String,
}

/// Names a remote repo in requests.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteRef {
    pub host: String,
    pub owner: String,
    pub name: String,
}

/// Where each token comes from; never the tokens themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    pub github: GithubCredential,
    pub gitlab: GitlabCredential,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum GithubCredential {
    GhCli,
    Stored,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum GitlabCredential {
    Stored,
    Missing,
}

/// Why a remote command failed, sent as `{"kind": "failed", "message": "..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteError {
    pub kind: RemoteErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RemoteErrorKind {
    /// The request was refused before anything was sent.
    Invalid,
    /// There is no token or address for the repo's host.
    NotConfigured,
    /// GitHub, GitLab or Credential Manager failed.
    Failed,
    /// The task itself failed.
    Internal,
}

impl RemoteError {
    fn new(kind: RemoteErrorKind, message: impl Into<String>) -> RemoteError {
        RemoteError {
            kind,
            message: message.into(),
        }
    }
}

const GITHUB_KEY: &str = "github";

/// Cache slots by key. The map is locked only to find a slot; each slot has its own
/// lock, held while it is fetched, so a slow source or repo holds up only callers
/// asking for that same one, and they then get its answer.
type Slots<T> = Mutex<HashMap<String, Arc<Mutex<Slot<T>>>>>;

/// The caches, managed as Tauri state.
pub struct Remote {
    http: Client,
    /// Keyed by `github` or `gitlab:<base URL>`.
    lists: Slots<Vec<RemoteRepo>>,
    /// Keyed by [`details_key`].
    details: Slots<Details>,
}

impl Remote {
    pub fn new() -> Remote {
        Remote {
            http: Client::new(),
            lists: Mutex::default(),
            details: Mutex::default(),
        }
    }

    /// Drops everything cached for `kind`, after its token changed.
    fn forget(&self, kind: RemoteKind) {
        let is_kind = |key: &str| match kind {
            RemoteKind::Github => key == GITHUB_KEY || key.starts_with("github.com/"),
            RemoteKind::Gitlab => key != GITHUB_KEY && !key.starts_with("github.com/"),
        };
        lock(&self.lists).retain(|key, _| !is_kind(key));
        lock(&self.details).retain(|key, _| !is_kind(key));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The slot for `key`, made if there is none. A fetch still running for a slot
/// that [`Remote::forget`] dropped stores its answer in the dropped slot.
fn slot<T>(slots: &Slots<T>, key: &str) -> Arc<Mutex<Slot<T>>> {
    lock(slots).entry(key.to_owned()).or_default().clone()
}

/// Seconds since 1970, for rate limit resets.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `time` as RFC 3339 in UTC, to the second.
pub fn rfc3339(time: SystemTime) -> String {
    let secs = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's civil_from_days, for days since 1970-01-01.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// The cache key for one repo: `host/owner/name` in lower case.
pub fn details_key(repo: &RemoteRef) -> String {
    format!("{}/{}/{}", repo.host, repo.owner, repo.name).to_ascii_lowercase()
}

/// Checks a [`RemoteRef`] from the interface: no part empty or with control
/// characters, `..` or a leading or trailing `/`, and a name without `/`.
pub fn check_ref(repo: &RemoteRef) -> Result<(), RemoteError> {
    let parts = [&repo.host, &repo.owner, &repo.name];
    let bad = parts.iter().any(|part| {
        part.is_empty()
            || part.chars().any(|c| c.is_control() || c.is_whitespace())
            || part.starts_with('/')
            || part.ends_with('/')
            || part
                .split('/')
                .any(|s| s.is_empty() || s == "." || s == "..")
    }) || repo.name.contains('/')
        || repo.host.contains('/');
    if bad {
        return Err(RemoteError::new(
            RemoteErrorKind::Invalid,
            format!(
                "Not a repo Refract can look up: {}/{}/{}",
                repo.host, repo.owner, repo.name
            ),
        ));
    }
    Ok(())
}

/// The distro the helper runs in, for `gh` in WSL.
fn distro(app: &AppHandle) -> Option<String> {
    app.try_state::<Agent>().and_then(|agent| agent.distro())
}

/// The stored GitLab token, with a read failure worded for a person.
fn gitlab_token() -> Result<Option<String>, String> {
    creds::read(creds::GITLAB_TARGET)
        .map_err(|err| format!("Cannot read the GitLab token from Credential Manager: {err}"))
}

/// Lists one source through its cache slot and adds what it has to `list`.
fn add_source(
    list: &mut RemoteList,
    oldest: &mut Option<SystemTime>,
    kind: RemoteKind,
    host: String,
    slot: &Slot<Vec<RemoteRepo>>,
    configured: bool,
) {
    if let Some((repos, at)) = slot.good() {
        list.repos.extend(repos.iter().cloned());
        *oldest = Some(oldest.map_or(at, |o| o.min(at)));
    }
    list.sources.push(RemoteSource {
        kind,
        host,
        configured,
        error: slot.error().map(str::to_owned),
    });
}

fn list_blocking(app: &AppHandle, refresh: bool) -> RemoteList {
    let remote = app.state::<Remote>();
    let settings = settings::load(app);
    let (now, wall, unix) = (Instant::now(), SystemTime::now(), unix_now());
    let mut list = RemoteList {
        repos: Vec::new(),
        sources: Vec::new(),
        fetched_at: String::new(),
    };
    let mut oldest = None;

    let github_slot = slot(&remote.lists, GITHUB_KEY);
    let mut github_slot = lock(&github_slot);
    let configured = cache::update(
        &mut github_slot,
        refresh,
        now,
        wall,
        || Ok(creds::github_token(|| distro(app)).map(|(token, _)| token)),
        |token| github::list_repos(&remote.http, token, unix),
    );
    add_source(
        &mut list,
        &mut oldest,
        RemoteKind::Github,
        github::HOST.into(),
        &github_slot,
        configured,
    );
    drop(github_slot);

    match settings.gitlab_url.as_deref() {
        Some(base) => {
            let host = gitlab::host(base).unwrap_or_default();
            let gitlab_slot = slot(&remote.lists, &format!("gitlab:{base}"));
            let mut gitlab_slot = lock(&gitlab_slot);
            let configured = cache::update(
                &mut gitlab_slot,
                refresh,
                now,
                wall,
                gitlab_token,
                |token| gitlab::list_projects(&remote.http, base, token, unix),
            );
            add_source(
                &mut list,
                &mut oldest,
                RemoteKind::Gitlab,
                host,
                &gitlab_slot,
                configured,
            );
        }
        None => list.sources.push(RemoteSource {
            kind: RemoteKind::Gitlab,
            host: String::new(),
            configured: false,
            error: None,
        }),
    }
    // Lists for GitLab addresses no longer in settings.
    let current = settings.gitlab_url.map(|base| format!("gitlab:{base}"));
    lock(&remote.lists).retain(|key, _| key == GITHUB_KEY || Some(key) == current.as_ref());

    list.fetched_at = rfc3339(oldest.unwrap_or(wall));
    list
}

fn details_blocking(
    app: &AppHandle,
    repo: RemoteRef,
    refresh: bool,
) -> Result<RemoteDetails, RemoteError> {
    check_ref(&repo)?;
    let remote = app.state::<Remote>();
    let (now, wall, unix) = (Instant::now(), SystemTime::now(), unix_now());
    let host = repo.host.to_ascii_lowercase();
    let not_configured = |what: &str| RemoteError::new(RemoteErrorKind::NotConfigured, what);
    // GitLab's base URL, which may have a port that `host` leaves out.
    let gitlab_base = if host == github::HOST {
        None
    } else {
        let base = settings::load(app)
            .gitlab_url
            .filter(|base| gitlab::host(base).as_deref() == Some(host.as_str()))
            .ok_or_else(|| {
                not_configured(&format!(
                    "Refract has no GitHub or GitLab account set up for {host}."
                ))
            })?;
        Some(base)
    };
    let slot = slot(&remote.details, &details_key(&repo));
    let mut slot = lock(&slot);
    let configured = match &gitlab_base {
        None => cache::update(
            &mut slot,
            refresh,
            now,
            wall,
            || Ok(creds::github_token(|| distro(app)).map(|(token, _)| token)),
            |token| github::details(&remote.http, token, &repo.owner, &repo.name, unix),
        ),
        Some(base) => cache::update(&mut slot, refresh, now, wall, gitlab_token, |token| {
            gitlab::details(&remote.http, base, token, &repo.owner, &repo.name, unix)
        }),
    };
    if !configured {
        return Err(not_configured(if host == github::HOST {
            "Sign in to GitHub with `gh auth login`, or store a token in Refract's settings."
        } else {
            "Store a GitLab token in Refract's settings."
        }));
    }
    if let Some(err) = slot.error() {
        eprintln!("refract: details for {host}: {err}");
    }
    match slot.good() {
        Some((d, at)) => Ok(RemoteDetails {
            open_pull_requests: d.open_pull_requests,
            ci: d.ci,
            ci_url: d.ci_url.clone(),
            fetched_at: rfc3339(at),
        }),
        None => Err(RemoteError::new(
            RemoteErrorKind::Failed,
            slot.error().unwrap_or("No details were fetched."),
        )),
    }
}

async fn run_blocking<T: Send + 'static>(
    task: impl FnOnce() -> Result<T, RemoteError> + Send + 'static,
) -> Result<T, RemoteError> {
    tauri::async_runtime::spawn_blocking(task)
        .await
        .map_err(|err| {
            RemoteError::new(
                RemoteErrorKind::Internal,
                format!("The remote task failed: {err}"),
            )
        })?
}

/// Repos from GitHub and GitLab, cached; `refresh` asks again now.
#[tauri::command]
pub async fn remote_list(app: AppHandle, refresh: bool) -> Result<RemoteList, RemoteError> {
    run_blocking(move || Ok(list_blocking(&app, refresh))).await
}

/// Open pull or merge requests and CI on the default branch of one repo, cached.
#[tauri::command]
pub async fn remote_details(
    app: AppHandle,
    repo: RemoteRef,
    refresh: bool,
) -> Result<RemoteDetails, RemoteError> {
    run_blocking(move || details_blocking(&app, repo, refresh)).await
}

/// Where the tokens come from. May run `gh auth token`.
#[tauri::command]
pub async fn credentials(app: AppHandle) -> Result<Credentials, RemoteError> {
    run_blocking(move || {
        let github = match creds::read(creds::GITHUB_TARGET) {
            Ok(Some(_)) => GithubCredential::Stored,
            _ => match creds::gh_cli_token(|| distro(&app)) {
                Some(_) => GithubCredential::GhCli,
                None => GithubCredential::Missing,
            },
        };
        let gitlab = match gitlab_token() {
            Ok(Some(_)) => GitlabCredential::Stored,
            Ok(None) => GitlabCredential::Missing,
            Err(err) => {
                eprintln!("refract: {err}");
                GitlabCredential::Missing
            }
        };
        Ok(Credentials { github, gitlab })
    })
    .await
}

/// Stores a token in Credential Manager, or removes it when `token` is `None` or
/// blank, and drops what was cached with the old one.
#[tauri::command]
pub async fn set_token(
    app: AppHandle,
    kind: RemoteKind,
    token: Option<String>,
) -> Result<(), RemoteError> {
    let token = token.filter(|t| !t.trim().is_empty());
    let token = match &token {
        Some(token) => Some(
            creds::check_token(token)
                .map_err(|err| RemoteError::new(RemoteErrorKind::Invalid, err))?
                .to_owned(),
        ),
        None => None,
    };
    let target = match kind {
        RemoteKind::Github => creds::GITHUB_TARGET,
        RemoteKind::Gitlab => creds::GITLAB_TARGET,
    };
    run_blocking(move || {
        let result = match &token {
            Some(token) => creds::write(target, token),
            None => creds::delete(target),
        };
        result.map_err(|err| {
            RemoteError::new(
                RemoteErrorKind::Failed,
                format!("Cannot save the token in Credential Manager: {err}"),
            )
        })?;
        app.state::<Remote>().forget(kind);
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn times_are_rfc3339() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let at = |secs| rfc3339(UNIX_EPOCH + Duration::from_secs(secs));
        assert_eq!(at(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(at(1_793_318_399), "2026-10-29T23:59:59Z");
        assert_eq!(at(4_107_542_400), "2100-03-01T00:00:00Z");
    }

    fn repo_ref(host: &str, owner: &str, name: &str) -> RemoteRef {
        RemoteRef {
            host: host.into(),
            owner: owner.into(),
            name: name.into(),
        }
    }

    #[test]
    fn refs_are_checked() {
        assert!(check_ref(&repo_ref("github.com", "example-dev", "tidepool-api")).is_ok());
        assert!(check_ref(&repo_ref(
            "gitlab.example.com",
            "example-group/app",
            "reef-survey"
        ))
        .is_ok());
        for (host, owner, name) in [
            ("", "a", "b"),
            ("github.com", "", "b"),
            ("github.com", "a", ""),
            ("github.com", "a", "b/c"),
            ("github.com", "/a", "b"),
            ("github.com", "a/", "b"),
            ("github.com", "a//c", "b"),
            ("github.com", "..", "b"),
            ("github.com", "a", ".."),
            ("github.com/x", "a", "b"),
            ("github.com", "a b", "c"),
            ("github.com", "a", "b\n"),
        ] {
            let err = check_ref(&repo_ref(host, owner, name)).unwrap_err();
            assert_eq!(err.kind, RemoteErrorKind::Invalid, "{host} {owner} {name}");
        }
        assert_eq!(
            details_key(&repo_ref("GitHub.com", "Example-Dev", "Tidepool-API")),
            "github.com/example-dev/tidepool-api"
        );
    }

    #[test]
    fn wire_shapes_are_camel_case() {
        let repo = RemoteRepo {
            kind: RemoteKind::Gitlab,
            host: "gitlab.example.com".into(),
            owner: "example-group/app".into(),
            name: "reef-survey".into(),
            web_url: "https://gitlab.example.com/example-group/app/reef-survey".into(),
            clone_url: "https://gitlab.example.com/example-group/app/reef-survey.git".into(),
            default_branch: Some("main".into()),
            language: None,
            size_kb: 12,
            pushed_at: None,
            fork: false,
            archived: false,
            private: true,
            description: None,
        };
        assert_eq!(
            serde_json::to_value(&repo).unwrap(),
            json!({
                "kind": "gitlab", "host": "gitlab.example.com", "owner": "example-group/app",
                "name": "reef-survey",
                "webUrl": "https://gitlab.example.com/example-group/app/reef-survey",
                "cloneUrl": "https://gitlab.example.com/example-group/app/reef-survey.git",
                "defaultBranch": "main", "language": null, "sizeKb": 12, "pushedAt": null,
                "fork": false, "archived": false, "private": true, "description": null,
            })
        );
        let details = RemoteDetails {
            open_pull_requests: Some(2),
            ci: Some(CiStatus::None),
            ci_url: None,
            fetched_at: "2026-10-29T00:00:00Z".into(),
        };
        assert_eq!(
            serde_json::to_value(&details).unwrap(),
            json!({"openPullRequests": 2, "ci": "none", "ciUrl": null, "fetchedAt": "2026-10-29T00:00:00Z"})
        );
        let source = RemoteSource {
            kind: RemoteKind::Github,
            host: "github.com".into(),
            configured: true,
            error: None,
        };
        assert_eq!(
            serde_json::to_value(&source).unwrap(),
            json!({"kind": "github", "host": "github.com", "configured": true, "error": null})
        );
        assert_eq!(
            serde_json::to_value(Credentials {
                github: GithubCredential::GhCli,
                gitlab: GitlabCredential::Missing,
            })
            .unwrap(),
            json!({"github": "ghCli", "gitlab": "missing"})
        );
        assert_eq!(
            serde_json::to_value(RemoteError::new(RemoteErrorKind::NotConfigured, "x")).unwrap(),
            json!({"kind": "notConfigured", "message": "x"})
        );
        let parsed: RemoteRef =
            serde_json::from_value(json!({"host": "github.com", "owner": "o", "name": "n"}))
                .unwrap();
        assert_eq!(parsed, repo_ref("github.com", "o", "n"));
    }

    #[test]
    fn sources_add_their_last_good_repos() {
        let mut list = RemoteList {
            repos: Vec::new(),
            sources: Vec::new(),
            fetched_at: String::new(),
        };
        let mut oldest = None;
        let mut slot = Slot::default();
        let early = UNIX_EPOCH + Duration::from_secs(100);
        slot.record(Ok(Vec::new()), Instant::now(), early);
        slot.record(
            Err("GitHub answered 500.".into()),
            Instant::now(),
            UNIX_EPOCH,
        );
        add_source(
            &mut list,
            &mut oldest,
            RemoteKind::Github,
            "github.com".into(),
            &slot,
            true,
        );
        assert_eq!(oldest, Some(early));
        assert_eq!(
            list.sources[0].error.as_deref(),
            Some("GitHub answered 500.")
        );
    }
}
