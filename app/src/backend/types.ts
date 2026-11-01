// Mirrors of the Rust/serde shapes the interface reads. Field names follow the wire
// format (camelCase); see crates/refract-core/src/protocol.rs and crates/refract-client.

import type { ThemeSetting } from "../theme";

/** State of the WSL helper process, from refract-client's `AgentStatus`. */
export type AgentStatus =
  | { state: "starting" }
  | { state: "running" }
  | { state: "restarting"; attempt: number; retryInMs: number; lastError: string | null }
  | { state: "stopped" };

/** Why `agent_request` failed, from refract-client's `RequestError`. */
export type AgentRequestError =
  | { kind: "agent"; code: number; message: string }
  | { kind: "timeout" }
  | { kind: "disconnected" }
  | { kind: "notRunning" };

export type LiveState = "working" | "waitingOnUser" | "idle" | "unknown";
export type InterruptReason =
  | "unansweredPrompt"
  | "toolCallWithoutResult"
  | "toolResultWithoutReply"
  | "apiError";
export type FinishKind = "completed" | "userInterrupted";

export type SessionState =
  | { kind: "live"; detail: LiveState }
  | { kind: "interrupted"; detail: InterruptReason }
  | { kind: "finished"; detail: FinishKind };

export type Preselect =
  | { kind: "noPicker" }
  | { kind: "resume"; sessionId: string }
  | { kind: "newAgent" }
  /** Preselect "Go to the running agent" for this live session. */
  | { kind: "focus"; sessionId: string };

/** Why a session that isn't running is worth resuming (spec §4.3). */
export type ResumeReason = "interrupted" | "askedQuestion" | "uncommittedChanges" | "unpushedCommits";

export interface SessionInfo {
  id: string;
  state: SessionState;
  /** RFC 3339. */
  lastTimestamp: string | null;
  lastAssistantText: string | null;
  byteSize: number;
  /** Live, in a terminal of the Refract window on this repo: the picker can go to it. */
  focusable: boolean;
  /** Why it is worth resuming; null when it is running or done. */
  resumeReason: ResumeReason | null;
}

export interface RemoteId {
  host: string;
  owner: string;
  name: string;
}

export interface LastCommit {
  summary: string;
  /** Unix seconds. */
  time: number;
  /** Abbreviated commit id; missing from older helpers. */
  sha?: string | null;
}

/** Where a refactor stands (refactor spec §4). */
export type RefactorState =
  | "starting"
  | "working"
  | "needsYou"
  | "interrupted"
  | "ready"
  | "nothingToDo"
  | "mergeBlocked";

export interface RefactorCommit {
  /** Abbreviated commit id. */
  sha: string;
  summary: string;
}

export type CheckOutcome = "pass" | "fail" | "notRun";

/** One check the refactor agent ran, before its refactors and after them. */
export interface CheckResult {
  command: string;
  before: CheckOutcome;
  after: CheckOutcome;
}

export type MergeBlockReason =
  | "dirtyOverlap"
  | "conflict"
  | "targetMissing"
  | "live"
  /** The target branch is checked out, or being rebased, in another worktree. */
  | "targetBusy"
  /** The refactor worktree is off its branch, or a rebase is under way in it. */
  | "worktreeBusy"
  /** Uncommitted changes in the refactor worktree; `files` lists them. */
  | "worktreeDirty"
  /** The target no longer contains the commit the refactor started from. */
  | "targetRewound";

/** Why a merge stopped; `files` are relative to the repo. */
export interface MergeBlock {
  reason: MergeBlockReason;
  files: string[];
}

/** A repo's refactor, from refract-core's `RefactorInfo`. */
export interface RefactorInfo {
  state: RefactorState;
  /** Absolute Linux path of the worktree. */
  worktree: string;
  /** What `code --folder-uri` opens to review the worktree. */
  folderUri: string;
  /** Such as `refactor/20261031-1405`. */
  branch: string;
  /** The branch a merge moves. */
  targetBranch: string;
  /** Full id of the commit the worktree started from. */
  baseCommit: string;
  /** Empty when none is known. */
  sessionId: string;
  /** RFC 3339. */
  createdAt: string;
  /** The refactor agent's `claude` process is running. */
  live: boolean;
  /** Not running and its transcript ends mid-turn: worth resuming, whatever the state. */
  interrupted?: boolean;
  /** Oldest first. */
  commits: RefactorCommit[];
  filesChanged: number;
  insertions: number;
  deletions: number;
  /** `null` when the agent's result file is missing or not valid. */
  checks: CheckResult[] | null;
  summary: string | null;
  blocked: MergeBlock | null;
}

