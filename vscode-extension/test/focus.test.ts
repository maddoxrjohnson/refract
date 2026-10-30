import { afterEach, describe, expect, it, vi } from "vitest";
import { findTerminal, PROCESS_ID_TIMEOUT_MS } from "../src/focus";

afterEach(() => {
  vi.useRealTimers();
});

interface FakeTerminal {
  name: string;
  pid: number | undefined | Error | "never";
}

const processId = (t: FakeTerminal): PromiseLike<number | undefined> => {
  if (t.pid === "never") return new Promise(() => {});
  return t.pid instanceof Error ? Promise.reject(t.pid) : Promise.resolve(t.pid);
};

describe("findTerminal", () => {
  const terminals: FakeTerminal[] = [
    { name: "gone", pid: new Error("disposed") },
    { name: "starting", pid: undefined },
    { name: "other", pid: 300 },
    { name: "agent", pid: 640 },
    { name: "agent too", pid: 600 },
  ];

  it("finds the first terminal whose shell is in the chain", async () => {
    expect((await findTerminal(terminals, processId, [812, 640, 600]))?.name).toBe("agent");
    expect((await findTerminal(terminals, processId, [812, 600]))?.name).toBe("agent too");
  });

  it("finds nothing when no terminal matches", async () => {
    expect(await findTerminal(terminals, processId, [812, 700])).toBeUndefined();
    expect(await findTerminal([], processId, [812])).toBeUndefined();
  });

  it("treats a processId that throws as no match", async () => {
    const throwing = (t: FakeTerminal) => {
      if (t.name === "gone") throw new Error("disposed");
      return processId(t);
    };
    expect((await findTerminal(terminals, throwing, [640]))?.name).toBe("agent");
  });

  it("does not wait on a terminal that never answers", async () => {
    vi.useFakeTimers();
    const stuck: FakeTerminal[] = [
      { name: "stuck", pid: "never" },
      { name: "agent", pid: 640 },
    ];
    const asked: string[] = [];
    const watching = (t: FakeTerminal) => {
      asked.push(t.name);
      return processId(t);
    };
    let result: FakeTerminal | undefined | null = null;
    void findTerminal(stuck, watching, [640]).then((t) => (result = t));
    // Both are asked at once, not one after the other.
    expect(asked).toEqual(["stuck", "agent"]);
    await vi.advanceTimersByTimeAsync(PROCESS_ID_TIMEOUT_MS - 1);
    expect(result).toBeNull();
    await vi.advanceTimersByTimeAsync(1);
    expect(result).toEqual({ name: "agent", pid: 640 });
  });

  it("counts a terminal that times out as no match", async () => {
    vi.useFakeTimers();
    let result: FakeTerminal | undefined | null = null;
    void findTerminal([{ name: "stuck", pid: "never" }], processId, [640], 50).then((t) => (result = t));
    await vi.advanceTimersByTimeAsync(50);
    expect(result).toBeUndefined();
  });
});
