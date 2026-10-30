# 5. API requests do not follow redirects

Status: accepted (M5)

**Context.** Every request to GitHub or GitLab carries a bearer token. An HTTP client that follows redirects can be led to another host, which is a way for a token to leak, and a GitLab address typed with a wrong host or an old name would quietly work through a redirect.

**Decision.** The HTTP layer (`remote/http.rs`) is built with zero redirects allowed, HTTPS only. A 3xx comes back as an ordinary answer, and for GitLab it becomes the message "the address moved to …; update it in Refract's settings". Pagination follows the `Link` header's next page only when it is on `api.github.com`. GitLab addresses are stored as `https://host[:port]` with no user name, query or fragment.

**Consequences.** A GitLab instance that moves shows an error until the person fixes the address. That is deliberate: an error that says what to change is better than a token sent somewhere new.
