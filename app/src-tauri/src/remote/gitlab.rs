//! GitLab: the projects the user is a member of on the instance in settings, and
//! one project's open merge requests and latest pipeline on its default branch
//! (REST, the token as a bearer token).

use serde::Deserialize;
use serde_json::Value;

use super::http::{encode, status_message, url_host, Http, Request, Response};
use super::{CiStatus, Details, RemoteKind, RemoteRepo};

/// 100 projects a page; this stops a runaway `X-Next-Page` chain.
const MAX_PAGES: usize = 50;
const AUTH_HINT: &str = "Store a new personal access token (read_api scope) in Refract's settings.";

/// The host name of a GitLab base URL, in lower case and without a port, as repos'
/// origin URLs name it. API calls still use the base URL, port and all.
pub fn host(base: &str) -> Option<String> {
    url_host(base).map(str::to_ascii_lowercase)
}

/// Page `page` of the user's projects.
pub fn list_url(base: &str, page: u32) -> String {
    format!(
        "{base}/api/v4/projects?membership=true&statistics=true&order_by=last_activity_at&per_page=100&page={page}"
    )
}

/// `request` with the token in `Authorization`, which `ureq` would drop on a
/// redirect to another host (it does not follow any; see `http`).
fn authed(request: Request, token: &str) -> Request {
    request.header("Authorization", format!("Bearer {token}"))
}

fn check(response: Response, now_unix: u64) -> Result<Response, String> {
    if response.is_success() {
        Ok(response)
    } else if (300..400).contains(&response.status) {
        Err(match response.header("location") {
            Some(to) => format!(
                "The GitLab address moved to {}; update it in Refract's settings.",
                to.trim()
            ),
            None => format!(
                "GitLab answered {} (a redirect); check the address in Refract's settings.",
                response.status
            ),
        })
    } else {
        Err(status_message("GitLab", AUTH_HINT, &response, now_unix))
    }
}

/// The page after this one, from `X-Next-Page` (empty on the last page).
fn next_page(response: &Response) -> Option<u32> {
    response.header("x-next-page")?.trim().parse().ok()
}

#[derive(Deserialize)]
struct ApiProject {
    path: String,
    namespace: ApiNamespace,
    web_url: String,
    http_url_to_repo: String,
    default_branch: Option<String>,
    last_activity_at: Option<String>,
    forked_from_project: Option<Value>,
    #[serde(default)]
    archived: bool,
    visibility: Option<String>,
    description: Option<String>,
    statistics: Option<ApiStatistics>,
}

#[derive(Deserialize)]
struct ApiNamespace {
    full_path: String,
}

#[derive(Deserialize)]
struct ApiStatistics {
    #[serde(default)]
    repository_size: u64,
}

/// One page of `/projects`. Only `public` projects count as not private.
pub fn parse_projects(body: &str, host: &str) -> Result<Vec<RemoteRepo>, String> {
    let projects: Vec<ApiProject> = serde_json::from_str(body)
        .map_err(|err| format!("GitLab sent a project list Refract cannot read: {err}"))?;
    Ok(projects
        .into_iter()
        .map(|p| RemoteRepo {
            kind: RemoteKind::Gitlab,
            host: host.to_owned(),
            owner: p.namespace.full_path,
            name: p.path,
            web_url: p.web_url,
            clone_url: p.http_url_to_repo,
            default_branch: p.default_branch,
            language: None,
            size_kb: p.statistics.map_or(0, |s| s.repository_size / 1024),
            pushed_at: p.last_activity_at,
            fork: p.forked_from_project.is_some_and(|f| !f.is_null()),
            archived: p.archived,
            private: p.visibility.as_deref() != Some("public"),
            description: p.description.filter(|d| !d.trim().is_empty()),
        })
        .collect())
}

