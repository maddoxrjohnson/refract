import * as fs from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { isSafeToken, isStale, launchDir, matchesFolder, parseRequest, terminalCommand } from "../src/request";

const valid = {
  version: 1,
  requestId: "abc",
  folder: "/home/me/projects/example",
  argv: ["claude", "--session-id", "0b7e5c1a-1111-4222-8333-444455556666"],
  title: "New agent",
  createdAt: "2026-10-29T17:54:44Z",
};

describe("parseRequest", () => {
  it("accepts a valid request", () => {
    const r = parseRequest(JSON.stringify(valid));
    expect(r).toEqual({ ok: true, request: valid });
  });
  it("rejects bad JSON and non-objects", () => {
    expect(parseRequest("{").ok).toBe(false);
    expect(parseRequest("[]").ok).toBe(false);
    expect(parseRequest("null").ok).toBe(false);
  });
  it("rejects unknown versions", () => {
    expect(parseRequest(JSON.stringify({ ...valid, version: 2 })).ok).toBe(false);
    const { version: _v, ...rest } = valid;
    expect(parseRequest(JSON.stringify(rest)).ok).toBe(false);
  });
  it.each(["requestId", "folder", "title", "createdAt"])("requires %s", (key) => {
    expect(parseRequest(JSON.stringify({ ...valid, [key]: undefined })).ok).toBe(false);
    expect(parseRequest(JSON.stringify({ ...valid, [key]: 5 })).ok).toBe(false);
  });
  it("requires a non-empty string argv", () => {
    for (const argv of [[], "claude", [1], ["claude", null], undefined]) {
      expect(parseRequest(JSON.stringify({ ...valid, argv })).ok).toBe(false);
    }
  });
});

describe("matchesFolder", () => {
  it("matches exactly after stripping trailing slashes", () => {
    expect(matchesFolder({ folder: "/a/b" }, "/a/b")).toBe(true);
    expect(matchesFolder({ folder: "/a/b/" }, "/a/b")).toBe(true);
    expect(matchesFolder({ folder: "/a/b" }, "/a/b//")).toBe(true);
    expect(matchesFolder({ folder: "/a/b" }, "/a/c")).toBe(false);
    expect(matchesFolder({ folder: "/a/b" }, "/a/b/c")).toBe(false);
    expect(matchesFolder({ folder: "/a/b" }, "/a/B")).toBe(false);
  });
});

describe("isStale", () => {
  const now = Date.parse("2026-10-29T18:00:00Z");
  it("is fresh within two minutes", () => {
    expect(isStale("2026-10-29T17:59:30Z", now)).toBe(false);
    expect(isStale("2026-10-29T17:58:00Z", new Date(now))).toBe(false);
  });
  it("is stale beyond two minutes", () => {
    expect(isStale("2026-10-29T17:57:59Z", now)).toBe(true);
  });
  it("is stale when unparseable", () => {
    expect(isStale("yesterday-ish", now)).toBe(true);
    expect(isStale("", now)).toBe(true);
  });
  it("is stale when more than a minute in the future", () => {
    expect(isStale("2026-10-29T18:00:50Z", now)).toBe(false);
    expect(isStale("2026-10-29T18:01:01Z", now)).toBe(true);
  });
});