/** What `mergeRefactor` did. */
export type MergeOutcome =
  /** The target holds the commits and the refactor is gone; `kept` says what git would not remove. */
  | { outcome: "merged"; commits: number; kept?: string[] }
  /** Nothing changed; the refactor stays. */
  | { outcome: "blocked"; reason: MergeBlockReason; files: string[] };

/** A refactor alert for the interface's toast, sent as `refactor://notice`. */
export interface RefactorNotice {
  path: string;
  repoName: string;
  alert: "planReady" | "finished";
}

/** The helper's error code for a refused or failed refactor request. */
export const REFACTOR_FAILED = 8;
/** The helper's error code for a refused `removeWorktree`; the message names the rule. */
export const WORKTREE_REFUSED = 9;

/**
 * The git part of a linked worktree's status: the first matching row of the worktrees
 * spec §4 rows 3-10. The app puts the agent rows 1-2 on top.
 */
export type WorktreeStatus =
  | { kind: "locked" }
  | { kind: "operation"; op: GitOperation }
  /** Uncommitted changes, untracked files included where they are listed. */
  | { kind: "changes"; count: number }
  /** On a Windows drive with no changes seen, but untracked files unknown. */
  | { kind: "unchecked" }
  /** Commits only on this computer; `count` 100 means "100+". */
  | { kind: "onlyHere"; count: number }
  /** Git could not count the commits only on this computer, and HEAD is not merged. */
  | { kind: "onlyHereUnknown" }
  /** Another repo lies in the folder (a submodule, a nested worktree or repo, or a `.git` in an ignored folder). */
  | { kind: "holdsRepo" }
  /** The ignored folders hold too many folders to check them all for another repo. */
  | { kind: "tooBigToCheck" }
  | { kind: "release"; tag: string }
  | { kind: "merged"; branch: string }
  /** Every commit is on a remote, though not merged. */
  | { kind: "pushed" };

/** What a linked worktree holds that removing it could lose, from refract-core's `WorktreeInfo`. */
export interface WorktreeInfo {
  locked: boolean;
  operation: GitOperation | null;
  /** False on a Windows drive, where untracked files aren't listed. */
  untrackedChecked: boolean;
  /** Commits on HEAD not on any remote-tracking ref, capped at 100. */
  onlyHere: number;
  /** The default branch's name when HEAD is its ancestor. */
  merged: string | null;
  /** The oldest tag containing HEAD. */
  release: string | null;
  /** Top-level ignored entries relative to the worktree (directories end in `/`), at most 9. */
  ignored: string[];
  status: WorktreeStatus;
  /** Rows 8-10 and not on a Windows drive; the app also needs no live session and no refactor in it. */
  removable: boolean;
}

export interface RepoSnapshot {
  path: string;
  name: string;
  originUrl: string | null;
  remote: RemoteId | null;
  /** `null` when HEAD is detached. */
  branch: string | null;
  ahead: number | null;
  behind: number | null;
  changedFiles: number;
  dirtyPaths: string[];
  lastCommit: LastCommit | null;
  language: string | null;
  trackedBytes: number;
  commits30d: number;
  mass: number;
  sessions: SessionInfo[];
  preselect: Preselect;
  /** The repo's refactor, while it has one; missing from older helpers. */
  refactor?: RefactorInfo | null;
  /**
   * For a linked worktree: its main working tree's path, whether or not that repo is
   * known. Missing for main checkouts, worktrees of bare repos and refactor worktrees.
   */
  worktreeOf?: string;
  /** Facts for a linked worktree, set exactly when `worktreeOf` is. */
  worktree?: WorktreeInfo;
}

export interface RepoList {
  repos: RepoSnapshot[];
  /** True while the first load runs; the rest arrive as `repoChanged` events. */
  loading: boolean;
}

/** An unsolicited message from the helper. */
export type AgentEvent =
  | { event: "repoChanged"; data: RepoSnapshot }
  | { event: "repoRemoved"; data: { path: string } }
  | {
      event: "sessionChanged";
      data: { repoPath: string; session: SessionInfo; preselect: Preselect };
    }
  | { event: "loadComplete" }
  /** A Move to WSL migration moved on. */
  | { event: "migrateProgress"; data: MigrateProgress }
  /** A migration finished its checks and waits for an answer; also re-sent after a restart. */
  | { event: "migrateReady"; data: { id: string; report: MigrationReport } };

