# 10. Install backs up settings.json; uninstall restores it byte for byte

Status: accepted (M6)

**Context.** The hook installer edits a file that belongs to Claude Code and to the user: it may hold their own hooks and a formatting they care about. The spec promises that removing the hook leaves settings exactly as they were.

**Decision.** Install and uninstall are pure functions over the file's bytes. Before the first change the helper saves the original bytes (or a marker if there was no file) and never overwrites that backup. On uninstall, if what remains after removing Refract's entries equals the backup as JSON (ignoring event lists left empty), the backup's bytes are written back unchanged; otherwise only Refract's entries are removed and the file is written in Claude Code's format. Files are written next to the old one and renamed, keeping their mode; a symlinked file has its target edited; if the file changes while Refract works, the edit is redone once and a second change is an error.

**Consequences.** The common case, install then uninstall, is exactly reversible, and the uncommon one never discards the user's later edits. A `hooks` value that is not an object of lists is refused rather than rewritten, and an unreadable `settings.json` is never touched.
