import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { claimRequests, promptLate, release, Scanner, type ClaimedRequest, type ClaimFs } from "../src/claim";

const nodeFs: ClaimFs = {
  readdir: (d) => fs.promises.readdir(d),
  readFile: (f) => fs.promises.readFile(f, "utf8"),
  rename: (a, b) => fs.promises.rename(a, b),
  unlink: (f) => fs.promises.unlink(f),
};

const folder = "/home/me/projects/example";
const now = Date.parse("2026-10-29T18:00:00Z");

function req(id: string, over: Record<string, unknown> = {}) {
  return {
    version: 1,
    requestId: id,
    folder,
    argv: ["claude", "--session-id", id],
    title: "New agent",
    createdAt: "2026-10-29T17:59:50Z",
    ...over,
  };
}

let dir: string;
const write = (id: string, body: unknown) =>
  fs.writeFileSync(path.join(dir, `${id}.json`), typeof body === "string" ? body : JSON.stringify(body));

beforeEach(() => {
  dir = fs.mkdtempSync(path.join(os.tmpdir(), "refract-claim-"));
});
afterEach(() => fs.rmSync(dir, { recursive: true, force: true }));

describe("claimRequests", () => {
  it("claims a matching request by renaming it", async () => {
    write("a", req("a"));
    const got = await claimRequests(nodeFs, dir, folder, now);
    expect(got.map((c) => c.request.requestId)).toEqual(["a"]);
    expect(fs.readdirSync(dir)).toEqual(["a.claimed"]);
  });

  it("skips other folders, expired, invalid, unknown version and non-json files", async () => {
    write("other", req("other", { folder: "/elsewhere" }));
    write("stale", req("stale", { createdAt: "2026-10-29T16:00:00Z" }));
    write("bad", "{not json");
    write("v2", req("v2", { version: 2 }));
    fs.writeFileSync(path.join(dir, "note.txt"), "x");
    const logs: string[] = [];
    const got = await claimRequests(nodeFs, dir, folder, now, (m) => logs.push(m));
    expect(got).toEqual([]);
    expect(fs.readdirSync(dir).sort()).toEqual(["bad.json", "note.txt", "other.json", "stale.json", "v2.json"]);
    expect(logs.length).toBe(4);
  });

  it("claims a request between two minutes and an hour old as late", async () => {
    write("a", req("a", { createdAt: "2026-10-29T17:53:00Z" }));
    write("b", req("b", { createdAt: "2026-10-29T17:59:30Z" }));
    const got = await claimRequests(nodeFs, dir, folder, now);
    expect(got.map((c) => [c.request.requestId, c.late, c.ageMinutes])).toEqual([
      ["a", true, 7],
      ["b", false, 0],
    ]);
    expect(fs.readdirSync(dir).sort()).toEqual(["a.claimed", "b.claimed"]);
  });

  it("keeps only the newest of several late requests and deletes the rest", async () => {
    write("a", req("a", { createdAt: "2026-10-29T17:50:00Z" }));
    write("b", req("b", { createdAt: "2026-10-29T17:55:00Z" }));
    write("c", req("c", { createdAt: "2026-10-29T17:52:00Z" }));
    write("d", req("d"));
    const got = await claimRequests(nodeFs, dir, folder, now);
    expect(got.map((c) => c.request.requestId)).toEqual(["b", "d"]);
    expect(fs.readdirSync(dir).sort()).toEqual(["b.claimed", "d.claimed"]);
  });

  it("returns nothing when the directory does not exist", async () => {
    expect(await claimRequests(nodeFs, path.join(dir, "nope"), folder, now)).toEqual([]);
  });

  it("skips a request another window claims first (rename ENOENT)", async () => {
    write("a", req("a"));
    const racing: ClaimFs = {
      ...nodeFs,
      rename: async (from, to) => {
        await nodeFs.rename(from, to); // the other window wins
        return nodeFs.rename(from, to + ".again"); // ours fails with ENOENT
      },
    };
    expect(await claimRequests(racing, dir, folder, now)).toEqual([]);
  });

  it("only one of two concurrent claimers gets a request", async () => {
    write("a", req("a"));
    const [x, y] = await Promise.all([
      claimRequests(nodeFs, dir, folder, now),
      claimRequests(nodeFs, dir, folder, now),
    ]);
    expect(x.length + y.length).toBe(1);
  });

  it("release deletes the claimed file and tolerates it being gone", async () => {
    write("a", req("a"));
    const [c] = await claimRequests(nodeFs, dir, folder, now);
    await release(nodeFs, c!);
    expect(fs.readdirSync(dir)).toEqual([]);
    await expect(release(nodeFs, c!)).resolves.toBeUndefined();
  });
});