/** How to start the agent, from refract-core's `LaunchMode`. */
export type LaunchMode =
  | { kind: "new"; prompt?: string; planMode?: boolean }
  | { kind: "resume"; sessionId: string; prompt?: string }
  /** Show the VS Code terminal already running this live session; starts nothing. */
  | { kind: "focus"; sessionId: string };

/** What a launch returns, from refract-core's `LaunchTicket`. */
export interface LaunchTicket {
  requestId: string;
  sessionId: string;
  folderUri: string;
}

/** What a launch is doing, sent as `launch://progress` events. */
export type LaunchStep = "setup" | "server" | "prepare" | "opening" | "done";

/** Why the `launch` command failed; `message` is meant for a person. */
export interface LaunchError {
  kind: "vscodeNotFound" | "setup" | "agent" | "vscode" | "internal";
  message: string;
}

/** GPU use from Windows' performance counters, sent as `system://gpu` events. */
export interface GpuUsage {
  /** Busiest engine across all processes, 0 to 100. */
  totalPercent: number;
  /** Refract's own share of that engine, 0 to 100. */
  ownPercent: number;
}

// Remote repos (M5). Fetched on the Windows side from GitHub and GitLab; see
// app/src-tauri/src/remote.

export type RemoteKind = "github" | "gitlab";

/** A repo on GitHub or GitLab that the user owns, collaborates on or is a member of. */
export interface RemoteRepo {
  kind: RemoteKind;
  /** "github.com" or the GitLab host, lower case. */
  host: string;
  /** Owner login, or the GitLab namespace path (may contain `/`). */
  owner: string;
  name: string;
  webUrl: string;
  /** HTTPS clone URL. */
  cloneUrl: string;
  defaultBranch: string | null;
  language: string | null;
  sizeKb: number;
  /** RFC 3339. */
  pushedAt: string | null;
  fork: boolean;
  archived: boolean;
  private: boolean;
  description: string | null;
}

export interface RemoteSource {
  kind: RemoteKind;
  host: string;
  /** False when there is no URL or token for it, so it was not asked. */
  configured: boolean;
  /** Why listing failed, for a person; `null` when it worked or was not tried. */
  error: string | null;
}

export interface RemoteList {
  repos: RemoteRepo[];
  sources: RemoteSource[];
  /** RFC 3339. */
  fetchedAt: string;
}

export type CiStatus = "success" | "failure" | "pending" | "none";

/** Pull or merge requests and CI on the default branch; cached about 15 minutes. */
export interface RemoteDetails {
  openPullRequests: number | null;
  ci: CiStatus | null;
  ciUrl: string | null;
  /** RFC 3339. */
  fetchedAt: string;
}

/** Identifies a remote repo in requests. */
export interface RemoteRef {
  host: string;
  owner: string;
  name: string;
}

/** App settings, stored as JSON on the Windows side. Tokens are never in here. */
export interface Settings {
  /** The GitLab instance, e.g. "https://gitlab.example.com"; `null` turns GitLab off. */
  gitlabUrl: string | null;
  showForks: boolean;
  showArchived: boolean;
  /** Project keys (see `projectKey` in src/projects.ts) the user hid. */
  hidden: string[];
  /** The usage widget's optional cost estimate (spec §3.5). */
  costEstimate: CostEstimate;
  /** Light, dark, or following Windows (banknote spec §5). */
  theme: ThemeSetting;
}

/** Whether the usage widget estimates cost, and at what prices. */
export interface CostEstimate {
  enabled: boolean;
  /** Tried in order; the first row whose `match` is in the model id prices it. At most 50. */
  prices: PriceRow[];
}

/** Prices in US dollars per million tokens for models whose id holds `match`, ignoring case. */
export interface PriceRow {
  match: string;
  input: number;
  output: number;
  cacheWrite5m: number;
  cacheWrite1h: number;
  cacheRead: number;
}

/** Where each token comes from. Tokens themselves never reach the interface. */
export interface Credentials {
  github: "ghCli" | "stored" | "missing";
  gitlab: "stored" | "missing";
}

/** Why a remote, settings or clone command failed; `message` is meant for a person. */
export interface CommandError {
  kind: string;
  message: string;
}

// Status hook and widgets (M6).

