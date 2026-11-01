# Decision records

Short notes on choices that were weighed and are easy to undo by accident. Each says what the problem was, what was decided and what it costs. They were written after the fact for the work in milestones M3 to M7; new ones go at the end with the next number.

Format: a title, a status line, then **Context**, **Decision** and **Consequences**. Keep them to a screen. When a decision is replaced, leave the old record in place, change its status to `Superseded by NNNN` and link the new one.

| # | Decision | Milestone |
|---|---|---|
| 0001 | [Nothing runs while the window is hidden](0001-nothing-runs-while-hidden.md) | M3, M4, M6 |
| 0002 | [Draw on change, paced by a ticker](0002-draw-on-change.md) | M4 |
| 0003 | [Two-pass, screen-space glass](0003-two-pass-screen-space-refraction.md) | M4 |
| 0004 | [Pick the graphics level from frame intervals and GPU counters](0004-adaptive-quality-from-frames-and-gpu.md) | M4, polish |
| 0005 | [API requests do not follow redirects](0005-api-requests-do-not-follow-redirects.md) | M5 |
| 0006 | [Tokens live in Windows Credential Manager and never reach the interface](0006-tokens-in-credential-manager.md) | M5 |
| 0007 | [Remote data is cached for fifteen minutes and a failure keeps the last good answer](0007-remote-data-cached-fifteen-minutes.md) | M5 |
| 0008 | [Clone inside WSL, with git's own credentials](0008-clone-in-the-helper.md) | M5 |
| 0009 | [The status hook command always exits 0](0009-hook-command-never-blocks-claude-code.md) | M6 |
| 0010 | [Install backs up settings.json; uninstall restores it byte for byte](0010-hook-backup-and-byte-identical-restore.md) | M6 |
| 0011 | [Which Claude Code events set which status](0011-hook-events-and-statuses.md) | M6 |
| 0012 | [Usage totals come from Claude Code's local transcripts](0012-usage-totals-from-local-transcripts.md) | M6 |
| 0013 | [The web demo runs the real interface over invented data](0013-demo-backend-and-simulated-launch.md) | M7 |
| 0014 | [The app bundles the helper and installs it into WSL by version match](0014-bundled-helper-installed-by-version.md) | M7 |
| 0015 | [CI and release are read-only by default and pin what they run](0015-ci-and-release-hardening.md) | M7 |
| 0016 | [Go to the running agent, found by its window's folder and process ancestry](0016-go-to-the-running-agent.md) | Post-M7 |
| 0017 | [The Banknote scene replaces Frozen shatter](0017-banknote-scene.md) | Post-M7 |
| 0018 | [A refactor runs in its own worktree, in plan mode, and merges only on request](0018-refactor-in-a-worktree.md) | Post-M7 |

See also [architecture.md](../architecture.md) for how the pieces fit together.
