import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { DIRTY_REPO_PROMPT } from "../repoStatus";
import type { CodexReadiness, RemoteRepo } from "../backend/types";
import type { RemoteDetailsState } from "../hooks/useRemoteDetails";
import { codexLine, DetailPanel, type DetailPanelProps } from "./DetailPanel";
import { hoursAgo, NOW, testRepo, testSession } from "./testRepo";

afterEach(cleanup);

function setup(repo = testRepo(), extra: Partial<DetailPanelProps> = {}) {
  const props: DetailPanelProps = {
    repo,
    now: NOW,
    onPrimary: vi.fn(),
    onLaunch: vi.fn(),
    onOpenFolder: vi.fn(),
    onOpenUrl: vi.fn(),
    onClose: vi.fn(),
    onHide: vi.fn(),
    ...extra,
  };
  const view = render(<DetailPanel {...props} />);
  return { props, panel: screen.getByRole("complementary", { name: `${repo.name} details` }), view };
}

describe("DetailPanel", () => {
  it("shows branch, upstream, changes and the last commit with its age", () => {
    const { panel } = setup(testRepo({ ahead: 2, behind: 1, changedFiles: 3 }));
    const text = panel.textContent ?? "";
    expect(text).toContain("main");
    expect(text).toContain("2 ahead, 1 behind");
    expect(text).toContain("3 changed files");
    expect(text).toContain("Split the sync worker into stages");
    expect(text).toContain("2 hours ago");
    expect(text).toContain("No agent sessions yet");
  });

  it("says when there is no upstream or the branch is detached", () => {
    const { panel } = setup(testRepo({ ahead: null, behind: null, branch: null, remote: null }));
    expect(panel.textContent).toContain("No upstream");
    expect(panel.textContent).toContain("detached HEAD");
    expect(screen.queryByRole("button", { name: /Open on/ })).toBeNull();
  });

  it("shows the newest session: when, whether it finished, its first line", () => {
    const { panel } = setup(
      testRepo({
        sessions: [
          testSession("old", { kind: "finished", detail: "completed" }, 30, "Old news"),
          testSession("new", { kind: "interrupted", detail: "apiError" }, 5, "\nRunning the migration.\nMore"),
        ],
      }),
    );
    expect(panel.textContent).toContain("Didn't finish · 5 hours ago");
    expect(panel.textContent).toContain("Running the migration.");
    expect(panel.textContent).not.toContain("More");
    expect(panel.textContent).not.toContain("Old news");
  });

  it("counts the other sessions worth resuming besides the newest", () => {
    const one = setup(
      testRepo({
        sessions: [
          testSession("old", { kind: "interrupted", detail: "unansweredPrompt" }, 30),
          testSession("new", { kind: "finished", detail: "completed" }, 5),
        ],
      }),
    );
    expect(one.panel.textContent).toContain("1 more to resume");
    one.view.unmount();
    const two = setup(
      testRepo({
        sessions: [
          testSession("a", { kind: "interrupted", detail: "unansweredPrompt" }, 30),
          testSession("b", { kind: "interrupted", detail: "apiError" }, 20),
          testSession("new", { kind: "interrupted", detail: "apiError" }, 5),
        ],
      }),
    );
    expect(two.panel.textContent).toContain("2 more to resume");
    two.view.unmount();
    const none = setup(
      testRepo({ sessions: [testSession("new", { kind: "interrupted", detail: "apiError" }, 5)] }),
    );
    expect(none.panel.textContent).not.toContain("more to resume");
  });

  it("for a clean repo the primary action starts a new agent on Enter", () => {
    const { props } = setup();
    const primary = screen.getByRole("button", { name: /New agent/ });
    expect(primary.getAttribute("aria-describedby")).toBeTruthy();
    expect(document.getElementById(primary.getAttribute("aria-describedby")!)?.textContent).toMatch(
      /Enter starts a new agent/,
    );
    expect(screen.getAllByRole("button", { name: /New agent/ })).toHaveLength(1);
    expect(screen.queryByRole("button", { name: "Resume" })).toBeNull();
    fireEvent.click(primary);
    expect(props.onPrimary).toHaveBeenCalledOnce();
  });

  it("with unfinished work offers the picker on Enter plus direct Resume and New agent", () => {
    const repo = testRepo({
      changedFiles: 2,
      sessions: [testSession("s1", { kind: "interrupted", detail: "apiError" }, 3, "Hi")],
      preselect: { kind: "resume", sessionId: "s1" },
    });
    const { props } = setup(repo);
    fireEvent.click(screen.getByRole("button", { name: /Launch/ }));
    expect(props.onPrimary).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole("button", { name: "Resume" }));
    expect(props.onLaunch).toHaveBeenLastCalledWith({ kind: "resume", sessionId: "s1" });
    fireEvent.click(screen.getByRole("button", { name: "New agent" }));
    expect(props.onLaunch).toHaveBeenLastCalledWith({ kind: "new", prompt: DIRTY_REPO_PROMPT });
  });

  it("with a running agent in its Refract window offers to go to it", () => {
    const running = { ...testSession("s2", { kind: "live", detail: "waitingOnUser" }, 1, "Shall I?"), focusable: true };
    const { panel, props } = setup(testRepo({ sessions: [running], preselect: { kind: "focus", sessionId: "s2" } }));
    expect(panel.textContent).toContain("go to the running agent");
    // Waiting for you: the status line and the hint are the red ones.
    expect(panel.querySelector(".rf-detail__header .rf-state--attention .rf-sw--waiting")).not.toBeNull();
    expect(panel.querySelector(".rf-hint--attention")?.textContent).toContain("go to the running agent");
    fireEvent.click(screen.getByRole("button", { name: /Launch/ }));
    expect(props.onPrimary).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole("button", { name: "Go to agent" }));
    expect(props.onLaunch).toHaveBeenLastCalledWith({ kind: "focus", sessionId: "s2" });
    expect(screen.queryByRole("button", { name: "Resume" })).toBeNull();
  });

  it("opens the folder, the remote and closes", () => {
    const { props } = setup();
    fireEvent.click(screen.getByRole("button", { name: "Open folder" }));
    expect(props.onOpenFolder).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole("button", { name: "Open on GitHub" }));
    expect(props.onOpenUrl).toHaveBeenCalledWith("https://github.com/demo-user/tidepool");
    fireEvent.click(screen.getByRole("button", { name: /Close details/ }));
    expect(props.onClose).toHaveBeenCalledOnce();
  });

  it("hides the gem", () => {
    const { props } = setup();
    fireEvent.click(screen.getByRole("button", { name: "Hide" }));
    expect(props.onHide).toHaveBeenCalledOnce();
  });
});

