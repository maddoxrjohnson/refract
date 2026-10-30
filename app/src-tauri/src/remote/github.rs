//! GitHub: the repos the user owns, collaborates on or reaches through an
//! organization (REST, paginated), and one repo's open pull requests and CI state
//! (one GraphQL query).

use serde::Deserialize;
use serde_json::{json, Value};

use super::http::{status_message, Http, Request, Response};
use super::{CiStatus, Details, RemoteKind, RemoteRepo};

pub const HOST: &str = "github.com";
const API: &str = "https://api.github.com";
/// 100 repos a page; this stops a runaway `Link` chain.
const MAX_PAGES: usize = 50;
const AUTH_HINT: &str =
    "Sign in again with `gh auth login`, or store a new token in Refract's settings.";

/// The first page of the user's repos.
pub fn list_url() -> String {
    format!(
        "{API}/user/repos?affiliation=owner,collaborator,organization_member&sort=pushed&per_page=100"
    )
}

/// `request` with the token and GitHub's API headers.
fn authed(request: Request, token: &str) -> Request {
    request
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
}

/// The `rel="next"` URL in a `Link` header, if it is on the API host, so the token
/// is never sent anywhere else.
pub fn next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        let is_next = params
            .split(';')
            .any(|p| p.trim().eq_ignore_ascii_case("rel=\"next\""));
        let url = url.trim().strip_prefix('<')?.strip_suffix('>')?;
        (is_next && url.starts_with(&format!("{API}/"))).then(|| url.to_owned())
    })
}

#[derive(Deserialize)]
struct ApiRepo {
    name: String,
    owner: ApiOwner,
    html_url: String,
    clone_url: String,
    default_branch: Option<String>,
    language: Option<String>,
    #[serde(default)]
    size: u64,
    pushed_at: Option<String>,
    #[serde(default)]
    fork: bool,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    private: bool,
    description: Option<String>,
}

#[derive(Deserialize)]
struct ApiOwner {
    login: String,
}

/// One page of `/user/repos`.
pub fn parse_repos(body: &str) -> Result<Vec<RemoteRepo>, String> {
    let repos: Vec<ApiRepo> = serde_json::from_str(body)
        .map_err(|err| format!("GitHub sent a repo list Refract cannot read: {err}"))?;
    Ok(repos
        .into_iter()
        .map(|r| RemoteRepo {
            kind: RemoteKind::Github,
            host: HOST.into(),
            owner: r.owner.login,
            name: r.name,
            web_url: r.html_url,
            clone_url: r.clone_url,
            default_branch: r.default_branch,
            language: r.language,
            size_kb: r.size,
            pushed_at: r.pushed_at,
            fork: r.fork,
            archived: r.archived,
            private: r.private,
            description: r.description.filter(|d| !d.trim().is_empty()),
        })
        .collect())
}

fn check(response: Response, now_unix: u64) -> Result<Response, String> {
    if response.is_success() {
        Ok(response)
    } else {
        Err(status_message("GitHub", AUTH_HINT, &response, now_unix))
    }
}

/// Every repo the token can list, following `Link` headers.
pub fn list_repos(http: &dyn Http, token: &str, now_unix: u64) -> Result<Vec<RemoteRepo>, String> {
    let mut repos = Vec::new();
    let mut url = Some(list_url());
    let mut pages = 0;
    while let Some(next) = url.take() {
        if pages == MAX_PAGES {
            break;
        }
        pages += 1;
        let response = check(http.send(&authed(Request::get(next), token))?, now_unix)?;
        repos.extend(parse_repos(&response.body)?);
        url = response.header("link").and_then(next_link);
    }
    Ok(repos)
}

const DETAILS_QUERY: &str = "query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    pullRequests(states: OPEN) { totalCount }
    defaultBranchRef {
      target { ... on Commit { url statusCheckRollup { state } } }
    }
  }
}";

/// The GraphQL request for one repo's details.
pub fn details_request(token: &str, owner: &str, name: &str) -> Request {
    let body = json!({
        "query": DETAILS_QUERY,
        "variables": { "owner": owner, "name": name },
    });
    authed(
        Request::post_json(format!("{API}/graphql"), body.to_string()),
        token,
    )
}

