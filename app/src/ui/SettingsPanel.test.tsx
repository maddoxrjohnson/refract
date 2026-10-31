import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Backend } from "../backend/Backend";
import type { Settings } from "../backend/types";
import { DEFAULT_PRICES } from "../usage/cost";
import { SettingsPanel, type SettingsPanelProps } from "./SettingsPanel";

afterEach(cleanup);

const SETTINGS: Settings = {
  gitlabUrl: "https://gitlab.example.com",
  showForks: false,
  showArchived: false,
  hidden: ["remote:github.com/demo-user/orbit-cli", "/home/demo/projects/scratch"],
  costEstimate: {
    enabled: false,
    prices: [{ match: "opus", input: 5, output: 25, cacheWrite5m: 6.25, cacheWrite1h: 10, cacheRead: 0.5 }],
  },
  theme: "system",
};

function fakeBackend() {
  return {
    credentials: vi.fn().mockResolvedValue({ github: "ghCli", gitlab: "missing" }),
    setToken: vi.fn().mockResolvedValue(undefined),
    addRepo: vi.fn().mockResolvedValue(undefined),
    openLogFolder: vi.fn().mockResolvedValue(undefined),
  };
}

async function setup(extra: Partial<SettingsPanelProps> = {}) {
  const backend = fakeBackend();
  const props: SettingsPanelProps = {
    backend: backend as unknown as Backend,
    settings: SETTINGS,
    sources: [],
    remotesLoading: false,
    onChange: vi.fn().mockResolvedValue(undefined),
    onRefresh: vi.fn(),
    onClose: vi.fn(),
    hook: { info: null, error: null, install: vi.fn(), remove: vi.fn() },
    ...extra,
  };
  render(<SettingsPanel {...props} />);
  await act(async () => {});
  return { props, backend, dialog: screen.getByRole("dialog", { name: "Settings" }) };
}

