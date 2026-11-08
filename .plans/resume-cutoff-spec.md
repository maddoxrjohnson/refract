# Resume after /clear and after a restart

Three changes to which sessions the picker offers to resume (spec §4.3, architecture "resume
reasons"). Order of work: B, then A, then C. Each is committed on its own.

## B. A session ended by /clear is not interrupted

### Bug

Session X is interrupted (WSL shut down mid-turn). The person runs `claude --resume X`, then
`/clear` before sending a prompt. The process now runs a new session Y (the hook's pids place it
there, procs.rs step 1). X's transcript still ends mid-turn and no process runs X, so X classifies
as `Interrupted` and stays in the picker as "Resume: stopped mid-task" forever. Its `ended` status
file is pruned after 10 minutes, so even the hook's evidence that X was closed is lost.

### Rule

Claude Code's `SessionEnd` input carries `reason` (`clear`, `logout`, `prompt_input_exit`,
`other`, …). A session whose latest report says `ended` with reason `clear` was closed on purpose
by the person: it is not interrupted. Only `clear` counts; closing a terminal or a crash says
nothing about whether the person saw the cut-off turn.

- **Script** (`crates/refract-agent/src/status-hook.sh`): for the `ended` status only, read
  `"reason"` from the input the same way as `hook_event_name` (first match, letters and `_` only,
  else empty), and write it as a new last field for every status: `…,"starts":[…],"reason":"clear"`.
  Write `"reason":""` when there is none or the status is not `ended` (other events' tool input
  can hold a `reason` key of its own).
- **Status file** (`refract-core/src/hook.rs` `StatusFile`): `#[serde(default)] reason:
  Option<String>`; old files without it still parse.
- **Report** (`refract-core/src/classify.rs` `HookReport`): `pub cleared: bool`, true when the
  status is `Ended` and the reason is `clear`. `HookReport::new` sets false. It stays `Copy`.
