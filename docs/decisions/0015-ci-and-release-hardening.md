# 15. CI and release are read-only by default and pin what they run

Status: accepted (M7)

**Context.** The repository is public, builds an installer people will run, and its pipelines handle a release token.

**Decision.** Workflows start with `contents: read`; only the final publish job, which runs nothing from the build, can write. Checkouts do not keep credentials. Third-party actions are pinned to commit hashes. The secret scan downloads gitleaks pinned by version and checksum and scans the full history. Pages deploys only after CI has passed on a push to `main`, building the commit CI tested. The browser tests run at the Low graphics level, because software WebGL on a small runner cannot keep up with Medium's constant motion. The installer is unsigned, and the README says what Windows will show.

**Consequences.** Updating an action means updating a hash by hand. Windows warns about the unsigned installer until a certificate is bought (an open question in the plan).
