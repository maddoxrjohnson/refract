// Pure launch-request logic. Must not import `vscode`.
import * as path from "node:path";

export interface LaunchRequest {
  version: 1;
  requestId: string;
  folder: string;
  argv: string[];
  title: string;
  createdAt: string;
}

export type ParseResult =
  | { ok: true; request: LaunchRequest }
  | { ok: false; reason: string };

const STALE_AFTER_MS = 2 * 60 * 1000;
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
  const argv = o.argv;
  if (
    !Array.isArray(argv) ||
    argv.length === 0 ||
    !argv.every((a) => typeof a === "string")
  ) {
    return { ok: false, reason: "argv must be a non-empty array of strings" };
  }
  try {
    terminalCommand(argv as string[]);
  } catch (err) {
    return { ok: false, reason: (err as Error).message };
  }
  return {
    ok: true,
    request: {
      version: 1,
      requestId: o.requestId as string,
      folder: o.folder as string,
      argv: argv as string[],
      title: o.title as string,
      createdAt: o.createdAt as string,
    },
  };
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

/** Older than two minutes, unparseable, or more than a minute in the future. */
export function isStale(createdAt: string, now: Date | number): boolean {
  const created = Date.parse(createdAt);
  if (Number.isNaN(created)) {
    return true;
  }
  const age = (typeof now === "number" ? now : now.getTime()) - created;
  return age > STALE_AFTER_MS || age < -FUTURE_SLACK_MS;
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