- **Classify** (`classify_report`, not-live branch only): when the report is `cleared` and the
  transcript's state would be `Interrupted(_)`, give `Finished(FinishKind::Completed)` instead,
  but only if the report is not older than the transcript's last line: `(at + 1) * 1000 >
  last_timestamp_millis` (`at` is whole seconds). No `at` or no last timestamp: no change. A live
  session is unaffected (the process decides). No new wire value. The condition (cleared, not
  older than the last line) is `classify::ended_by_clear`.
- **Preselect** (`refract-core/src/preselect.rs` `Candidate`): `pub cleared: bool`, set in
  state.rs `candidates()` when the session is not live and `ended_by_clear`. When the newest
  conversation is cleared there is no last conversation (no session gets `askedQuestion`,
  `uncommittedChanges` or `unpushedCommits`), and as the newest stopped session with no reason it
  keeps older sessions from being preselected (decided 2026-11-08: block, don't skip). Older
  interrupted sessions are still listed.
- **Pruning** (`refract-agent/src/hook.rs` `prune_statuses`): an `ended` file whose reason is
  `clear` is kept for `CLEARED_KEEP` = 90 days (transcripts can outlive Claude Code's 30-day
  default, and the files are tiny) instead of `ENDED_GRACE`, and the 7-day stale rule does not delete it before then. Other
  `ended` files and temporary files: unchanged (10 minutes). Any other status: unchanged.
- A resumed X writes new reports into the same file, so a later interrupt of X is seen as usual.
- Installs from before this change have no reason: nothing changes for them. Changing the script
  makes Settings show "Needs update" (the installed script bytes differ), as before.

### Tests

- Script (existing script tests in `refract-agent`): SessionEnd input with `"reason":"clear"`
  writes `"reason":"clear"`; input without one writes `"reason":""`; a reason with other
  characters is dropped (empty).
- Parse: file with and without `reason`; `cleared` only for `ended` + `clear`.
- Classify: interrupted tail + cleared report at/after the last line → `Finished(Completed)`;
  cleared report older than the last line → still interrupted; `ended` with another reason →
  still interrupted; live session → unchanged.
- Prune: cleared `ended` file 1 hour, 8 days and 89 days old is kept, 91 days old is deleted; plain
  `ended` file 11 minutes old is deleted as before.
- Snapshot level (state.rs): interrupted session X with a cleared report has no resume reason and
  no `interrupted` chip; preselect does not pick it.

### Docs

architecture.md: status file format (`reason`), hook events table (`SessionEnd` row: "records
the reason"), pruning paragraph (cleared files kept 90 days), resume reasons ("a session ended by
`/clear` is not interrupted"). Spec §6.2 if it lists the file fields.

## A. "Resume: cut off by a restart"

### Problem

Two agents ran in a repo when WSL shut down. One had finished its turn and sat idle at the prompt;
its transcript ends normally, the repo is clean and pushed, so it is `Finished(Completed)` with no
resume reason and the picker does not offer it. The person expected to pick it up again.

### Rule (status hook only; decided 2026-11-08: no saved state; revised after review: boot id + init, no clock)

A session is **cut off** when all hold:
- it is not live (no process runs it), and its transcript has a conversation;
- its latest status report exists and does not say `ended` (Claude Code never ran SessionEnd, so
  the process died without closing);
- the report is under 7 days old by its `at` (`now - at < 7 d`; full reads of the status directory
  delete it after 7 days anyway; the hourly sweep does not);
- the report is from before a restart. Not by the clock: `btime` is the shared WSL VM's boot,
  which `wsl --terminate <distro>` keeps, and after a host sleep WSL steps the guest clock, so a
  freshly started helper reads a later `btime` (false cut-offs). Instead:
  - **Script** (`status-hook.sh`): appends `"boot":"<boot id>"` (`/proc/sys/kernel/random/boot_id`,
    hex and `-` only, else empty) and `"init":<ticks>` (field 22 of `/proc/1/stat` via
    `stat_fields`, else 0) after `reason`.
  - **Status file / report** (core `hook.rs`, `classify.rs`): `StatusFile.boot: Option<String>`,
    `init: Option<u64>`; `HookReport.boot: u128` (`parse_boot_id`), `init: u64`, 0 = unknown.
  - **Helper** (`procs::current_boot`, read once in `Agent::new`): a `classify::Boot` with the
    current boot id, PID 1 start ticks and `btime`.
  - **Rule** (`classify::cut_off`): a report with a boot id or init is cut off when either differs
    from the helper's (a value either side doesn't know is not compared). A report with neither
    (older script) falls back to `at < btime + init_ticks / 100`.

Without the hook, or with a report lacking `at`, nothing is cut off. Resuming the session writes a
fresh report (this boot), and closing it writes `ended`, so the reason goes away by itself.
`RepoState::refresh_preselect` recomputes the sessions' reasons as well as the preselect, so a
cut-off lapses when its report turns 7 days old.

- **Reason**: new `ResumeReason::CutOff` (wire `"cutOff"`), second in the order: interrupted >
  cutOff > askedQuestion > uncommittedChanges > unpushedCommits. Like interrupted it counts for
  **any** session, not only the repo's last conversation, and also when another session is live.
  A session that is `Interrupted` keeps `Interrupted`.
- **Where**: `preselect::Candidate` gains `cut_off: bool`; `resume_reasons` uses it.
  `refract-agent/src/state.rs` `candidates()` computes it from `hooks`, `live`, the boot time and
  `now`. The boot time comes from wherever the helper keeps process-scan context (read once;
  tests pass it in). Preselect: unchanged rule (newest stopped session with a reason, < 2 days),
  which now picks a cut-off newest session.
- **No chip**: the stone's chipped corner stays for `Interrupted` only (app `repoStatus.ts`
  `interrupted` is from state, check it does not use `resumeReason`).
- **App**: `types.ts` `ResumeReason` adds `"cutOff"`; `Picker.tsx` `RESUME_LABELS.cutOff =
  "Resume: cut off by a restart"`; DemoBackend may show one cut-off session in an invented repo.
- **Docs**: architecture.md resume reasons table + picker label list; spec §4.3 table row.

### Tests

- `resume_reasons`: cut-off older session next to a newer finished one gets `CutOff`; interrupted
  + cut_off gives `Interrupted`; cut-off while another session is live still gets `CutOff`.
- `candidates`/snapshot (state.rs): finished session with a `finished` report from another boot id
  and no process → `cutOff`, preselected Resume when newest and < 2 days; same boot id, other init
  (distro restart) → `cutOff`; same boot id and init, even with `at < btime` → none; `ended` →
  none; live → none; older than 7 days → none; no report → none; old report without boot/init →
  `btime` fallback. Empty transcript with a cut-off report → none.
- `cut_off`/`parse_boot_id` unit tests; status file parse with and without `boot`/`init`; script
  integration test: the file has `boot` (36-char uuid, this machine's) and `init` (> 0).
- Agent level: `agent.boot` set directly, a cut-off session gets `cutOff` through `store`, and
  `refresh_preselects` drops it once the report is a week old.
- Vitest: picker label for `cutOff`; chip not shown for a cut-off finished session.

## C. Panel hint (outline)

The detail panel's "Last agent session" shows only the newest session. When another session has a
resume reason, add "N more to resume".
