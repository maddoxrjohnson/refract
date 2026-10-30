# 14. The app bundles the helper and installs it into WSL by version match

Status: accepted (M7)

**Context.** The helper must be inside WSL, but a person installing Refract should not have to build or copy a Linux binary.

**Decision.** The installer bundles a static musl build of `refract-agent`. When it starts the helper, the app runs `refract-agent --version` in the chosen distro and, unless it prints the app's own version, copies the bundled file to `~/.local/share/refract/bin/` through a temporary file and a rename. The release workflow checks that the app, the helper and `tauri.conf.json` versions agree and match the tag. `REFRACT_AGENT` bypasses all of this for development, and an empty bundled file (a normal developer build) means "use what is installed".

**Consequences.** Every release bumps three versions together. An old helper is replaced as soon as a new app starts, so app and helper never disagree about the protocol. A failed copy leaves the old helper in place and shows the reason in the helper's status.
