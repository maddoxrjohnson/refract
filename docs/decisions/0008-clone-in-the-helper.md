# 8. Clone inside WSL, with git's own credentials

Status: accepted (M5)

**Context.** Clones belong in the Linux filesystem, next to the other repos, where the helper and Claude Code run. Passing Refract's token to git would put it on a command line or in a URL.

**Decision.** `cloneRepo` runs `git clone` in the helper, into a hidden sibling of `~/projects/<name>`, and renames it into place when it succeeds. The URL must be `https://` with no user name, and the name a plain folder name. git never prompts, and Refract does not give it a token, so a private repo clones only if git in WSL has credentials for it. A clone has ten minutes, after which git and its children are stopped as a group. Clones run one at a time. A folder that is already a clone of the same URL is returned as it is.

**Consequences.** A failed or cancelled clone never leaves a half-written folder, and retrying after a failed launch is safe. Private repos need credentials that git in WSL can use on its own, and a failure shows git's own message.
