// Pure launch-request logic. Must not import `vscode`.
import * as path from "node:path";

export interface LaunchRequest {
  version: 1;
  requestId: string;
  folder: string;
  /** Empty for a focus request, which starts nothing. */
  argv: string[];
  title: string;
  createdAt: string;
  /** Show the terminal already running a session instead of starting one. */
  focus?: FocusTarget;
}

/** Which terminal a focus request shows: the one whose shell is among `pids`. */
export interface FocusTarget {
  sessionId: string;
  /** The session's `claude` process and its ancestors, nearest first. */
  pids: number[];
}

/** The most pids a focus request may list. */
export const MAX_FOCUS_PIDS = 32;

export type ParseResult =
  | { ok: true; request: LaunchRequest }
  | { ok: false; reason: string };

const STALE_AFTER_MS = 2 * 60 * 1000;
const EXPIRED_AFTER_MS = 60 * 60 * 1000;
const FUTURE_SLACK_MS = 60 * 1000;

export function parseRequest(text: string): ParseResult {
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return { ok: false, reason: "invalid JSON" };
  }
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    return { ok: false, reason: "not an object" };
  }
  const o = raw as Record<string, unknown>;
  if (o.version !== 1) {
    return { ok: false, reason: `unsupported version ${String(o.version)}` };
  }
  for (const key of ["requestId", "folder", "title", "createdAt"] as const) {
    if (typeof o[key] !== "string" || o[key] === "") {
      return { ok: false, reason: `missing or invalid ${key}` };
    }
  }
  let focus: FocusTarget | undefined;
  if (o.focus !== undefined) {
    const parsed = parseFocus(o.focus);
    if (typeof parsed === "string") {
      return { ok: false, reason: parsed };
    }
    focus = parsed;
  }
  const argv = o.argv;
  if (!Array.isArray(argv) || !argv.every((a) => typeof a === "string")) {
    return { ok: false, reason: "argv must be an array of strings" };
  }
  if (focus) {
    if (argv.length !== 0) {
      return { ok: false, reason: "a focus request must have an empty argv" };
    }
  } else if (argv.length === 0) {
    return { ok: false, reason: "argv must be a non-empty array of strings" };
  } else {
    try {
      terminalCommand(argv as string[]);
    } catch (err) {
      return { ok: false, reason: (err as Error).message };
    }
  }
  const request: LaunchRequest = {
    version: 1,
    requestId: o.requestId as string,
    folder: o.folder as string,
    argv: argv as string[],
    title: o.title as string,
    createdAt: o.createdAt as string,
  };
  if (focus) {
    request.focus = focus;
  }
  return { ok: true, request };
}

/** A valid `focus` object, or why it is not one. */
function parseFocus(raw: unknown): FocusTarget | string {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    return "focus must be an object";
  }
  const { sessionId, pids } = raw as Record<string, unknown>;
  if (typeof sessionId !== "string" || sessionId === "") {
    return "missing or invalid focus.sessionId";
  }
  if (
    !Array.isArray(pids) ||
    pids.length === 0 ||
    pids.length > MAX_FOCUS_PIDS ||
    !pids.every((p) => Number.isSafeInteger(p) && (p as number) > 0)
  ) {
    return `focus.pids must be 1 to ${MAX_FOCUS_PIDS} positive integers`;
  }
  return { sessionId, pids: pids as number[] };
}

function stripTrailingSlashes(p: string): string {
  const stripped = p.replace(/\/+$/, "");
  return stripped === "" && p !== "" ? "/" : stripped;
}

export function matchesFolder(
  request: Pick<LaunchRequest, "folder">,
  workspacePath: string,
): boolean {
  return (
    stripTrailingSlashes(request.folder) === stripTrailingSlashes(workspacePath)
  );
}

export type Freshness = "fresh" | "ask" | "ignore";

/**
 * What to do with a request of this age. Up to two minutes old it starts at
 * once. Up to an hour old it waits for the user to confirm. Older than that,
 * unparseable, or more than a minute in the future, it is ignored.
 */
export function classifyAge(createdAt: string, now: Date | number): Freshness {
  const created = Date.parse(createdAt);
  if (Number.isNaN(created)) {
    return "ignore";
  }
  const age = (typeof now === "number" ? now : now.getTime()) - created;
  if (age < -FUTURE_SLACK_MS || age > EXPIRED_AFTER_MS) {
    return "ignore";
  }
  return age > STALE_AFTER_MS ? "ask" : "fresh";
}

/** How long ago `createdAt` was, in whole minutes (0 if unparseable or in the future). */
export function ageMinutes(createdAt: string, now: Date | number): number {
  const created = Date.parse(createdAt);
  const age = (typeof now === "number" ? now : now.getTime()) - created;
  return Number.isNaN(age) || age < 0 ? 0 : Math.floor(age / 60_000);
}

const SAFE_TOKEN = /^[A-Za-z0-9._/:=@%+-]+$/;

/** A word that can be typed into any shell as-is. */
export function isSafeToken(token: string): boolean {
  return SAFE_TOKEN.test(token);
}

export interface TerminalCommand {
  /** Short single line to type: safe tokens, spaces and `"$REFRACT_ARG_n"` only. */
  line: string;
  /** Out-of-band values for the terminal's environment, keyed `REFRACT_ARG_<i>`. */
  env: Record<string, string>;
}

/**
 * Builds the line to type for `argv`. Anything that is not a safe token (tabs,
 * newlines, quotes, very long prompts) travels in the terminal's environment
 * and is referenced as `"$REFRACT_ARG_<i>"`, so the typed line stays short and
 * free of characters a tty or readline would interpret.
 */
export function terminalCommand(argv: readonly string[]): TerminalCommand {
  const [program] = argv;
  if (program === undefined || !isSafeToken(program)) {
    throw new Error("argv[0] must be a plain program name or path");
  }
  const env: Record<string, string> = {};
  const words = argv.map((arg, i) => {
    if (isSafeToken(arg)) {
      return arg;
    }
    if (arg.includes("\0")) {
      throw new Error(`argv[${i}] contains a NUL character`);
    }
    const name = `REFRACT_ARG_${i}`;
    env[name] = arg;
    return `"$${name}"`;
  });
  return { line: words.join(" "), env };
}

export function launchDir(
  env: Record<string, string | undefined>,
  home: string,
): string {
  const xdg = env.XDG_STATE_HOME;
  const base = xdg && path.isAbsolute(xdg) ? xdg : path.join(home, ".local", "state");
  return path.join(base, "refract", "launch");
}
