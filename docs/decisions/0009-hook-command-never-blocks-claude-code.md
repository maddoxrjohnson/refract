# 9. The status hook command always exits 0

Status: accepted (M6)

**Context.** Claude Code treats a hook exiting with code 2 as an instruction to block the prompt or the stop. `sh` exits 2 when the script file it was told to run does not exist, which happens if someone deletes `~/.local/share/refract` while the hook is still in `settings.json`. A status light must never be able to stop real work.

**Decision.** The installed command is `[ -f '<script>' ] && sh '<script>' <status> '<dir>' || cat >/dev/null; exit 0`. If the script is missing, the command drains its input and exits 0. The script itself reads a bounded amount of input, prints nothing, ignores input it does not understand, and ends in `exit 0`. Writing a status file goes through a temporary file and a rename.

**Consequences.** A broken hook is silent: statuses stop updating and the gem shows no working or waiting state, but Claude Code is unaffected. Because the command is built from the script's path, `refract-core` recognises Refract's entries by that path when it adds, replaces or removes them.