describe("focus requests", () => {
  const focus = (id: string, createdAt: string) =>
    req(id, { argv: [], title: "Running agent", createdAt, focus: { sessionId: "s", pids: [812, 640] } });

  it("claims a fresh focus request", async () => {
    write("f", focus("f", "2026-10-29T17:59:50Z"));
    const got = await claimRequests(nodeFs, dir, folder, now);
    expect(got.map((c) => [c.request.requestId, c.request.focus?.pids, c.late])).toEqual([["f", [812, 640], false]]);
    expect(fs.readdirSync(dir)).toEqual(["f.claimed"]);
  });

  it("deletes a late focus request without claiming it or asking", async () => {
    write("f", focus("f", "2026-10-29T17:53:00Z"));
    write("a", req("a", { createdAt: "2026-10-29T17:53:00Z" }));
    const logs: string[] = [];
    const got = await claimRequests(nodeFs, dir, folder, now, (m) => logs.push(m));
    expect(got.map((c) => c.request.requestId)).toEqual(["a"]);
    expect(fs.readdirSync(dir)).toEqual(["a.claimed"]);
    expect(logs).toContain("dropped f.json: a late focus request");
  });
});

describe("Scanner", () => {
  it("never handles a request twice across overlapping scans", async () => {
    write("a", req("a"));
    const handled: Array<[string, boolean]> = [];
    const scanner = new Scanner(
      () => claimRequests(nodeFs, dir, folder, now),
      async (c, first) => {
        await new Promise((r) => setTimeout(r, 10));
        handled.push([c.request.requestId, first]);
      },
    );
    await Promise.all([scanner.scan(true), scanner.scan(), scanner.scan()]);
    expect(handled).toEqual([["a", true]]);
  });

  it("skips a request already seen even if it is listed again", async () => {
    // rename "succeeds" but the file stays listed, so only `seen` can stop a repeat
    const sticky: ClaimFs = {
      readdir: async () => ["a.json"],
      readFile: async () => JSON.stringify(req("a")),
      rename: async () => {},
      unlink: async () => {},
    };
    const handled: string[] = [];
    const scanner = new Scanner(
      () => claimRequests(sticky, dir, folder, now),
      async (c) => {
        handled.push(c.request.requestId);
      },
    );
    await scanner.scan(true);
    await scanner.scan();
    await Promise.all([scanner.scan(), scanner.scan()]);
    expect(handled).toEqual(["a"]);
  });

  it("passes first=false for later scans and survives handler errors", async () => {
    const handled: Array<[string, boolean]> = [];
    const logs: string[] = [];
    const scanner = new Scanner(
      () => claimRequests(nodeFs, dir, folder, now),
      async (c, first) => {
        if (c.request.requestId === "boom") throw new Error("nope");
        handled.push([c.request.requestId, first]);
      },
      (m) => logs.push(m),
    );
    await scanner.scan(true);
    write("boom", req("boom"));
    write("b", req("b"));
    await scanner.scan();
    expect(handled).toEqual([["b", false]]);
    expect(logs.some((l) => l.includes("boom"))).toBe(true);
  });
});

describe("promptLate", () => {
  async function lateClaim(): Promise<ClaimedRequest> {
    write("a", req("a", { title: "Fix the widget", createdAt: "2026-10-29T17:53:00Z" }));
    const [c] = await claimRequests(nodeFs, dir, folder, now);
    return c!;
  }

  function deps(answer: string | undefined) {
    const shown: Array<[string, string[]]> = [];
    const started: string[] = [];
    return {
      shown,
      started,
      deps: {
        showMessage: async (message: string, ...buttons: string[]) => {
          shown.push([message, buttons]);
          return answer;
        },
        start: async (c: ClaimedRequest) => {
          started.push(c.request.requestId);
        },
        fs: nodeFs,
        log: () => {},
      },
    };
  }

  it("names the request and its age, and offers Start and Dismiss", async () => {
    const claimed = await lateClaim();
    const d = deps("Dismiss");
    await promptLate(claimed, d.deps);
    expect(d.shown).toEqual([
      [
        'Refract asked to start "Fix the widget" here 7 minutes ago, but the window took too long to open. Start it now?',
        ["Start", "Dismiss"],
      ],
    ]);
  });

  it("starts the request on Start and leaves the file for start to release", async () => {
    const claimed = await lateClaim();
    const d = deps("Start");
    await promptLate(claimed, d.deps);
    expect(d.started).toEqual(["a"]);
    expect(fs.readdirSync(dir)).toEqual(["a.claimed"]);
  });

  it("deletes the claimed file and starts nothing on Dismiss", async () => {
    const claimed = await lateClaim();
    const d = deps("Dismiss");
    await promptLate(claimed, d.deps);
    expect(d.started).toEqual([]);
    expect(fs.readdirSync(dir)).toEqual([]);
  });

  it("treats a closed message like Dismiss", async () => {
    const claimed = await lateClaim();
    const d = deps(undefined);
    await promptLate(claimed, d.deps);
    expect(d.started).toEqual([]);
    expect(fs.readdirSync(dir)).toEqual([]);
  });

  it("logs and does not throw when starting fails", async () => {
    const claimed = await lateClaim();
    const logs: string[] = [];
    const d = deps("Start");
    await promptLate(claimed, {
      ...d.deps,
      start: async () => {
        throw new Error("nope");
      },
      log: (m) => logs.push(m),
    });
    expect(logs.some((l) => l.includes("failed to start a"))).toBe(true);
  });
});