describe("terminalCommand", () => {
  it("types safe tokens as they are, with no env", () => {
    expect(terminalCommand(["claude", "--session-id", "0b7e5c1a-1111-4222-8333-444455556666"])).toEqual({
      line: "claude --session-id 0b7e5c1a-1111-4222-8333-444455556666",
      env: {},
    });
  });
  it("classifies safe tokens", () => {
    for (const t of ["claude", "/usr/bin/claude", "a.b_c-d", "k=v", "a@b:c%d+e"]) {
      expect(isSafeToken(t)).toBe(true);
    }
    for (const t of ["", "a b", "a\tb", "a\nb", "a'b", 'a"b', "$x", "`x`", "a;b", "a*b", "a\\b"]) {
      expect(isSafeToken(t)).toBe(false);
    }
  });
  it("passes awkward prompts through the environment", () => {
    const prompt = "tab\there\nnew line 'single' \"double\" $(rm -rf /) `id` $HOME \\ ; &";
    const { line, env } = terminalCommand(["claude", "--", prompt]);
    expect(line).toBe('claude -- "$REFRACT_ARG_2"');
    expect(env).toEqual({ REFRACT_ARG_2: prompt });
  });
  it("keeps the line short for a 5000 character prompt", () => {
    const prompt = "x ".repeat(2500);
    const { line, env } = terminalCommand(["claude", "--session-id", "abc", "--", prompt]);
    expect(line.length).toBeLessThan(200);
    expect(env.REFRACT_ARG_4).toBe(prompt);
    expect(Object.keys(env)).toEqual(["REFRACT_ARG_4"]);
  });
  it("only ever types safe tokens, spaces, double quotes and references", () => {
    const { line } = terminalCommand(["claude", "a b", "", "$(x)", "ok"]);
    expect(line).toBe('claude "$REFRACT_ARG_1" "$REFRACT_ARG_2" "$REFRACT_ARG_3" ok');
    expect(line).toMatch(/^[A-Za-z0-9._/:=@%+\- "$]+$/);
  });
  it("indexes variables by argv position and keeps the empty string", () => {
    expect(terminalCommand(["claude", "", "a b"]).env).toEqual({ REFRACT_ARG_1: "", REFRACT_ARG_2: "a b" });
  });
  it("rejects an argv[0] that is not a safe token", () => {
    expect(() => terminalCommand(["rm -rf /"])).toThrow();
    expect(() => terminalCommand(["$(x)", "a"])).toThrow();
    expect(() => terminalCommand([])).toThrow();
  });
  it("rejects NUL, which cannot live in the environment", () => {
    expect(() => terminalCommand(["claude", "a\0b"])).toThrow();
  });
  it("makes parseRequest reject such requests", () => {
    expect(parseRequest(JSON.stringify({ ...valid, argv: ["a b"] })).ok).toBe(false);
    expect(parseRequest(JSON.stringify({ ...valid, argv: ["claude", "a\u0000b"] })).ok).toBe(false);
  });
});

describe("shared launch_request_v1 fixture", () => {
  const text = fs.readFileSync(
    fileURLToPath(new URL("../../crates/refract-core/tests/fixtures/launch_request_v1.json", import.meta.url)),
    "utf8",
  );
  it("parses with the exact fields", () => {
    expect(parseRequest(text)).toEqual({
      ok: true,
      request: {
        version: 1,
        requestId: "6f1c2a7e-1b8d-4c3e-9a5f-2d4b6e8f0a1c",
        folder: "/home/example/projects/demo-repo",
        argv: [
          "claude",
          "--session-id",
          "0b7e4d2c-5a6f-4e1b-8c9d-3f2a1b0c9d8e",
          "--",
          "This repo has uncommitted changes.\tReview `git diff`.",
        ],
        title: "New agent",
        createdAt: "2026-10-29T17:54:44Z",
      },
    });
  });
  it("turns into a short line plus the prompt in env", () => {
    const parsed = parseRequest(text);
    if (!parsed.ok) throw new Error(parsed.reason);
    const { line, env } = terminalCommand(parsed.request.argv);
    expect(line).toBe('claude --session-id 0b7e4d2c-5a6f-4e1b-8c9d-3f2a1b0c9d8e -- "$REFRACT_ARG_4"');
    expect(line.length).toBeLessThan(200);
    expect(line).not.toMatch(/[\t\n]/);
    expect(env).toEqual({ REFRACT_ARG_4: "This repo has uncommitted changes.\tReview `git diff`." });
  });
});

describe("launchDir", () => {
  it("uses XDG_STATE_HOME when set", () => {
    expect(launchDir({ XDG_STATE_HOME: "/x/state" }, "/home/me")).toBe("/x/state/refract/launch");
  });
  it("falls back to ~/.local/state", () => {
    expect(launchDir({}, "/home/me")).toBe("/home/me/.local/state/refract/launch");
    expect(launchDir({ XDG_STATE_HOME: "" }, "/home/me")).toBe("/home/me/.local/state/refract/launch");
  });
});
