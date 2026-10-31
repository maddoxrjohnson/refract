# 11. Which Claude Code events set which status

Status: accepted (M6)

**Context.** The gem should show when an agent is working and when it is waiting for you, using only what Claude Code reports through hooks. Some events are noisy or misleading.

**Decision.** Seven events are used: `SessionStart` (startup, resume, clear) and `Stop` mean finished, `UserPromptSubmit` and `PostToolUse` mean working, `Notification` limited to `permission_prompt` and `elicitation_dialog` means waiting on you, `PreToolUse` limited to `AskUserQuestion` and `ExitPlanMode` means waiting on you too, and `SessionEnd` means ended. `PostToolUse` is what flips a turn back to working once a permission prompt or a question is answered. A multiple-choice question and a plan awaiting approval send no `Notification`, which is why their `PreToolUse` was added (an install from before then shows as needing an update). `Notification`'s `idle_prompt`, sent after a minute at the prompt, would wrongly say the agent is blocked, so it is excluded. `SessionStart` leaves out `compact` because that happens mid-turn.

**Consequences.** A status counts only for a session with a running `claude` process, and only while the hook is installed, so stale files cannot colour a gem. Pressing Esc or denying a permission ends a turn without a `Stop`, so a live session whose transcript ends in a user interrupt after the status is shown as idle.