/// Reads the answer to [`details_request`]. No default branch (an empty repo) gives
/// no CI state; a branch without checks gives [`CiStatus::None`].
pub fn parse_details(body: &str) -> Result<Details, String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|err| format!("GitHub sent details Refract cannot read: {err}"))?;
    let repo = &value["data"]["repository"];
    if repo.is_null() {
        let message = value["errors"][0]["message"]
            .as_str()
            .unwrap_or("the repo was not found");
        return Err(format!("GitHub: {message}"));
    }
    let open = repo["pullRequests"]["totalCount"]
        .as_u64()
        .map(|n| n.min(u32::MAX as u64) as u32);
    let commit = &repo["defaultBranchRef"]["target"];
    let (ci, ci_url) = if commit.is_null() {
        (None, None)
    } else {
        let state = commit["statusCheckRollup"]["state"].as_str();
        let ci = match state {
            Some("SUCCESS") => CiStatus::Success,
            Some("FAILURE" | "ERROR") => CiStatus::Failure,
            Some("PENDING" | "EXPECTED") => CiStatus::Pending,
            _ => CiStatus::None,
        };
        let url = (ci != CiStatus::None)
            .then(|| commit["url"].as_str().map(str::to_owned))
            .flatten();
        (Some(ci), url)
    };
    Ok(Details {
        open_pull_requests: open,
        ci,
        ci_url,
    })
}

/// Open pull requests and CI on the default branch of `owner/name`.
pub fn details(
    http: &dyn Http,
    token: &str,
    owner: &str,
    name: &str,
    now_unix: u64,
) -> Result<Details, String> {
    let response = check(http.send(&details_request(token, owner, name))?, now_unix)?;
    parse_details(&response.body)
}

#[cfg(test)]
mod tests {
    use super::super::http::fake::{response, FakeHttp};
    use super::super::http::Method;
    use super::*;