const REMOTE: RemoteRepo = {
  kind: "gitlab",
  host: "gitlab.example.com",
  owner: "example-group/platform",
  name: "parser-project",
  webUrl: "https://gitlab.example.com/example-group/platform/parser-project",
  cloneUrl: "https://gitlab.example.com/example-group/platform/parser-project.git",
  defaultBranch: "main",
  language: "Kotlin",
  sizeKb: 4100,
  pushedAt: hoursAgo(3),
  fork: false,
  archived: false,
  private: true,
  description: "Shared project in a nested group.",
};

const ready = (extra: Partial<Extract<RemoteDetailsState, { status: "ready" }>["details"]> = {}): RemoteDetailsState => ({
  status: "ready",
  details: { openPullRequests: 2, ci: "failure", ciUrl: "https://gitlab.example.com/p/-/pipelines/7", fetchedAt: hoursAgo(0.2), ...extra },
});

describe("DetailPanel remote details", () => {
  it("shows loading, then pull request count, CI with a link and when it was fetched", () => {
    const { props, view } = setup(testRepo(), { details: { status: "loading", details: null } });
    expect(screen.getByRole("region", { name: "Pull requests and CI" }).textContent).toContain("Loading…");
    view.rerender(<DetailPanel {...props} details={ready()} />);
    const section = screen.getByRole("region", { name: "Pull requests and CI" });
    expect(section.textContent).toContain("2 pull requests");
    expect(section.textContent).toContain("Failing");
    expect(section.textContent).toContain("Updated 12 minutes ago");
    fireEvent.click(screen.getByRole("button", { name: "View run" }));
    expect(props.onOpenUrl).toHaveBeenCalledWith("https://gitlab.example.com/p/-/pipelines/7");
  });

  it("says merge requests for GitLab and handles none, no CI and refresh", () => {
    const onRefreshDetails = vi.fn();
    setup(testRepo(), {
      remote: REMOTE,
      details: ready({ openPullRequests: 0, ci: "none", ciUrl: null }),
      onRefreshDetails,
    });
    const section = screen.getByRole("region", { name: "Merge requests and CI" });
    expect(section.textContent).toContain("No open merge requests");
    expect(section.textContent).toContain("No CI");
    expect(screen.queryByRole("button", { name: "View run" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    expect(onRefreshDetails).toHaveBeenCalledOnce();
  });

  it("shows errors", () => {
    setup(testRepo(), { details: { status: "error", message: "GitHub said 401.", details: null } });
    expect(screen.getByRole("alert").textContent).toContain("GitHub said 401.");
  });

  it("opens the remote's own web address when it is known", () => {
    const { props } = setup(testRepo(), { remote: REMOTE });
    fireEvent.click(screen.getByRole("button", { name: "Open on GitLab" }));
    expect(props.onOpenUrl).toHaveBeenCalledWith(REMOTE.webUrl);
  });

  it("offers Clone and launch for a remote-only repo, without Open folder", () => {
    const repo = testRepo({
      path: "remote:gitlab.example.com/example-group/platform/parser-project",
      name: "parser-project",
      remote: { host: REMOTE.host, owner: REMOTE.owner, name: REMOTE.name },
      ahead: null,
      behind: null,
      lastCommit: null,
    });
    const { props, panel } = setup(repo, { remote: REMOTE, remoteOnly: true });
    expect(panel.textContent).toContain("Not cloned");
    expect(panel.textContent).toContain("Shared project in a nested group.");
    expect(panel.textContent).toContain("~/projects/parser-project");
    expect(screen.queryByRole("button", { name: "Open folder" })).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /Clone and launch/ }));
    expect(props.onPrimary).toHaveBeenCalledOnce();
    fireEvent.click(screen.getByRole("button", { name: "Hide" }));
    expect(props.onHide).toHaveBeenCalledOnce();
  });
  it("says a remote-only repo must be cloned before it can be refactored", () => {
    const { panel } = setup(testRepo(), { remoteOnly: true });
    expect(panel.textContent).toContain("Clone it first.");
    expect(screen.queryByRole("button", { name: /Refactor/ })).toBeNull();
  });

  it("shows the refactor section of a cloned repo", () => {
    const { panel } = setup(testRepo(), { refactor: <section aria-label="Refactor">refactor here</section> });
    expect(panel.textContent).toContain("refactor here");
  });

  describe("Codex", () => {
    const warnings: CodexReadiness = {
      state: "warnings",
      findings: [
        { file: "AGENTS.md", line: 14, kind: "claudeOnly", text: "Ask with AskUserQuestion first.", term: "AskUserQuestion" },
        { file: "notes/HANDOFF.md", line: null, kind: "claudeOnly", text: "Run /clear after each step.", term: "/clear" },
      ],
      gates: {},
    };

    it("says how Codex stands", () => {
      expect(codexLine(undefined)).toBe("Codex not installed");
      expect(codexLine({ state: "ready", findings: [], gates: {} })).toBe("Codex: Ready");
      expect(codexLine(warnings)).toBe("Codex: 2 warnings");
      expect(codexLine({ ...warnings, findings: warnings.findings.slice(0, 1) })).toBe("Codex: 1 warning");
      expect(codexLine({ state: "notReady", findings: [], gates: {} })).toBe("Codex: Not ready");
      const busy = (state: "working" | "waitingOnUser"): CodexReadiness => ({
        state: "busy",
        findings: [],
        gates: { codex: { kind: "busy", agent: "claude", sessionId: "s", state } },
      });
      expect(codexLine(busy("working"))).toBe("Codex: Busy (Claude working)");
      expect(codexLine(busy("waitingOnUser"))).toBe("Codex: Busy (Claude waiting on you)");
    });

    it("shows Codex not installed without readiness, and no button", () => {
      const { panel } = setup(testRepo(), { onMakeCodexReady: vi.fn() });
      expect(panel.textContent).toContain("Codex not installed");
      expect(screen.queryByRole("button", { name: "Make Codex-ready" })).toBeNull();
    });

    it("lists the findings on request and offers Make Codex-ready", () => {
      const onMakeCodexReady = vi.fn();
      const { panel } = setup(testRepo({ codex: warnings }), { onMakeCodexReady });
      expect(panel.textContent).toContain("Codex: 2 warnings");
      expect(panel.textContent).not.toContain("AGENTS.md:14");
      const toggle = screen.getByRole("button", { name: "Show 2 findings" });
      expect(toggle.getAttribute("aria-expanded")).toBe("false");
      fireEvent.click(toggle);
      expect(toggle.getAttribute("aria-expanded")).toBe("true");
      const items = screen.getAllByRole("listitem").map((li) => li.textContent);
      expect(items).toEqual(["AGENTS.md:14 Ask with AskUserQuestion first.", "notes/HANDOFF.md Run /clear after each step."]);
      fireEvent.click(screen.getByRole("button", { name: "Hide findings" }));
      expect(screen.queryAllByRole("listitem")).toHaveLength(0);
      fireEvent.click(screen.getByRole("button", { name: "Make Codex-ready" }));
      expect(onMakeCodexReady).toHaveBeenCalledOnce();
    });

    it("offers Make Codex-ready only when there is something to fix, and waits while launching", () => {
      const notReady: CodexReadiness = {
        state: "notReady",
        findings: [{ file: "AGENTS.md", line: null, kind: "noAgentsMd", text: "No AGENTS.md at the repo root" }],
        gates: {},
      };
      const { view } = setup(testRepo({ codex: notReady }), { onMakeCodexReady: vi.fn(), launching: true });
      expect(screen.getByRole("button", { name: "Make Codex-ready" })).toHaveProperty("disabled", true);
      view.unmount();
      setup(testRepo({ codex: { state: "ready", findings: [], gates: {} } }), { onMakeCodexReady: vi.fn() });
      expect(screen.queryByRole("button", { name: "Make Codex-ready" })).toBeNull();
      expect(screen.queryByRole("button", { name: /findings/ })).toBeNull();
    });

    const busyFor = (agent: "claude" | "codex"): CodexReadiness => ({
      state: agent === "codex" ? "busy" : "ready",
      findings: [],
      gates: { [agent]: { kind: "busy", agent: agent === "codex" ? "claude" : "codex", sessionId: "b", state: "working" } },
    });

    it("holds Resume and New agent back while their agent's gate is busy", () => {
      const cut = testSession("k1", { kind: "interrupted", detail: "apiError" }, 1);
      const { panel, props } = setup(
        testRepo({ sessions: [cut], preselect: { kind: "resume", sessionId: "k1" }, codex: busyFor("claude") }),
      );
      const resume = screen.getByRole("button", { name: "Resume" });
      const fresh = screen.getByRole("button", { name: "New agent" });
      expect(resume).toHaveProperty("disabled", true);
      expect(resume.getAttribute("title")).toBe("Busy: Codex is working");
      expect(fresh).toHaveProperty("disabled", true);
      expect(panel.textContent).toContain("Busy: Codex is working");
      fireEvent.click(resume);
      expect(props.onLaunch).not.toHaveBeenCalled();
    });

    it("resumes a Codex session while Codex is not ready", () => {
      const cut = { ...testSession("c1", { kind: "interrupted", detail: "apiError" }, 1), agent: "codex" as const };
      const notReady: CodexReadiness = { state: "notReady", findings: [], gates: {} };
      setup(testRepo({ sessions: [cut], preselect: { kind: "resume", sessionId: "c1" }, codex: notReady }));
      expect(screen.getByRole("button", { name: "Resume" })).toHaveProperty("disabled", false);
    });

    it("stops the idle other agent first from Resume and New agent", () => {
      const cut = testSession("k1", { kind: "interrupted", detail: "apiError" }, 1);
      const codex: CodexReadiness = { state: "ready", findings: [], gates: { claude: { kind: "stopIdle", sessionIds: ["idle-1"] } } };
      const { props } = setup(testRepo({ sessions: [cut], preselect: { kind: "resume", sessionId: "k1" }, codex }));
      fireEvent.click(screen.getByRole("button", { name: "Stop the idle Codex agent and resume" }));
      expect(props.onLaunch).toHaveBeenLastCalledWith({ kind: "resume", sessionId: "k1" }, ["idle-1"]);
      fireEvent.click(screen.getByRole("button", { name: "Stop the idle Codex agent and start Claude" }));
      expect(props.onLaunch).toHaveBeenLastCalledWith({ kind: "new" }, ["idle-1"]);
    });

    it("starts the New agent the preselect names, briefed", () => {
      const done = testSession("k1", { kind: "finished", detail: "completed" }, 1, null, 100_000, "uncommittedChanges");
      const { props } = setup(
        testRepo({
          sessions: [done],
          preselect: { kind: "newAgent" },
          newAgent: { agent: "codex", briefFrom: "k1", reason: { kind: "nudge", usedPercent: 97 } },
          codex: { state: "ready", findings: [], gates: {} },
        }),
      );
      fireEvent.click(screen.getByRole("button", { name: "New Codex agent, briefed" }));
      expect(props.onLaunch).toHaveBeenCalledWith({ kind: "new", agent: "codex", brief: "k1" });
    });

    it("opens the picker rather than starting Claude at once while Claude's gate is busy", () => {
      const { props } = setup(testRepo({ preselect: { kind: "noPicker" }, codex: busyFor("claude") }));
      expect(screen.getByRole("button", { name: /Launch…/ })).toBeTruthy();
      expect(screen.getByRole("button", { name: "New agent" })).toHaveProperty("disabled", true);
      expect(props.onLaunch).not.toHaveBeenCalled();
    });

    it("holds Make Codex-ready back while either agent is busy", () => {
      for (const agent of ["claude", "codex"] as const) {
        const codex = { ...busyFor(agent), state: "warnings" as const, findings: warnings.findings };
        const { view } = setup(testRepo({ codex }), { onMakeCodexReady: vi.fn() });
        const button = screen.getByRole("button", { name: "Make Codex-ready" });
        expect(button).toHaveProperty("disabled", true);
        expect(button.getAttribute("title")).toBe(`Busy: ${agent === "codex" ? "Claude" : "Codex"} is working`);
        view.unmount();
      }
    });

    it("holds Make Codex-ready back while an idle agent is open beside it", () => {
      const codex = {
        ...warnings,
        gates: { codex: { kind: "stopIdle" as const, sessionIds: ["s1"] } },
      };
      setup(testRepo({ codex }), { onMakeCodexReady: vi.fn() });
      const button = screen.getByRole("button", { name: "Make Codex-ready" });
      expect(button).toHaveProperty("disabled", true);
      expect(button.getAttribute("title")).toBe("Close the idle agent in this repo first");
    });

    it("resumes a Codex session as Codex", () => {
      const cut = { ...testSession("c1", { kind: "interrupted", detail: "apiError" }, 1), agent: "codex" as const };
      const { props } = setup(testRepo({ sessions: [cut], preselect: { kind: "resume", sessionId: "c1" } }));
      fireEvent.click(screen.getByRole("button", { name: "Resume" }));
      expect(props.onLaunch).toHaveBeenCalledWith({ kind: "resume", agent: "codex", sessionId: "c1" });
    });
  });
});