describe("SettingsPanel", () => {
  it("is a focused dialog that closes on Escape", async () => {
    const { props, dialog } = await setup();
    expect(document.activeElement).toBe(dialog);
    const outer = vi.fn();
    window.addEventListener("keydown", outer);
    fireEvent.keyDown(dialog, { key: "Escape" });
    window.removeEventListener("keydown", outer);
    expect(props.onClose).toHaveBeenCalledWith(true);
    expect(outer).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: /Close settings/ }));
    expect(props.onClose).toHaveBeenLastCalledWith(false);
  });

  it("shows where tokens come from and source errors", async () => {
    const { dialog } = await setup({
      sources: [{ kind: "gitlab", host: "gitlab.example.com", configured: false, error: "No GitLab token." }],
    });
    expect(dialog.textContent).toContain("Token: GitHub CLI login");
    expect(dialog.textContent).toContain("Token: Missing");
    expect(dialog.textContent).toContain("No GitLab token.");
  });

  it("sends a token straight to the backend and clears the field", async () => {
    const { props, backend } = await setup();
    const input = screen.getByLabelText(/GitLab personal access token/) as HTMLInputElement;
    expect(input.type).toBe("password");
    fireEvent.change(input, { target: { value: " glpat-example " } });
    await act(async () => {
      fireEvent.submit(input.closest("form")!);
    });
    expect(backend.setToken).toHaveBeenCalledWith("gitlab", "glpat-example");
    expect(input.value).toBe("");
    expect(backend.credentials).toHaveBeenCalledTimes(2);
    expect(props.onRefresh).toHaveBeenCalledOnce();
  });

  it("saves the GitLab address, refusing anything but https", async () => {
    const { props } = await setup();
    const input = screen.getByLabelText("GitLab address");
    fireEvent.change(input, { target: { value: "http://gitlab.example.com" } });
    await act(async () => {
      fireEvent.submit(input.closest("form")!);
    });
    expect(props.onChange).not.toHaveBeenCalled();
    expect(screen.getByRole("alert").textContent).toMatch(/https:\/\//);
    fireEvent.change(input, { target: { value: "https://gitlab.example.com/" } });
    await act(async () => {
      fireEvent.submit(input.closest("form")!);
    });
    expect(props.onChange).toHaveBeenCalledWith({ ...SETTINGS, gitlabUrl: "https://gitlab.example.com" });
    expect(props.onRefresh).toHaveBeenCalledOnce();
  });

  it("picks light, dark or system under Appearance", async () => {
    const { props } = await setup();
    const group = screen.getByRole("radiogroup", { name: "Appearance" });
    const radios = within(group).getAllByRole("radio") as HTMLInputElement[];
    expect(radios.map((r) => r.labels?.[0]?.textContent)).toEqual(["Light", "Dark", "System"]);
    expect(within(group).getByRole("radio", { name: "System" })).toHaveProperty("checked", true);
    await act(async () => {
      fireEvent.click(within(group).getByRole("radio", { name: "Dark" }));
    });
    expect(props.onChange).toHaveBeenLastCalledWith({ ...SETTINGS, theme: "dark" });
  });

  it("shows why a theme did not save", async () => {
    const { props } = await setup({
      onChange: vi.fn().mockRejectedValue({ kind: "io", message: "Could not write settings.json." }),
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("radio", { name: "Light" }));
    });
    expect(props.onChange).toHaveBeenCalledWith({ ...SETTINGS, theme: "light" });
    expect(screen.getByRole("alert").textContent).toContain("Could not write settings.json.");
  });

  it("toggles forks and archived repos", async () => {
    const { props } = await setup();
    await act(async () => {
      fireEvent.click(screen.getByRole("checkbox", { name: "Show forks" }));
    });
    expect(props.onChange).toHaveBeenLastCalledWith({ ...SETTINGS, showForks: true });
    await act(async () => {
      fireEvent.click(screen.getByRole("checkbox", { name: "Show archived repos" }));
    });
    expect(props.onChange).toHaveBeenLastCalledWith({ ...SETTINGS, showArchived: true });
  });

  it("lists hidden repos and unhides them", async () => {
    const { props } = await setup();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Unhide demo-user/orbit-cli · github.com" }));
    });
    expect(props.onChange).toHaveBeenCalledWith({ ...SETTINGS, hidden: ["/home/demo/projects/scratch"] });
  });

  it("shows a failed save", async () => {
    await setup({ onChange: vi.fn().mockRejectedValue({ kind: "io", message: "Could not write settings." }) });
    await act(async () => {
      fireEvent.click(screen.getByRole("checkbox", { name: "Show forks" }));
    });
    expect(screen.getByRole("alert").textContent).toBe("Could not write settings.");
  });

  it("adds a local folder and refreshes remotes", async () => {
    const { props, backend } = await setup();
    const input = screen.getByLabelText(/Add a folder/);
    fireEvent.change(input, { target: { value: "/home/demo/code/sketches" } });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Add" }));
    });
    expect(backend.addRepo).toHaveBeenCalledWith("/home/demo/code/sketches");
    expect(screen.getByRole("status").textContent).toContain("Added /home/demo/code/sketches.");
    fireEvent.click(screen.getByRole("button", { name: "Refresh remotes" }));
    expect(props.onRefresh).toHaveBeenCalledOnce();
  });

  it("opens the log folder, showing a failure", async () => {
    const { backend } = await setup();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Open log folder" }));
    });
    expect(backend.openLogFolder).toHaveBeenCalledOnce();
    expect(screen.queryByRole("alert")).toBeNull();
    backend.openLogFolder.mockRejectedValueOnce({ kind: "failed", message: "Cannot open the folder." });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Open log folder" }));
    });
    expect(screen.getByRole("alert").textContent).toBe("Cannot open the folder.");
  });

  it("offers the log folder while settings load", async () => {
    await setup({ settings: null });
    expect(screen.getByRole("button", { name: "Open log folder" })).toBeTruthy();
  });

  describe("cost estimate", () => {
    const ON: Settings = { ...SETTINGS, costEstimate: { ...SETTINGS.costEstimate, enabled: true } };
    const OPUS = SETTINGS.costEstimate.prices[0];

    it("turns on at once and shows the table only when on", async () => {
      const { props } = await setup();
      expect(screen.queryByRole("table")).toBeNull();
      await act(async () => {
        fireEvent.click(screen.getByLabelText("Estimate cost at API prices"));
      });
      expect(props.onChange).toHaveBeenCalledWith(ON);
    });

    it("keeps edits as a draft and saves them as numbers", async () => {
      const { props } = await setup({ settings: ON });
      expect(screen.getAllByRole("row")).toHaveLength(2);
      fireEvent.change(screen.getByLabelText("Row 1 input price"), { target: { value: " 4.5 " } });
      fireEvent.click(screen.getByRole("button", { name: "Add row" }));
      fireEvent.change(screen.getByLabelText("Row 2 model match"), { target: { value: " Haiku " } });
      fireEvent.change(screen.getByLabelText("Row 2 cache read price"), { target: { value: "0.01" } });
      expect(props.onChange).not.toHaveBeenCalled();
      await act(async () => {
        fireEvent.click(screen.getByRole("button", { name: "Save prices" }));
      });
      expect(props.onChange).toHaveBeenCalledWith({
        ...ON,
        costEstimate: {
          enabled: true,
          prices: [
            { ...OPUS, input: 4.5 },
            { match: "Haiku", input: 0, output: 0, cacheWrite5m: 0, cacheWrite1h: 0, cacheRead: 0.01 },
          ],
        },
      });
      expect(screen.queryByRole("button", { name: "Save prices" })).toBeNull();
    });

    it("refuses blank matches and bad prices without saving", async () => {
      const { props } = await setup({ settings: ON });
      fireEvent.change(screen.getByLabelText("Row 1 output price"), { target: { value: "-1" } });
      fireEvent.click(screen.getByRole("button", { name: "Save prices" }));
      const alert = screen.getByRole("alert");
      expect(alert.textContent).toBe('Prices for "opus" must be numbers from 0 to 10,000.');
      // The bad cell points at the message; the others are left alone.
      const output = screen.getByLabelText("Row 1 output price");
      expect(output.getAttribute("aria-invalid")).toBe("true");
      expect(output.getAttribute("aria-describedby")).toBe(alert.id);
      expect(screen.getByLabelText("Row 1 input price").getAttribute("aria-invalid")).toBeNull();
      fireEvent.change(screen.getByLabelText("Row 1 output price"), { target: { value: "" } });
      fireEvent.click(screen.getByRole("button", { name: "Save prices" }));
      expect(screen.getByRole("alert").textContent).toContain("must be numbers");
      fireEvent.change(screen.getByLabelText("Row 1 output price"), { target: { value: "10000.5" } });
      fireEvent.click(screen.getByRole("button", { name: "Save prices" }));
      expect(screen.getByRole("alert").textContent).toContain("from 0 to 10,000");
      fireEvent.change(screen.getByLabelText("Row 1 output price"), { target: { value: "25" } });
      fireEvent.change(screen.getByLabelText("Row 1 model match"), { target: { value: "  " } });
      fireEvent.click(screen.getByRole("button", { name: "Save prices" }));
      expect(screen.getByRole("alert").textContent).toBe("Row 1 needs a model to match.");
      expect(screen.getByLabelText("Row 1 model match").getAttribute("aria-invalid")).toBe("true");
      fireEvent.change(screen.getByLabelText("Row 1 model match"), { target: { value: "x".repeat(101) } });
      fireEvent.click(screen.getByRole("button", { name: "Save prices" }));
      expect(screen.getByRole("alert").textContent).toBe("Row 1: a model match can be at most 100 characters.");
      expect(props.onChange).not.toHaveBeenCalled();
    });

    it("removes rows, resets to the defaults and discards a draft", async () => {
      const { props } = await setup({ settings: ON });
      fireEvent.click(screen.getByRole("button", { name: "Remove row 1 (opus)" }));
      expect(screen.getAllByRole("row")).toHaveLength(1);
      fireEvent.click(screen.getByRole("button", { name: "Discard" }));
      expect(screen.getAllByRole("row")).toHaveLength(2);
      fireEvent.click(screen.getByRole("button", { name: "Reset to defaults" }));
      expect(screen.getAllByRole("row")).toHaveLength(DEFAULT_PRICES.length + 1);
      await act(async () => {
        fireEvent.click(screen.getByRole("button", { name: "Save prices" }));
      });
      expect(props.onChange).toHaveBeenCalledWith({
        ...ON,
        costEstimate: { enabled: true, prices: DEFAULT_PRICES },
      });
    });

    it("keeps edits made while a save is under way", async () => {
      let finish!: () => void;
      const onChange = vi.fn(() => new Promise<void>((resolve) => (finish = resolve)));
      await setup({ settings: ON, onChange });
      fireEvent.change(screen.getByLabelText("Row 1 input price"), { target: { value: "6" } });
      fireEvent.click(screen.getByRole("button", { name: "Save prices" }));
      expect(onChange).toHaveBeenCalledOnce();
      // Typed before the save answers.
      fireEvent.change(screen.getByLabelText("Row 1 input price"), { target: { value: "7" } });
      await act(async () => finish());
      expect((screen.getByLabelText("Row 1 input price") as HTMLInputElement).value).toBe("7");
      expect(screen.getByRole("button", { name: "Save prices" })).toBeTruthy();
    });

    it("warns about a row an earlier, shorter match hides", async () => {
      await setup({ settings: ON });
      expect(screen.queryByText(/is never used/)).toBeNull();
      fireEvent.click(screen.getByRole("button", { name: "Add row" }));
      fireEvent.change(screen.getByLabelText("Row 2 model match"), { target: { value: "opus-5-5" } });
      expect(screen.getByText('Row 2 is never used: row 1 ("opus") matches first.')).toBeTruthy();
    });

    it("keeps the draft and says why when saving fails", async () => {
      const onChange = vi.fn().mockRejectedValue({ kind: "invalid", message: "The price table can have at most 50 rows." });
      await setup({ settings: ON, onChange });
      fireEvent.change(screen.getByLabelText("Row 1 input price"), { target: { value: "6" } });
      await act(async () => {
        fireEvent.click(screen.getByRole("button", { name: "Save prices" }));
      });
      expect(screen.getByRole("alert").textContent).toContain("at most 50 rows");
      expect((screen.getByLabelText("Row 1 input price") as HTMLInputElement).value).toBe("6");
    });
  });
});