    const PAGE_1: &str = r#"[
      {"name": "tidepool-api", "owner": {"login": "example-dev"},
       "html_url": "https://github.com/example-dev/tidepool-api",
       "clone_url": "https://github.com/example-dev/tidepool-api.git",
       "default_branch": "main", "language": "Rust", "size": 2048,
       "pushed_at": "2026-10-01T12:00:00Z", "fork": false, "archived": false,
       "private": true, "description": "Tide tables", "extra": 1},
      {"name": "lanternfish", "owner": {"login": "example-org"},
       "html_url": "https://github.com/example-org/lanternfish",
       "clone_url": "https://github.com/example-org/lanternfish.git",
       "default_branch": null, "language": null, "size": 0,
       "pushed_at": null, "fork": true, "archived": true,
       "private": false, "description": "  "}
    ]"#;
    const PAGE_2: &str = r#"[{"name": "kelp", "owner": {"login": "example-dev"},
       "html_url": "https://github.com/example-dev/kelp",
       "clone_url": "https://github.com/example-dev/kelp.git"}]"#;

    #[test]
    fn repos_are_parsed() {
        let repos = parse_repos(PAGE_1).unwrap();
        assert_eq!(repos.len(), 2);
        let first = &repos[0];
        assert_eq!(first.kind, RemoteKind::Github);
        assert_eq!(first.host, "github.com");
        assert_eq!(
            (first.owner.as_str(), first.name.as_str()),
            ("example-dev", "tidepool-api")
        );
        assert_eq!(first.default_branch.as_deref(), Some("main"));
        assert_eq!(first.size_kb, 2048);
        assert!(first.private && !first.fork && !first.archived);
        assert_eq!(first.description.as_deref(), Some("Tide tables"));
        let second = &repos[1];
        assert!(second.fork && second.archived && !second.private);
        assert_eq!(second.default_branch, None);
        assert_eq!(second.description, None);
        assert!(parse_repos(r#"{"message": "x"}"#).is_err());
    }

    #[test]
    fn next_links_stay_on_the_api() {
        let link = r#"<https://api.github.com/user/repos?page=2>; rel="next", <https://api.github.com/user/repos?page=5>; rel="last""#;
        assert_eq!(
            next_link(link).as_deref(),
            Some("https://api.github.com/user/repos?page=2")
        );
        let last = r#"<https://api.github.com/user/repos?page=1>; rel="prev""#;
        assert_eq!(next_link(last), None);
        assert_eq!(
            next_link(r#"<https://evil.example.com/x>; rel="next""#),
            None
        );
        assert_eq!(next_link(""), None);
    }

    #[test]
    fn listing_follows_pages_with_the_token() {
        let next = r#"<https://api.github.com/user/repos?page=2>; rel="next""#;
        let http = FakeHttp::new()
            .ok(
                "https://api.github.com/user/repos?affiliation=",
                PAGE_1,
                &[("link", next)],
            )
            .ok("https://api.github.com/user/repos?page=2", PAGE_2, &[]);
        let repos = list_repos(&http, "tok", 0).unwrap();
        assert_eq!(repos.len(), 3);
        assert_eq!(repos[2].name, "kelp");
        assert_eq!(
            http.urls(),
            [
                list_url(),
                "https://api.github.com/user/repos?page=2".into()
            ]
        );
        for sent in http.sent.borrow().iter() {
            assert_eq!(sent.header_value("authorization"), Some("Bearer tok"));
        }
        assert!(list_url().contains("affiliation=owner,collaborator,organization_member"));
    }

    #[test]
    fn listing_failures_are_worded() {
        let http = FakeHttp::new().answer(
            "https://api.github.com/",
            Ok(response(401, r#"{"message":"Bad credentials"}"#, &[])),
        );
        let err = list_repos(&http, "tok", 0).unwrap_err();
        assert!(err.starts_with("GitHub refused the token"), "{err}");
        let http = FakeHttp::new().answer(
            "https://api.github.com/",
            Err("Cannot reach api.github.com: down".into()),
        );
        assert_eq!(
            list_repos(&http, "tok", 0).unwrap_err(),
            "Cannot reach api.github.com: down"
        );
    }

    #[test]
    fn details_query_uses_variables() {
        let request = details_request("tok", "example-dev", "tidepool-api");
        assert_eq!(request.method, Method::Post);
        assert_eq!(request.url, "https://api.github.com/graphql");
        let body: Value = serde_json::from_str(request.body.as_deref().unwrap()).unwrap();
        assert_eq!(
            body["variables"],
            json!({"owner": "example-dev", "name": "tidepool-api"})
        );
        assert!(body["query"]
            .as_str()
            .unwrap()
            .contains("statusCheckRollup"));
    }

    fn details_body(target: Value) -> String {
        json!({"data": {"repository": {
            "pullRequests": {"totalCount": 3},
            "defaultBranchRef": if target.is_null() { Value::Null } else { json!({"target": target}) },
        }}})
        .to_string()
    }

    #[test]
    fn details_are_parsed() {
        let commit = "https://github.com/example-dev/tidepool-api/commit/abc123";
        let with = |state: Value| details_body(json!({"url": commit, "statusCheckRollup": state}));
        let d = parse_details(&with(json!({"state": "SUCCESS"}))).unwrap();
        assert_eq!(d.open_pull_requests, Some(3));
        assert_eq!(d.ci, Some(CiStatus::Success));
        assert_eq!(d.ci_url.as_deref(), Some(commit));
        for (state, ci) in [
            ("FAILURE", CiStatus::Failure),
            ("ERROR", CiStatus::Failure),
            ("PENDING", CiStatus::Pending),
            ("EXPECTED", CiStatus::Pending),
        ] {
            assert_eq!(
                parse_details(&with(json!({"state": state}))).unwrap().ci,
                Some(ci)
            );
        }
        let none = parse_details(&with(Value::Null)).unwrap();
        assert_eq!((none.ci, none.ci_url), (Some(CiStatus::None), None));
        let empty = parse_details(&details_body(Value::Null)).unwrap();
        assert_eq!((empty.ci, empty.ci_url), (None, None));
    }

    #[test]
    fn missing_repos_are_errors() {
        let body = r#"{"data": {"repository": null}, "errors": [{"type": "NOT_FOUND", "message": "Could not resolve to a Repository with the name 'example-dev/gone'."}]}"#;
        assert_eq!(
            parse_details(body).unwrap_err(),
            "GitHub: Could not resolve to a Repository with the name 'example-dev/gone'."
        );
        assert!(parse_details("not json").is_err());
    }

    /// Lists the real repos with the token Refract would use, printing only the count
    /// and the first repo's flags. Run by hand:
    /// `cargo xwin test --target x86_64-pc-windows-msvc -- --ignored lists_real_github_repos --nocapture`.
    #[test]
    #[ignore = "uses the network and this machine's GitHub login"]
    fn lists_real_github_repos() {
        let (token, source) = crate::creds::github_token(crate::creds::tests::test_distro)
            .expect("no GitHub token found");
        let now = super::super::unix_now();
        let repos = list_repos(&super::super::http::Client::new(), &token, now).unwrap();
        println!("token from {source:?}; {} repos", repos.len());
        if let Some(first) = repos.first() {
            println!(
                "first: private={} fork={} archived={}",
                first.private, first.fork, first.archived
            );
        }
        assert!(!repos.is_empty());
    }
}