/// Every project the user is a member of on the instance at `base`.
pub fn list_projects(
    http: &dyn Http,
    base: &str,
    token: &str,
    now_unix: u64,
) -> Result<Vec<RemoteRepo>, String> {
    let host =
        host(base).ok_or_else(|| format!("{base} is not a GitLab address Refract can use."))?;
    let mut repos = Vec::new();
    let mut page = Some(1);
    while let Some(current) = page.take() {
        if current as usize > MAX_PAGES {
            break;
        }
        let request = authed(Request::get(list_url(base, current)), token);
        let response = check(http.send(&request)?, now_unix)?;
        repos.extend(parse_projects(&response.body, &host)?);
        page = next_page(&response).filter(|next| *next > current);
    }
    Ok(repos)
}

/// The project URL for `owner/name`, which GitLab takes as one encoded segment.
pub fn project_url(base: &str, owner: &str, name: &str) -> String {
    format!(
        "{base}/api/v4/projects/{}",
        encode(&format!("{owner}/{name}"))
    )
}

/// The open merge request count: `X-Total`, else the page's length when it is the
/// only page. GitLab leaves `X-Total` out above 10,000.
pub fn merge_request_count(response: &Response) -> Option<u32> {
    if let Some(total) = response
        .header("x-total")
        .and_then(|t| t.trim().parse().ok())
    {
        return Some(total);
    }
    if next_page(response).is_some() {
        return None;
    }
    let items: Vec<Value> = serde_json::from_str(&response.body).ok()?;
    Some(items.len() as u32)
}

/// The newest pipeline in a `/pipelines` answer, as a state and its page. No
/// pipeline gives [`CiStatus::None`].
pub fn parse_pipeline(body: &str) -> Result<(CiStatus, Option<String>), String> {
    let pipelines: Vec<Value> = serde_json::from_str(body)
        .map_err(|err| format!("GitLab sent pipelines Refract cannot read: {err}"))?;
    let Some(latest) = pipelines.first() else {
        return Ok((CiStatus::None, None));
    };
    let ci = match latest["status"].as_str() {
        Some("success") => CiStatus::Success,
        Some("failed") => CiStatus::Failure,
        Some(
            "running"
            | "pending"
            | "created"
            | "waiting_for_resource"
            | "preparing"
            | "scheduled"
            | "waiting_for_callback",
        ) => CiStatus::Pending,
        // canceled, skipped, manual: no result to show.
        _ => CiStatus::None,
    };
    Ok((ci, latest["web_url"].as_str().map(str::to_owned)))
}

/// Open merge requests and the latest pipeline on the default branch of
/// `owner/name`.
pub fn details(
    http: &dyn Http,
    base: &str,
    token: &str,
    owner: &str,
    name: &str,
    now_unix: u64,
) -> Result<Details, String> {
    let get = |url: String| -> Result<Response, String> {
        check(http.send(&authed(Request::get(url), token))?, now_unix)
    };
    let project: Value = serde_json::from_str(&get(project_url(base, owner, name))?.body)
        .map_err(|err| format!("GitLab sent a project Refract cannot read: {err}"))?;
    let id = project["id"]
        .as_u64()
        .ok_or("GitLab sent a project without an id.")?;
    let api = format!("{base}/api/v4/projects/{id}");
    let open = merge_request_count(&get(format!(
        "{api}/merge_requests?state=opened&per_page=100"
    ))?);
    let (ci, ci_url) = match project["default_branch"].as_str() {
        Some(branch) => {
            let url = format!("{api}/pipelines?ref={}&per_page=1", encode(branch));
            let (ci, ci_url) = parse_pipeline(&get(url)?.body)?;
            (Some(ci), ci_url)
        }
        None => (None, None),
    };
    Ok(Details {
        open_pull_requests: open,
        ci,
        ci_url,
    })
}

#[cfg(test)]
mod tests {
    use super::super::http::fake::{response, FakeHttp};
    use super::*;

    const BASE: &str = "https://gitlab.example.com";

