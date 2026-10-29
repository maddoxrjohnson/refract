// Claim-by-rename scanning of the launch directory. Must not import `vscode`.
import * as path from "node:path";
import { isStale, matchesFolder, parseRequest, type LaunchRequest } from "./request";

export interface ClaimFs {
  readdir(dir: string): Promise<string[]>;
  readFile(file: string): Promise<string>;
  rename(from: string, to: string): Promise<void>;
  unlink(file: string): Promise<void>;
}

export interface ClaimedRequest {
  request: LaunchRequest;
  claimedPath: string;
}

export type Log = (message: string) => void;

function isErrno(err: unknown, code: string): boolean {
  return typeof err === "object" && err !== null && (err as { code?: unknown }).code === code;
}

/** Claim every fresh request in `dir` that is addressed to `folder`. */
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
    if (isStale(request.createdAt, now)) {
      log(`skip ${name}: stale`);
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
    claimed.push({ request, claimedPath });
  }
  return claimed;
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
