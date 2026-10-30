//! A small blocking HTTP layer for the GitHub and GitLab clients. Requests and
//! responses are plain data, so the clients are tested against a fake [`Http`].

use std::fmt;
use std::time::Duration;

use serde_json::Value;

/// How long one request gets, from connecting to the end of the body.
pub const TIMEOUT: Duration = Duration::from_secs(15);
const USER_AGENT: &str = concat!("refract/", env!("CARGO_PKG_VERSION"));
/// The largest body read; a page of 100 repos is well under 1 MiB.
const MAX_BODY: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// One request. Its `Debug` output leaves out the headers, which carry the token.
#[derive(Clone, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// A JSON body, for `Post`.
    pub body: Option<String>,
}

impl Request {
    pub fn get(url: impl Into<String>) -> Request {
        Request {
            method: Method::Get,
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn post_json(url: impl Into<String>, body: String) -> Request {
        Request {
            method: Method::Post,
            url: url.into(),
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: Some(body),
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Request {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    /// The value of header `name`, ignoring case.
    #[cfg(test)]
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

/// A response of any status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Response {
    /// The value of header `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Sends requests. The error is a transport failure, worded for a person; any status
/// is a [`Response`].
pub trait Http {
    fn send(&self, request: &Request) -> Result<Response, String>;
}

/// [`Http`] over `ureq`, with the system's TLS and root certificates.
pub struct Client {
    agent: ureq::Agent,
}

impl Client {
    pub fn new() -> Client {
        Client {
            agent: agent_config().new_agent(),
        }
    }
}

/// The agent's settings. Redirects are not followed, so a token is never sent on to
/// another host; a 3xx comes back as a [`Response`].
fn agent_config() -> ureq::config::Config {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    let tls = TlsConfig::builder()
        .provider(TlsProvider::NativeTls)
        .root_certs(RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .https_only(true)
        .http_status_as_error(false)
        .max_redirects(0)
        .user_agent(USER_AGENT)
        .timeout_global(Some(TIMEOUT))
        .build()
}

impl Http for Client {
    fn send(&self, request: &Request) -> Result<Response, String> {
        let host = url_host(&request.url).unwrap_or("the server").to_owned();
        let failed = |err: ureq::Error| match err {
            ureq::Error::Timeout(_) => format!(
                "{host} did not answer within {} seconds.",
                TIMEOUT.as_secs()
            ),
            other => format!("Cannot reach {host}: {other}"),
        };
        let result = match request.method {
            Method::Get => {
                let mut builder = self.agent.get(&request.url);
                for (name, value) in &request.headers {
                    builder = builder.header(name, value);
                }
                builder.call()
            }
            Method::Post => {
                let mut builder = self.agent.post(&request.url);
                for (name, value) in &request.headers {
                    builder = builder.header(name, value);
                }
                builder.send(request.body.as_deref().unwrap_or(""))
            }
        };
        let response = result.map_err(failed)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
            })
            .collect();
        let body = response
            .into_body()
            .with_config()
            .limit(MAX_BODY)
            .read_to_string()
            .map_err(failed)?;
        Ok(Response {
            status,
            headers,
            body,
        })
    }
}

/// The host of an `https://` URL, without user info or port.
pub fn url_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

/// Percent-encodes everything but the unreserved characters, for a path segment or
/// query value.
pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A failed response, worded for a person. `service` is "GitHub" or "GitLab";
/// `auth_hint` says what to do about a refused token. Rate limits name the wait,
/// from `Retry-After` or the reset time against `now_unix`.
pub fn status_message(
    service: &str,
    auth_hint: &str,
    response: &Response,
    now_unix: u64,
) -> String {
    let remaining = response
        .header("x-ratelimit-remaining")
        .or(response.header("ratelimit-remaining"));
    let limited = response.status == 429
        || (response.status == 403
            && (remaining == Some("0") || response.header("retry-after").is_some()));
    if limited {
        let wait = response
            .header("retry-after")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .or_else(|| {
                response
                    .header("x-ratelimit-reset")
                    .or(response.header("ratelimit-reset"))
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .map(|reset| reset.saturating_sub(now_unix))
            });
        return match wait {
            Some(secs) => format!(
                "{service}'s rate limit is used up; try again in {}.",
                minutes(secs)
            ),
            None => format!("{service}'s rate limit is used up; try again later."),
        };
    }
    if response.status == 401 {
        return format!("{service} refused the token (401 Unauthorized). {auth_hint}");
    }
    match body_message(&response.body) {
        Some(message) => format!("{service} answered {}: {message}", response.status),
        None => format!("{service} answered {}.", response.status),
    }
}

/// "1 minute", "12 minutes": `secs` rounded up to whole minutes.
fn minutes(secs: u64) -> String {
    match secs.div_ceil(60).max(1) {
        1 => "1 minute".into(),
        n => format!("{n} minutes"),
    }
}

/// The error text in a JSON error body (`message` or `error`), else the body's first
/// line, cut to 200 characters.
fn body_message(body: &str) -> Option<String> {
    let from_json = serde_json::from_str::<Value>(body).ok().and_then(|value| {
        ["message", "error"]
            .iter()
            .find_map(|key| match value.get(key)? {
                Value::String(s) => Some(s.clone()),
                Value::Null => None,
                other => Some(other.to_string()),
            })
    });
    let text = from_json.unwrap_or_else(|| body.lines().next().unwrap_or("").to_owned());
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut cut: String = text.chars().take(200).collect();
    if cut.len() < text.len() {
        cut.push('…');
    }
    Some(cut)
}

#[cfg(test)]
pub mod fake {
    //! An [`Http`] that answers from a list and records what it was sent.

    use super::*;
    use std::cell::RefCell;

    pub struct FakeHttp {
        /// URL prefix and answer; the first match is used, and used up.
        answers: RefCell<Vec<(String, Result<Response, String>)>>,
        pub sent: RefCell<Vec<Request>>,
    }

    impl FakeHttp {
        pub fn new() -> FakeHttp {
            FakeHttp {
                answers: RefCell::new(Vec::new()),
                sent: RefCell::new(Vec::new()),
            }
        }

        pub fn answer(self, url_prefix: &str, response: Result<Response, String>) -> FakeHttp {
            self.answers
                .borrow_mut()
                .push((url_prefix.to_owned(), response));
            self
        }

        pub fn ok(self, url_prefix: &str, body: &str, headers: &[(&str, &str)]) -> FakeHttp {
            self.answer(url_prefix, Ok(response(200, body, headers)))
        }

        pub fn urls(&self) -> Vec<String> {
            self.sent.borrow().iter().map(|r| r.url.clone()).collect()
        }
    }

    pub fn response(status: u16, body: &str, headers: &[(&str, &str)]) -> Response {
        Response {
            status,
            headers: headers
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect(),
            body: body.into(),
        }
    }

    impl Http for FakeHttp {
        fn send(&self, request: &Request) -> Result<Response, String> {
            self.sent.borrow_mut().push(request.clone());
            let mut answers = self.answers.borrow_mut();
            let index = answers
                .iter()
                .position(|(prefix, _)| request.url.starts_with(prefix.as_str()))
                .unwrap_or_else(|| panic!("unexpected request to {}", request.url));
            answers.remove(index).1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::response;
    use super::*;

    #[test]
    fn redirects_are_not_followed() {
        let config = agent_config();
        assert_eq!(config.max_redirects(), 0);
        assert!(config.https_only() && !config.http_status_as_error());
    }

    #[test]
    fn hosts_come_from_https_urls() {
        assert_eq!(
            url_host("https://api.github.com/user/repos"),
            Some("api.github.com")
        );
        assert_eq!(
            url_host("https://u@gitlab.example.com:8443/x"),
            Some("gitlab.example.com")
        );
        assert_eq!(url_host("https://example.com?x"), Some("example.com"));
        assert_eq!(url_host("http://example.com"), None);
        assert_eq!(url_host("https:///x"), None);
    }

    #[test]
    fn encodes_path_segments() {
        assert_eq!(
            encode("team/sub group/tide-pool_1.x~"),
            "team%2Fsub%20group%2Ftide-pool_1.x~"
        );
        assert_eq!(encode("caf\u{e9}"), "caf%C3%A9");
    }

    #[test]
    fn request_debug_leaves_out_headers() {
        let request =
            Request::get("https://api.example.com/x").header("Authorization", "Bearer secret");
        let debug = format!("{request:?}");
        assert!(!debug.contains("secret"), "{debug}");
        assert!(debug.contains("api.example.com"));
        assert_eq!(request.header_value("authorization"), Some("Bearer secret"));
    }

    #[test]
    fn refused_tokens_say_what_to_do() {
        let message = status_message(
            "GitHub",
            "Run `gh auth login` again.",
            &response(401, r#"{"message":"Bad credentials"}"#, &[]),
            0,
        );
        assert_eq!(
            message,
            "GitHub refused the token (401 Unauthorized). Run `gh auth login` again."
        );
    }

    #[test]
    fn rate_limits_name_the_wait() {
        let github = response(
            403,
            r#"{"message":"API rate limit exceeded for user ID 1."}"#,
            &[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", "1000600"),
            ],
        );
        assert_eq!(
            status_message("GitHub", "", &github, 1_000_000),
            "GitHub's rate limit is used up; try again in 10 minutes."
        );
        let gitlab = response(429, "Retry later", &[("retry-after", "30")]);
        assert_eq!(
            status_message("GitLab", "", &gitlab, 0),
            "GitLab's rate limit is used up; try again in 1 minute."
        );
        let secondary = response(403, "{}", &[("retry-after", "120")]);
        assert_eq!(
            status_message("GitHub", "", &secondary, 0),
            "GitHub's rate limit is used up; try again in 2 minutes."
        );
        assert_eq!(
            status_message("GitLab", "", &response(429, "", &[]), 0),
            "GitLab's rate limit is used up; try again later."
        );
    }

    #[test]
    fn other_failures_quote_the_server() {
        let forbidden = response(
            403,
            r#"{"message":"Resource not accessible by integration"}"#,
            &[("x-ratelimit-remaining", "4000")],
        );
        assert_eq!(
            status_message("GitHub", "", &forbidden, 0),
            "GitHub answered 403: Resource not accessible by integration"
        );
        let gitlab = response(404, r#"{"message":"404 Project Not Found"}"#, &[]);
        assert_eq!(
            status_message("GitLab", "", &gitlab, 0),
            "GitLab answered 404: 404 Project Not Found"
        );
        let nested = response(400, r#"{"message":{"ref":["is invalid"]}}"#, &[]);
        assert_eq!(
            status_message("GitLab", "", &nested, 0),
            r#"GitLab answered 400: {"ref":["is invalid"]}"#
        );
        let html = response(502, "<html>Bad gateway</html>\n<body>", &[]);
        assert_eq!(
            status_message("GitLab", "", &html, 0),
            "GitLab answered 502: <html>Bad gateway</html>"
        );
        assert_eq!(
            status_message("GitHub", "", &response(500, "", &[]), 0),
            "GitHub answered 500."
        );
        let long = "x".repeat(300);
        let message = status_message("GitHub", "", &response(500, &long, &[]), 0);
        assert!(message.ends_with("x…") && message.len() < 230, "{message}");
    }
}
