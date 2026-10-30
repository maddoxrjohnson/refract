# 6. Tokens live in Windows Credential Manager and never reach the interface

Status: accepted (M5)

**Context.** Refract needs a GitHub and a GitLab token. A token in a settings file is easy to leak into a backup, a screenshot or a bug report, and a token held by the web interface is exposed to anything that runs in it.

**Decision.** Stored tokens are generic Windows credentials named `refract:github` and `refract:gitlab`. A token pasted into settings goes from an uncontrolled input straight to Rust and is never held in React state. The interface learns only where a token comes from (GitHub CLI login, stored, or missing). For GitHub the order is the stored token, then `gh.exe auth token` on Windows, then `gh auth token` in the helper's WSL distro, accepting only strings that look like GitHub tokens. GitLab uses the stored token only. Tokens are not logged and are left out of the `Debug` output of requests.

**Consequences.** Most people with the GitHub CLI paste nothing. Reading the CLI's token runs a process, which is why results are cached. Changing a token drops the cache for that provider. The cost of Credential Manager is that it is Windows-only, which matches the app.
