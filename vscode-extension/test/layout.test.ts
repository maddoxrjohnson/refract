import { describe, expect, it } from "vitest";
import { tabsToClose } from "../src/layout";

class Term {}
const isTerminal = (input: unknown) => input instanceof Term;
const tab = (name: string, input: unknown, isDirty = false) => ({ name, input, isDirty });

describe("tabsToClose", () => {
  it("selects clean non-terminal tabs in every group and keeps terminals", () => {
    const groups = [
      { tabs: [tab("a.ts", {}), tab("term1", new Term()), tab("b.ts", {})] },
      { tabs: [tab("welcome", {}), tab("term2", new Term())] },
    ];
    expect(tabsToClose(groups, isTerminal).map((t) => t.name)).toEqual(["a.ts", "b.ts", "welcome"]);
  });
  it("never selects a dirty tab", () => {
    const groups = [{ tabs: [tab("dirty.ts", {}, true), tab("clean.ts", {})] }];
    expect(tabsToClose(groups, isTerminal).map((t) => t.name)).toEqual(["clean.ts"]);
  });
  it("handles no groups and terminal-only groups", () => {
    expect(tabsToClose([], isTerminal)).toEqual([]);
    expect(tabsToClose([{ tabs: [tab("t", new Term())] }], isTerminal)).toEqual([]);
  });
});
