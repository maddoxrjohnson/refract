# Codex as an overflow agent (spec)

Status: interview done 2026-11-08. §3 checked against the installed CLI on 2026-11-08 (model mapping waits on the
account's plan, see §3).

## 1. Goal
Codex CLI runs next to Claude Code as an **overflow** agent. When Claude's 5-hour or weekly limit runs out, Codex picks
up the same repo handoff and continues the same "Next" item. When Claude has quota again, Claude takes over. Refract:
- shows whether a repo is ready for Codex,
- launches and resumes Codex the way it does Claude,
- briefs whichever agent picks up from the other.

Not in scope: deciding which code may go to which provider. Every local repo is treated the same.

## 2. Decisions

| Topic | Decision |
|---|---|
| Instruction source | `AGENTS.md` holds the shared rules. `CLAUDE.md` (or `CLAUDE.local.md`) is `@AGENTS.md` plus a "Claude only" section |
| What makes a repo not ready | No or unsynced AGENTS.md; Claude-only content in the instructions or handoff; the other agent is busy |
| Strictness | Ready / Ready with warnings / Not ready / Busy. Only Not ready and Busy block a launch |
| Where it shows | Detail panel only: a status line, findings (`file:line`), and a Make Codex-ready button |
| Fix-up | The button launches an agent with a fixed prompt (like Refactor). It runs as Claude, or as Codex when Claude is over the threshold. Refract re-checks through the repo watch |
| Git tracking | The fix-up never edits `.gitignore` and never commits. A new AGENTS.md is tracked only when CLAUDE.md is |
| Launch flags | `--sandbox workspace-write --ask-for-approval on-request`. The model and effort come from the user's Codex config, not Refract |
| Picker | One merged list: "New Claude agent", "New Codex agent", then sessions of both agents newest first, each tagged |
| Pick-up | A briefed launch: the new agent is told to follow the handoff, and given the other agent's last message and state, plus "check git status/diff first" |
| Hand-back | If the other agent's session is newer than this agent's newest session, preselect a briefed **new** agent instead of the stale resume |
| Nudge | Claude 5-hour or weekly use ≥ `codexThreshold` (default 95%, set in Settings) and repo Ready or with warnings → preselect "New Codex agent" with the reason |
| Busy rule | Blocked only while the other agent is working or waiting on the user. Open and idle → offer "Stop the idle <agent> and start <other>" |
| End of a Codex milestone | (A convention in the instructions, not Refract code) Codex commits on a branch and writes `Needs review: <branch>` in the handoff. Claude reviews, merges and pushes |

## 3. Codex facts as checked
Checked 2026-11-08 against codex-cli 0.167.0 (npm package, Linux/WSL), with short throwaway sessions in a scratch repo.

| Fact | Checked |
|---|---|
| Binary | `codex` is a Node script that `execve`s the bundled native binary. The live process is the native binary: `argv[0]` is a full path ending in `/codex`, the user's args follow unchanged. No Node process stays |
| Finding it | Installed under nvm, so it is on `PATH` only in **interactive** shells (`bash -ic`); `bash -lc` and plain `PATH` don't find it. VS Code terminals are interactive, so launches work. The helper's probe must use `bash -ic 'command -v codex'` or check the nvm/npm bin dirs |
| New with prompt | `codex [OPTIONS] [PROMPT]`. No way to choose the session id at start |
| Resume | `codex resume [SESSION_ID] [PROMPT]` (also `--last`). Top-level options before the subcommand work (`codex --no-daemon resume <id>` checked) |
| Sandbox / approval | `-s/--sandbox read-only\|workspace-write\|danger-full-access`; `-a/--ask-for-approval on-request\|never` |
| Shared daemon | By default the TUI starts (or joins) a shared `codex app-server … --managed-daemon` process, reparented to init and kept alive after the TUI quits. **The daemon, not the TUI, holds the rollout file and the thread lock.** With `--no-daemon` the TUI process holds both itself |
| Session id at start | `~/.codex/thread-writer-locks/<id>.lock` is created and held open as soon as the TUI is up, before the first message. The file is removed on quit. With `--no-daemon`, `/proc/<pid>/fd` of the TUI gives the id at once |
| Rollout file | `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<local time YYYY-MM-DDTHH-MM-SS>-<uuidv7>.jsonl`, plain JSONL (no `.zst`). Created on the **first message**, not at start. Resume appends to the same file. Held open while the session is open (by the daemon or the `--no-daemon` TUI). There is also a private SQLite index (`state_5.sqlite`); not used |
| Line shape | `{timestamp, ordinal, type, payload}`. Types: `session_meta`, `event_msg`, `response_item`, `turn_context`, `world_state`, `token_usage_record` |
| `session_meta` | `payload.id` (= `session_id`), `timestamp`, `cwd`, `originator` (`codex-tui` or `codex_exec`), `source` (`cli` or `exec`), `cli_version`, `git.branch`. Exec sessions are hidden from Codex's own resume picker by default; Refract should hide them too |
| End markers | Turn start `event_msg/task_started`. Finished turn `event_msg/task_complete` with `last_agent_message`. Interrupt (Esc) `event_msg/turn_aborted` with `reason: "interrupted"`. Quitting writes nothing. So: last turn marker `task_started` with no later complete/abort, and not live = cut off |
| Last assistant text | `task_complete.last_agent_message`, or the last `response_item` `message` with `role: "assistant"` |
| Rate limits | `event_msg/token_count.rate_limits`: `limit_id`, `primary {used_percent, window_minutes, resets_at (unix s)}`, `secondary` (nullable), `credits`, `plan_type`, `rate_limit_reached_type` (null unless hit; value when hit not seen). **This account reports `plan_type: "free"`, primary window 43200 min, secondary null** — not the Plus windows the research expected |
| Models (this account) | `gpt-6-luna` (default; "Fast and affordable model for easier tasks"), `gpt-5.6-terra` ("Older balanced"), `gpt-5.6-luna` ("Older fast and efficient"); hidden `gpt-reserve`, `codex-auto-review`. Efforts low/medium/high/xhigh/max. The list is per plan (`~/.codex/models_cache.json`), so recheck after Plus |
| Folder trust | First launch in a folder shows "Trust this folder?" and saves `[projects."<path>"] trust_level = "trusted"` in `~/.codex/config.toml` |
| Terminal status | The TUI writes `OSC 7501;state=working\|idle\|done:app=codex` and sets the terminal title (spinner + task). A possible C2 status source besides hooks |
| Instruction file | `$CODEX_HOME/AGENTS.md` (global) and the repo's `AGENTS.md` both load (checked with a temp `CODEX_HOME`). `@path` lines are **not** followed. Binary also has `AGENTS.override.md`, `project_doc_max_bytes`, `project_doc_fallback_filenames`; 32768 appears as a constant (size limit not checked live) |
| Skills | `.agents/skills` present in the binary; built-in skills live in `~/.codex/skills/.system` |
| Agent roles | `$CODEX_HOME/agents/*.toml` files are discovered on their own, with `name`, `description`, `model`, `model_reasoning_effort`, `developer_instructions` (checked: the role and its model show up in the spawn tool). `[agents.<name>] description + config_file` in config.toml also works. `max_concurrent_threads_per_session` sits under `features.multi_agent_v2` (off by default; `multi_agent` is on) |
| Hooks | `hooks.json`; event names in the binary: SessionStart, SessionEnd, UserPromptSubmit, PreToolUse, PostToolUse, PermissionRequest, PreCompact, Stop, SubagentStop, TurnStart. Hooks need persisted trust. Payload fields not checked (C2) |

Consequences for C1:
- Refract launches Codex with `--no-daemon`, so the process it started holds the lock and rollout and its pid maps to a
  session id from `/proc/<pid>/fd`. New: `codex --no-daemon --sandbox workspace-write --ask-for-approval on-request
  [PROMPT]`. Resume: `codex --no-daemon --sandbox workspace-write --ask-for-approval on-request resume <id>` (flag
  order to confirm live).
- Codex started outside Refract (daemon mode): the TUI pid holds nothing. Live = a `thread-writer-locks/<id>.lock`
  file held open by some process (the daemon); session id from the lock name, cwd from the rollout or the TUI's cwd.
- `procs.rs` must ignore `codex app-server …`, `codex exec …`, `codex-code-mode-host` and `codex-linux-sandbox`.
- A Codex session with a lock but no rollout yet is a live session that has not had its first message.
- `codex --version` probe goes through `bash -ic` (or known bin dirs), not `bash -lc`.

## 4. Readiness rules
Pure function in `refract-core` (`codex_ready.rs`). It takes the repo's instruction files and live-session facts and
returns `{ state, findings[] }`. Each finding is `{ file, line, kind, text }`.

**Not ready** (blocks the launch) when any of these is true:
- there is no non-empty `AGENTS.md` at the repo root;
- `CLAUDE.md` or `CLAUDE.local.md` exists, has no `@AGENTS.md` line, and is not byte-equal to AGENTS.md;
- AGENTS.md itself relies on `@path` imports;
- AGENTS.md plus nested AGENTS.md files on the way to the root are over 32 KiB (or the checked limit).

**Warnings** (the launch still works). A line-based scan covers AGENTS.md and the notes files it names by relative path
(for example a handoff or progress file). It looks for:
- `AskUserQuestion`, `ExitPlanMode`, `plan mode`;
- `subagent_type`, `model: opus|sonnet|haiku`;
- `~/.claude/`, `/clear`, `/compact`;
- `.claude/skills`, `Skill tool`.

The term list is a constant with tests. It should match a Claude-only instruction, not a mention: a line saying
"`/clear` in Claude, `/new` in Codex" names both and should not warn.

**Busy** (blocks): the other agent has a live session that is working or waiting on the user.

**Codex not installed**: `codex --version` fails in WSL. Codex rows are hidden and the panel says "Codex not
installed".

## 5. State table (New Codex agent row; mirror for New Claude agent)

| Repo / other agent | Row | Preselected? |
|---|---|---|
| Codex not installed | hidden | no |
| Not ready | dimmed, "Not ready: <first finding>" | no |
| Claude working or waiting on you | dimmed, "Busy: Claude is working" | no |
| Claude open and idle | "Stop the idle Claude agent and start Codex" | if nudge applies |
| Ready or warnings, Claude ≥ threshold | "New Codex agent" (briefed if Claude's newest session is unfinished) | yes, reason "Claude at N%" |
| Ready or warnings, Codex session newer than Claude's newest | (on the Claude side) "New Claude agent, briefed" | yes, over the stale Claude resume |
| Ready or warnings, otherwise | "New Codex agent" | no (today's rules) |

A live Codex session that is focusable gives "Go to the running Codex agent", through the existing `REFRACT_FOLDER`
focus path.

## 6. Protocol (serde defaults, no version bump)
- `AgentKind = claude | codex`, defaulting to claude.
- `LaunchMode::New { agent, prompt, planMode, brief? }` and `LaunchMode::Resume { agent, id }`.
- `SessionInfo.agent`; live process `agent`.
- `RepoSnapshot.codex?: { state, findings[] }`; helper capability `codexInstalled`.
- Mirrors: `refract-client` types and `app/src/backend/types.ts`.

## 7. Helper
- `launch_argv` branches on the agent. The extension stays generic (`argv[0]` is any plain program name).
- Codex session source:
  - path: `$CODEX_HOME` or `~/.codex/sessions`;
  - rollouts indexed by `cwd`, with an mtime/size head and tail cache;
  - inotify watch.
- A new session's id is learned after start. First choice is the rollout the process holds open (if checked). Otherwise
  it is the newest rollout for that cwd created after the launch time.
- `procs.rs` accepts `codex` command lines. The session id comes from `resume <id>` or the fd match. `ClaudeProcess` is
  renamed `AgentProcess`.
- Resume reasons apply per session, whatever the agent. Preselect adds hand-back and nudge (§5). The nudge reads the
  Claude limits the helper already loads.
- "Stop the idle agent" reuses the Refactor stop path. It refuses while background work runs.

## 8. Brief prompt (`brief.rs`)
The brief contains:
- "Follow AGENTS.md and the handoff it names";
- the other agent's name and how its session ended (cut off / rate-limited / interrupted / finished);
- its last assistant message, capped at about 2 KB and cut on a character boundary;
- "Run git status and git diff before continuing";
- for a hand-back to Claude, any `Needs review: <branch>` lines from the handoff.

It is passed after `--` for Claude and as the positional prompt for Codex.

## 9. Make Codex-ready prompt
The agent:
1. Moves the rules that apply to any agent into AGENTS.md.
2. Leaves CLAUDE.md (or CLAUDE.local.md, whichever exists) as `@AGENTS.md` plus a "Claude only" section.
3. Rewords Claude-only steps in the handoff neutrally, keeping their meaning (for example "start a fresh session:
   `/clear` in Claude, `/new` in Codex").
4. Gives AGENTS.md the same git tracking as CLAUDE.md.
5. Does not edit `.gitignore` and does not commit.
6. Ends with a list of what moved where.

## 10. UI
- Picker: two New rows, `[Claude]` / `[Codex]` tags, dimmed rows with reasons. `optionKey` includes the agent.
- Detail panel:
  - "Codex: Ready", "Codex: N warnings", "Codex: Not ready" or "Codex: Busy (Claude working)";
  - an expandable list of findings;
  - a Make Codex-ready button.
- Settings: `codexThreshold` (a percentage, 50–100).
- Demo: invented repos in each state; DemoTerminal shows a fake codex command line.

## 11. Phases
- **C1**: everything above.
- **C2**: Codex status hook (working/waiting/finished stones for Codex), opt-in with a byte-identical restore like the
  Claude hook.
- **C3**: Codex limits in the usage widget, from rollout `rate_limits`. The nudge can then also steer back to Claude when
  Codex is near its limit.
- **Maybe**: a panel line "Codex branch awaiting review".

## 12. Verification
- Rust:
  - `codex_ready` tests, covering each state, and each term with its line, including the both-names case;
  - Codex `launch_argv`;
  - rollout parsing from invented fixtures;
  - preselect hand-back and nudge;
  - the brief cap.
- App: Vitest for the picker options and the panel line; an e2e demo test that picks New Codex agent and sees a dimmed
  row with its reason.
- Live, in a scratch repo: Not ready → Make Codex-ready → Ready; New Codex agent opens codex; the session appears tagged
  and Resume works; Busy and stop-idle in both directions; a 1% threshold preselects Codex.
