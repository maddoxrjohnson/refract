import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Backend } from "../backend/Backend";
import type { Settings } from "../backend/types";
import { SettingsPanel, type SettingsPanelProps } from "./SettingsPanel";

afterEach(cleanup);

const SETTINGS: Settings = {
  gitlabUrl: "https://gitlab.example.com",
  showForks: false,
  showArchived: false,
  hidden: ["remote:github.com/demo-user/orbit-cli", "/home/demo/projects/scratch"],
};

function fakeBackend() {
  return {
    credentials: vi.fn().mockResolvedValue({ github: "ghCli", gitlab: "missing" }),
    setToken: vi.fn().mockResolvedValue(undefined),
    addRepo: vi.fn().mockResolvedValue(undefined),
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
});