    const PAGE_1: &str = r#"[
      {"id": 11, "path": "reef-survey", "path_with_namespace": "example-group/app/reef-survey",
       "namespace": {"full_path": "example-group/app"},
       "web_url": "https://gitlab.example.com/example-group/app/reef-survey",
       "http_url_to_repo": "https://gitlab.example.com/example-group/app/reef-survey.git",
       "default_branch": "main", "last_activity_at": "2026-10-02T09:30:00.000Z",
       "archived": false, "visibility": "internal", "description": "Survey data",
       "statistics": {"repository_size": 3145728}},
      {"id": 12, "path": "notes", "namespace": {"full_path": "example-user"},
       "web_url": "https://gitlab.example.com/example-user/notes",
       "http_url_to_repo": "https://gitlab.example.com/example-user/notes.git",
       "default_branch": null, "last_activity_at": null,
       "forked_from_project": {"id": 3}, "archived": true, "visibility": "public",
       "description": ""}
    ]"#;
    const PAGE_2: &str = r#"[{"id": 13, "path": "kelp", "namespace": {"full_path": "example-user"},
       "web_url": "https://gitlab.example.com/example-user/kelp",
       "http_url_to_repo": "https://gitlab.example.com/example-user/kelp.git",
       "visibility": "private"}]"#;

    #[test]
    fn hosts_drop_their_port() {
        assert_eq!(host(BASE).as_deref(), Some("gitlab.example.com"));
        assert_eq!(
            host("https://GitLab.Example.com:8443/sub").as_deref(),
            Some("gitlab.example.com")
        );
        assert_eq!(
            host("https://gitlab.example.com:443").as_deref(),
            Some("gitlab.example.com")
        );
        assert_eq!(host("http://gitlab.example.com"), None);
        assert_eq!(host("https://"), None);
    }

    #[test]
    fn projects_are_parsed() {
        let repos = parse_projects(PAGE_1, "gitlab.example.com").unwrap();
        let first = &repos[0];
        assert_eq!(first.kind, RemoteKind::Gitlab);
        assert_eq!(first.host, "gitlab.example.com");
        assert_eq!(
            (first.owner.as_str(), first.name.as_str()),
            ("example-group/app", "reef-survey")
        );
        assert_eq!(first.size_kb, 3072);
        assert!(first.private && !first.fork && !first.archived);
        assert_eq!(first.pushed_at.as_deref(), Some("2026-10-02T09:30:00.000Z"));
        let second = &repos[1];
        assert!(!second.private && second.fork && second.archived);
        assert_eq!((second.size_kb, second.description.as_deref()), (0, None));
        assert!(parse_projects("{}", "x").is_err());
    }

    #[test]
    fn listing_follows_next_page_with_the_token() {
        let http = FakeHttp::new()
            .ok(&list_url(BASE, 1), PAGE_1, &[("x-next-page", "2")])
            .ok(&list_url(BASE, 2), PAGE_2, &[("x-next-page", "")]);
        let repos = list_projects(&http, BASE, "glpat", 0).unwrap();
        assert_eq!(repos.len(), 3);
        assert_eq!(http.urls(), [list_url(BASE, 1), list_url(BASE, 2)]);
        for sent in http.sent.borrow().iter() {
            assert_eq!(sent.header_value("authorization"), Some("Bearer glpat"));
            assert_eq!(sent.header_value("private-token"), None);
            let carrying = sent.headers.iter().filter(|(_, v)| v.contains("glpat"));
            assert_eq!(carrying.count(), 1);
        }
        assert!(list_url(BASE, 1).contains("membership=true"));
    }

    #[test]
    fn listing_failures_are_worded() {
        let http = FakeHttp::new().answer(
            BASE,
            Ok(response(401, r#"{"message":"401 Unauthorized"}"#, &[])),
        );
        let err = list_projects(&http, BASE, "glpat", 0).unwrap_err();
        assert!(err.starts_with("GitLab refused the token"), "{err}");
        assert!(list_projects(&FakeHttp::new(), "http://x", "t", 0).is_err());
    }

    #[test]
    fn redirects_are_errors_naming_the_new_address() {
        let moved = "https://git.example.org/api/v4/projects";
        let http = FakeHttp::new().answer(BASE, Ok(response(301, "", &[("location", moved)])));
        let err = list_projects(&http, BASE, "glpat", 0).unwrap_err();
        assert_eq!(
            err,
            format!("The GitLab address moved to {moved}; update it in Refract's settings.")
        );
        assert_eq!(http.urls(), [list_url(BASE, 1)]);
        let http = FakeHttp::new().answer(BASE, Ok(response(302, "", &[])));
        let err = list_projects(&http, BASE, "glpat", 0).unwrap_err();
        assert!(err.contains("302") && !err.contains("glpat"), "{err}");
    }

    #[test]
    fn ported_instances_use_the_port_for_api_calls() {
        for base in [
            "https://gitlab.example.com:8443",
            "https://gitlab.example.com:443",
        ] {
            let http = FakeHttp::new().ok(&list_url(base, 1), PAGE_2, &[]);
            let repos = list_projects(&http, base, "glpat", 0).unwrap();
            assert_eq!(repos[0].host, "gitlab.example.com");
            assert_eq!(http.urls(), [list_url(base, 1)]);
        }
    }

    #[test]
    fn project_paths_are_one_segment() {
        assert_eq!(
            project_url(BASE, "example-group/app", "reef-survey"),
            "https://gitlab.example.com/api/v4/projects/example-group%2Fapp%2Freef-survey"
        );
    }

    #[test]
    fn merge_requests_are_counted() {
        assert_eq!(
            merge_request_count(&response(200, "[]", &[("x-total", "7")])),
            Some(7)
        );
        assert_eq!(
            merge_request_count(&response(200, "[{}, {}]", &[("x-next-page", "")])),
            Some(2)
        );
        assert_eq!(
            merge_request_count(&response(200, "[{}]", &[("x-next-page", "2")])),
            None
        );
    }

    #[test]
    fn pipelines_are_parsed() {
        let one = |status: &str| {
            format!(
                r#"[{{"status": "{status}", "web_url": "https://gitlab.example.com/p/-/pipelines/9"}}]"#
            )
        };
        assert_eq!(
            parse_pipeline(&one("success")).unwrap(),
            (
                CiStatus::Success,
                Some("https://gitlab.example.com/p/-/pipelines/9".into())
            )
        );
        assert_eq!(parse_pipeline(&one("failed")).unwrap().0, CiStatus::Failure);
        assert_eq!(
            parse_pipeline(&one("running")).unwrap().0,
            CiStatus::Pending
        );
        assert_eq!(parse_pipeline(&one("canceled")).unwrap().0, CiStatus::None);
        assert_eq!(parse_pipeline("[]").unwrap(), (CiStatus::None, None));
        assert!(parse_pipeline("{}").is_err());
    }

    #[test]
    fn details_use_the_default_branch() {
        let api = "https://gitlab.example.com/api/v4/projects/11";
        let http = FakeHttp::new()
            .ok(
                &project_url(BASE, "example-group/app", "reef-survey"),
                r#"{"id": 11, "default_branch": "release/2026"}"#,
                &[],
            )
            .ok(&format!("{api}/merge_requests"), "[]", &[("x-total", "4")])
            .ok(
                &format!("{api}/pipelines?ref=release%2F2026"),
                r#"[{"status": "pending", "web_url": "https://gitlab.example.com/x/-/pipelines/2"}]"#,
                &[],
            );
        let d = details(&http, BASE, "glpat", "example-group/app", "reef-survey", 0).unwrap();
        assert_eq!(d.open_pull_requests, Some(4));
        assert_eq!(d.ci, Some(CiStatus::Pending));
        assert_eq!(
            d.ci_url.as_deref(),
            Some("https://gitlab.example.com/x/-/pipelines/2")
        );

        let http = FakeHttp::new()
            .ok(BASE, r#"{"id": 12, "default_branch": null}"#, &[])
            .ok(BASE, "[]", &[("x-total", "0")]);
        let d = details(&http, BASE, "glpat", "example-user", "notes", 0).unwrap();
        assert_eq!((d.open_pull_requests, d.ci), (Some(0), None));
    }
}
