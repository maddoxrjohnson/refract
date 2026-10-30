// Claim-by-rename scanning of the launch directory. Must not import `vscode`.
import * as path from "node:path";
import { ageMinutes, classifyAge, matchesFolder, parseRequest, type LaunchRequest } from "./request";

export interface ClaimFs {
  readdir(dir: string): Promise<string[]>;
  readFile(file: string): Promise<string>;
  rename(from: string, to: string): Promise<void>;
  unlink(file: string): Promise<void>;
}

export interface ClaimedRequest {
  request: LaunchRequest;
  claimedPath: string;
  /** Older than the fresh window: the user must confirm before it starts. */
  late: boolean;
  /** Whole minutes between `createdAt` and the scan. */
  ageMinutes: number;
}

export type Log = (message: string) => void;

function isErrno(err: unknown, code: string): boolean {
  return typeof err === "object" && err !== null && (err as { code?: unknown }).code === code;
}

/**
 * Claim every request in `dir` addressed to `folder` that is fresh or late (see
 * `classifyAge`). When several late requests are found at once, only the newest
 * is kept; the older ones are deleted, so a held key cannot stack up prompts.
 * A late focus request is deleted without being claimed: showing a terminal
 * minutes after it was asked for would only surprise.
 */
export async function claimRequests(
  fs: ClaimFs,
  dir: string,
  folder: string,
  now: Date | number,
  log: Log = () => {},
): Promise<ClaimedRequest[]> {
  let names: string[];
  try {
    names = await fs.readdir(dir);
  } catch (err) {
    if (!isErrno(err, "ENOENT")) {
      log(`cannot list ${dir}: ${String(err)}`);
    }
    return [];
  }
  const claimed: ClaimedRequest[] = [];
  for (const name of names.filter((n) => n.endsWith(".json")).sort()) {
    const file = path.join(dir, name);
    let text: string;
    try {
      text = await fs.readFile(file);
    } catch (err) {
      if (!isErrno(err, "ENOENT")) {
        log(`skip ${name}: cannot read: ${String(err)}`);
      }
      continue;
    }
    const parsed = parseRequest(text);
    if (!parsed.ok) {
      log(`skip ${name}: ${parsed.reason}`);
      continue;
    }
    const { request } = parsed;
    if (!matchesFolder(request, folder)) {
      log(`skip ${name}: other folder`);
      continue;
    }
    const freshness = classifyAge(request.createdAt, now);
    if (freshness === "ignore") {
      log(`skip ${name}: expired`);
      continue;
    }
    if (request.focus && freshness === "ask") {
      try {
        await fs.unlink(file);
        log(`dropped ${name}: a late focus request`);
      } catch (err) {
        if (!isErrno(err, "ENOENT")) {
          log(`skip ${name}: cannot remove a late focus request: ${String(err)}`);
        }
      }
      continue;
    }
    const claimedPath = path.join(dir, name.slice(0, -".json".length) + ".claimed");
    try {
      await fs.rename(file, claimedPath);
    } catch (err) {
      log(
        isErrno(err, "ENOENT")
          ? `skip ${name}: claimed elsewhere`
          : `skip ${name}: cannot claim: ${String(err)}`,
      );
      continue;
    }
    log(`claimed ${name}`);
    claimed.push({
      request,
      claimedPath,
      late: freshness === "ask",
      ageMinutes: ageMinutes(request.createdAt, now),
    });
  }
  return dropOlderLate(fs, claimed, log);
}

async function dropOlderLate(fs: ClaimFs, claimed: ClaimedRequest[], log: Log): Promise<ClaimedRequest[]> {
  const late = claimed.filter((c) => c.late);
  if (late.length < 2) {
    return claimed;
  }
  const newest = late.reduce((a, b) =>
    Date.parse(b.request.createdAt) > Date.parse(a.request.createdAt) ? b : a,
  );
  const kept: ClaimedRequest[] = [];
  for (const c of claimed) {
    if (c.late && c !== newest) {
      log(`dropped ${c.request.requestId}: a newer late request replaces it`);
      await release(fs, c, log);
    } else {
      kept.push(c);
    }
  }
  return kept;
}

/** Delete a claimed file once its terminal has started. */
export async function release(fs: ClaimFs, claimed: ClaimedRequest, log: Log = () => {}): Promise<void> {
  try {
    await fs.unlink(claimed.claimedPath);
  } catch (err) {
    if (!isErrno(err, "ENOENT")) {
      log(`cannot remove ${claimed.claimedPath}: ${String(err)}`);
    }
  }
}

export interface PromptDeps {
  /** Shows `message` with `buttons`; resolves to the pressed button, or undefined if closed. */
  showMessage(message: string, ...buttons: string[]): Promise<string | undefined>;
  /** Starts the request exactly as a fresh one would start. */
  start(claimed: ClaimedRequest): Promise<void>;
  fs: ClaimFs;
  log: Log;
}

/** Asks whether to start a late request. Start runs it; anything else deletes it. */
export async function promptLate(claimed: ClaimedRequest, deps: PromptDeps): Promise<void> {
  const { request } = claimed;
  const { log } = deps;
  try {
    const choice = await deps.showMessage(
      `Refract asked to start "${request.title}" here ${claimed.ageMinutes} minutes ago, but the window took too long to open. Start it now?`,
      "Start",
      "Dismiss",
    );
    if (choice === "Start") {
      await deps.start(claimed);
    } else {
      log(`dismissed ${request.requestId}`);
      await release(deps.fs, claimed, log);
    }
  } catch (err) {
    log(`failed to start ${request.requestId}: ${String(err)}`);
  }
}

/**
 * Serialises scans so overlapping triggers (watcher, focus, activation) never
 * handle the same request twice. Each scan runs to completion, including the
 * handler, before the next starts.
 */
export class Scanner {
  private tail: Promise<void> = Promise.resolve();
  private readonly seen = new Set<string>();

  constructor(
    private readonly scanOnce: () => Promise<ClaimedRequest[]>,
    private readonly handle: (claimed: ClaimedRequest, first: boolean) => Promise<void>,
    private readonly log: Log = () => {},
  ) {}

  /** `first` marks the activation scan; its requests get the bare layout. */
  scan(first = false): Promise<void> {
    const run = this.tail.then(async () => {
      for (const claimed of await this.scanOnce()) {
        if (this.seen.has(claimed.request.requestId)) {
          continue;
        }
        this.seen.add(claimed.request.requestId);
        try {
          await this.handle(claimed, first);
        } catch (err) {
          this.log(`failed to start ${claimed.request.requestId}: ${String(err)}`);
        }
      }
    });
    this.tail = run.catch((err) => this.log(`scan failed: ${String(err)}`));
    return this.tail;
  }
}
