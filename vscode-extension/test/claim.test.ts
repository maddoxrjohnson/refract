import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { claimRequests, release, Scanner, type ClaimFs } from "../src/claim";

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

  it("skips other folders, stale, invalid, unknown version and non-json files", async () => {
    write("other", req("other", { folder: "/elsewhere" }));
    write("stale", req("stale", { createdAt: "2026-10-29T17:00:00Z" }));
    write("bad", "{not json");
    write("v2", req("v2", { version: 2 }));
    fs.writeFileSync(path.join(dir, "note.txt"), "x");
    const logs: string[] = [];
    const got = await claimRequests(nodeFs, dir, folder, now, (m) => logs.push(m));
    expect(got).toEqual([]);
    expect(fs.readdirSync(dir).sort()).toEqual(["bad.json", "note.txt", "other.json", "stale.json", "v2.json"]);
    expect(logs.length).toBe(4);
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