/** The optional Claude Code status hook (spec §6.2), from the helper. */
export interface HookInfo {
  installed: boolean;
  /**
   * Not installed, but the settings already hold Refract's entries: an install from an
   * older Refract, or one whose script is missing or outdated. Installing updates it.
   */
  outdated: boolean;
  /** `~/.claude/settings.json` in WSL. */
  settingsPath: string;
  /** Unified diff of what installing would change; `null` when installed. */
  diff: string | null;
}

export interface TokenTotals {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

/** One model's share of a window. */
export interface ModelUsage extends TokenTotals {
  /** `message.model`, such as "claude-opus-5-5"; empty for entries that name none. */
  model: string;
  /** The part of `cacheWrite` written with the 1-hour lifetime; the rest is 5-minute. */
  cacheWrite1h: number;
}

/** Each window's totals by model, heaviest first. */
export interface ModelWindows {
  today: ModelUsage[];
  last7Days: ModelUsage[];
  last5Hours: ModelUsage[];
}

export interface RepoUsage {
  /** The repo's path, or the session's working directory when it matches no repo. */
  path: string;
  name: string;
  last7Days: TokenTotals;
}

/** One plan-limit window as Claude Code's status line last saw it. */
export interface LimitWindow {
  /** Percentage of the window's limit used; can pass 100. */
  usedPercent: number;
  /** RFC 3339: when the window resets. */
  resetsAt: string;
}

/**
 * Plan-limit percentages relayed from Claude Code's status line through
 * `~/.claude/usage-state.json` (spec §3.5). Either window can be missing.
 */
export interface PlanLimits {
  fiveHour?: LimitWindow;
  sevenDay?: LimitWindow;
  /** RFC 3339: when the status line wrote the reading. */
  updatedAt: string;
}

/** Token use summed from Claude Code's local session files (spec §3.5). */
export interface UsageSummary {
  /** Since local midnight in WSL. */
  today: TokenTotals;
  last7Days: TokenTotals;
  /** Rolling, ending now. */
  last5Hours: TokenTotals;
  /** Running `claude` sessions. */
  liveAgents: number;
  /** Heaviest first. */
  byRepo: RepoUsage[];
  /** The windows split by model; absent from helpers older than the cost estimate. */
  byModel?: ModelWindows;
  /** Plan limits from the status line's file; absent with no reading or from older helpers. */
  limits?: PlanLimits;
  /** RFC 3339. */
  computedAt: string;
  /** False until the helper has read the transcripts once; the totals are zeros until then. */
  ready: boolean;
}

/** Machine load, sent as `system://stats` about every 2 s while visible. */
export interface SystemStats {
  cpuPercent: number;
  memUsedBytes: number;
  memTotalBytes: number;
  /** Busiest GPU engine, as in GpuUsage. */
  gpuPercent: number;
  /** Refract's own share: the app, its WebView2 processes and the helper's wsl.exe. */
  own: { cpuPercent: number; memBytes: number; gpuPercent: number };
}

// Move to WSL (.plans/wsl-migration-spec.md). The commands are documented in
// app/src-tauri/src/migrate/mod.rs; the report types mirror refract-core's migrate module.

/** Why a folder counts as a build folder that is left out of the copy. */
export type BuildFolder =
  | "nodeModules"
  | "pythonVenv"
  | "pythonCache"
  | "cargoTarget"
  | "toolCache"
  | "output";

/** A build folder left out of the copy, with its size. */
export interface SkippedFolder {
  /** Relative to the repo, `/`-separated. */
  path: string;
  rule: BuildFolder;
  bytes: number;
  files: number;
}

/** A command run in the copy after the checks pass. */
export interface InstallStep {
  folder: string;
  /** Must exist in WSL. */
  tool: string;
  command: string;
  /** What to run to get `tool` when it is missing. */
  installHint: string;
}

export type GitOperation = "merge" | "rebase" | "cherryPick" | "revert" | "bisect";

/** One reason a migration can't start. */
export type Refusal =
  | { kind: "notARepo" }
  | { kind: "notOnWindowsDrive" }
  | { kind: "linkedWorktree" }
  | { kind: "hasLinkedWorktrees" }
  | { kind: "operationInProgress"; operation: GitOperation }
  | { kind: "externalLinks"; paths: string[] }
  | { kind: "unreadable"; paths: string[] }
  | { kind: "lfsMissing" }
  | { kind: "destNotLinux" }
  | { kind: "destExists" }
  | { kind: "notEnoughSpace"; needed: number; free: number }
  // What only the Windows side sees.
  /** Only in the cloud (OneDrive placeholders): never opened, since that downloads them. */
  | { kind: "cloudOnly"; paths: string[] }
  | { kind: "trailingDotOrSpace"; paths: string[] }
  /** The folder itself is a symlink or junction. */
  | { kind: "sourceIsLink" };

/** A refusal with the sentence the dialog shows. */
export type RefusalView = Refusal & { message: string };

/** A VS Code window that has the folder open; `id` is for `migrateCloseWindow`. */
export interface VsCodeWindow {
  id: number;
  title: string;
}

/** An install step of the preflight; `missing` when its tool isn't found in WSL. */
export interface PlannedInstallStep extends InstallStep {
  missing: boolean;
}

/** What the dialog shows before Start, from `migratePreflight`. */
export interface MigratePreflight {
  /** The source's WSL path (`/mnt/c/...`). */
  source: string;
  windowsPath: string;
  name: string;
  /** The destination asked for, else the default one; `null` when none was found. */
  dest: string | null;
  /** Empty when the move may start. */
  refusals: RefusalView[];
  warnings: string[];
  /** What will be copied. */
  files: number;
  dirs: number;
  bytes: number;
  skipped: SkippedFolder[];
  install: PlannedInstallStep[];
  /** Alternate data streams (such as Zone.Identifier) that will be dropped. */
  droppedStreams: number;
  /** Claude sessions (transcripts) whose history moves with it. */
  historySessions: number;
  /** Each one blocks Start until closed. */
  vscodeWindows: VsCodeWindow[];
  /** `git.exe` is missing, so Linux git reads the source. */
  linuxGit: boolean;
  /** In a folder OneDrive (or another cloud service) syncs: deleting the original deletes it there too. */
  cloudSynced: boolean;
}

/** What a preflight or snapshot is doing. */
export type SnapshotPhase = "git" | "walking" | "probing" | "hashing" | "blobs";

/** Sent as `migrate://snapshot` while a preflight or snapshot reads the source. */
export interface MigrateSnapshotProgress {
  /** The source's WSL path. */
  path: string;
  phase: SnapshotPhase;
  filesDone: number;
  filesTotal: number;
  bytesDone: number;
  bytesTotal: number;
}

/** A step of a running migration in the helper. */
export type MigrateStep =
  | "copying"
  | "normalising"
  | "hashing"
  | "checkingGit"
  | "rechecking"
  | "movingHistory"
  | "installing";

/** Data of a `migrateProgress` event. `copying` counts bytes; the others count items. */
export interface MigrateProgress {
  id: string;
  /** WSL paths of the original (`/mnt/...`) and the copy, so a reloaded window can place it. */
  source: string;
  dest: string;
  step: MigrateStep;
  done: number;
  total: number;
  /** The copy is in place (from the install on, and during a re-sync): a cancel keeps it. Missing from older helpers. */
  placed?: boolean;
}

export interface MigrateProblem {
  path?: string;
  message: string;
}

export type MigrateCheck = "files" | "gitState" | "nothingSkipped" | "unchanged" | "history";

export interface MigrateCheckResult {
  check: MigrateCheck;
  /** The check's name as the report shows it. */
  title: string;
  status: "passed" | "failed" | "notRun";
  problems: MigrateProblem[];
  /** What to do; set when it failed. */
  advice?: string;
}

export interface MigrateInstallResult {
  step: InstallStep;
  /** `cancelled`: stopped before it finished (the move was stopped once the copy was in place, or the helper stopped). */
  status: "succeeded" | "failed" | "timedOut" | "toolMissing" | "cancelled";
  exitCode?: number;
  /** WSL path of the log. */
  logPath?: string;
}

export interface MigrateTotals {
  files: number;
  dirs: number;
  bytes: number;
}

export interface DroppedStream {
  path: string;
  stream: string;
  size: number;
}

/** The migration report (spec §2.4). */
export interface MigrationReport {
  version: number;
  id: string;
  /** WSL path of the original (`/mnt/...`). */
  source: string;
  /** WSL path of the copy. */
  dest: string;
  /** RFC 3339. */
  startedAt: string;
  finishedAt: string;
  /** The five checks, in order. Delete original needs the first four passed. */
  checks: MigrateCheckResult[];
  skipped: SkippedFolder[];
  droppedStreams: DroppedStream[];
  newObjects: number;
  sourceTotals: MigrateTotals;
  copyTotals: MigrateTotals;
  install: MigrateInstallResult[];
  /** Repo-relative paths the install created, changed or removed; they don't block delete. */
  installChanges?: { created: string[]; changed: string[]; removed: string[] };
  /**
   * What changed in the copy while the install was cut short (the helper stopped): the install's doing or the
   * user's own edits. Backed up from the original like `installChanges`; Discard and Re-sync are refused while set.
   */
  recoveredChanges?: { created: string[]; changed: string[]; removed: string[] };
  /** Linux folder holding the original's version of each file the install changed or removed. */
  installBackup?: string;
  history: { sessions: number; fromDirs: string[]; toDirs: string[] };
  linuxGit: boolean;
  warnings: string[];
  /** The copy was put in place (a check that failed before that deleted it): Keep both is possible. */
  placed?: boolean;
}

/** A report still waiting for an answer, from `migratePending`. */
export interface PendingMigration {
  id: string;
  report: MigrationReport;
  /** The delete has begun (the helper's deleting state): only finishing it is left, or ending it once the copy is gone. */
  deleting?: boolean;
  /** The placed copy is gone (deleted by hand): discarding only forgets the move. */
  copyMissing?: boolean;
}

/** What `migrateCancel` did. */
export interface CancelOutcome {
  /** The partial copy was deleted and the move is gone. False: the copy was already in place (or a re-sync ran), so it stays and a report follows. */
  ended: boolean;
}

/** What the helper found checking the original again, right before deleting it. */
export interface Recheck {
  /** Modified or added in the original since the check. */
  changed: string[];
  /** Gone from the original: still in the copy, so deleting stays safe. */
  removed: string[];
  /** Couldn't be read, so whether they changed is unknown. */
  unreadable: string[];
  /**
   * Changed in the copy after it was checked; `["."]` when the copy is gone. While deleting, only paths whose
   * original is still there (what Delete the rest anyway would delete a different version of).
   */
  copyChanged: string[];
  /** Of `copyChanged`, deleted in the copy: deleting the original removes these for good. */
  copyRemoved?: string[];
  /** Of `copyChanged`, couldn't be read in the copy (`"."` for the folder itself). Never forced. */
  copyUnreadable?: string[];
  /** The copy folder is gone (`copyChanged` is `["."]`), as against one that can't be read. */
  copyMissing?: boolean;
  /** Sentences to show as they are, one per agent still running in the folder. */
  busyAgents: string[];
  /** The original isn't there over `/mnt`; the app checks from Windows before saying it's gone. */
  sourceMissing: boolean;
}

/**
 * The copy changes the user saw and answered Delete the rest anyway for, by how each changed in the copy. The helper
 * skips a path only while it still changed that way.
 */
export interface Forced {
  /** Of `copyChanged`, those not in `copyRemoved`: edited or added in the copy. */
  changed: string[];
  /** Of `copyRemoved`: deleted in the copy. */
  removed: string[];
}

/** Nothing forced. */
export const NO_FORCE: Forced = { changed: [], removed: [] };

/**
 * What Delete original did. `deleting` says the delete has begun (the helper's deleting
 * state): from then on only finishing it is offered (or, with the copy gone, ending the move),
 * never Re-sync, Keep both or Discard.
 */
export type DeleteOutcome =
  /** The recheck failed, so nothing (more) was deleted. */
  | ({ kind: "changed"; deleting: boolean } & Recheck)
  /** Close these first. */
  | { kind: "windowsOpen"; windows: VsCodeWindow[]; deleting: boolean }
  /** Ask "Delete permanently?" (default No: Keep both, or later while deleting). */
  | { kind: "wontFit"; reason: string; deleting: boolean }
  /** The original is there on Windows, but its drive isn't visible from WSL, so it couldn't be checked; the delete didn't begin unless it had already. */
  | { kind: "driveHidden"; deleting: boolean }
  /** The delete began but didn't finish (a file was in use, or the helper didn't answer). */
  | { kind: "unfinished"; reason: string }
  | {
      kind: "deleted";
      permanent: boolean;
      /** The original folder was already gone (deleted by hand). */
      alreadyGone: boolean;
      shortcut?: string;
      shortcutError?: string;
    };

/** Keep both, or discard the copy. Deleting the original goes through `migrateDelete`. */
export type FinishAction = "keep" | "discard";

/** Why a migration command failed; `message` is meant for a person. */
export interface MigrateError {
  kind:
    | "invalid"
    | "refused"
    | "busy"
    | "cancelled"
    | "git"
    | "agent"
    | "io"
    | "notReady"
    | "internal";
  message: string;
  /** Set for `refused`. */
  refusals?: RefusalView[];
}
