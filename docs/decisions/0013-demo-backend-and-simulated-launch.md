# 13. The web demo runs the real interface over invented data

Status: accepted (M7)

**Context.** A public demo should be the actual interface, so it cannot drift from the app, but it cannot start VS Code or read anyone's repos.

**Decision.** The interface only knows the `Backend` interface. `DemoBackend` implements all of it with invented repos, pull requests, hook diffs and drifting numbers, and `main.tsx` picks it in a `--mode demo` build or whenever the page is not inside Tauri. A launch walks through its progress steps and then a pretend terminal types the `claude` command and an invented reply. Links to GitHub open Refract's own page rather than the invented owners. The demo is built with `npm run build:demo` and deployed to GitHub Pages after CI passes on `main`.

**Consequences.** Every `Backend` change has to be mirrored in the demo, which the type checker enforces. Playwright tests run the real interface against the demo build. No private repository name appears in the demo, screenshots or fixtures.
