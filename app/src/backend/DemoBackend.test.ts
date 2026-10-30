import { describe, expect, it } from "vitest";
import { demoHookDiff } from "./DemoBackend";

describe("demoHookDiff", () => {
  it("shows the command and matchers the real installer writes", () => {
    const diff = demoHookDiff();
    expect(diff).toContain(
      `"command": "[ -f '/home/demo/.local/share/refract/status-hook.sh' ] && sh '/home/demo/.local/share/refract/status-hook.sh' waitingOnUser '/home/demo/.local/state/refract/sessions' || cat >/dev/null; exit 0"`,
    );
    expect(diff).toContain(`{ "matcher": "permission_prompt|elicitation_dialog", "hooks": [`);
    expect(diff).toContain(`{ "matcher": "startup|resume|clear", "hooks": [`);
    expect(diff).toContain(`"PostToolUse": [\n+      { "matcher": "*", "hooks": [`);
    expect(diff).toContain(`"Stop": [\n+      { "hooks": [`);
  });

  it("counts its lines in the hunk header", () => {
    const lines = demoHookDiff().trimEnd().split("\n").slice(3);
    const [, oldCount, newCount] = /^@@ -1,(\d+) \+1,(\d+) @@$/.exec(demoHookDiff().split("\n")[2])!;
    expect(lines.filter((l) => !l.startsWith("+")).length).toBe(Number(oldCount));
    expect(lines.filter((l) => !l.startsWith("-")).length).toBe(Number(newCount));
  });
});
