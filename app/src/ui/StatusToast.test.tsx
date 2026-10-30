import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { StatusToast, describeError } from "./StatusToast";

afterEach(cleanup);

describe("StatusToast", () => {
  it("renders nothing while the helper runs and no launch is in progress", () => {
    const { container } = render(<StatusToast agent={{ state: "running" }} launch={null} />);
    expect(container.innerHTML).toBe("");
  });

  it("shows the helper connecting and restarting", () => {
    const { rerender } = render(<StatusToast agent={null} launch={null} />);
    expect(screen.getByRole("status").textContent).toBe("Agent: connecting…");
    rerender(
      <StatusToast
        agent={{ state: "restarting", attempt: 2, retryInMs: 3000, lastError: "pipe closed" }}
        launch={null}
      />,
    );
    expect(screen.getByRole("status").textContent).toBe(
      "Agent: restarting (attempt 2, retry in 3 s): pipe closed",
    );
  });

  it("shows launch progress with the shell's wording", () => {
    const { rerender } = render(
      <StatusToast agent={{ state: "running" }} launch={{ repoName: "tidepool", step: "starting" }} />,
    );
    expect(screen.getByRole("status").textContent).toBe("tidepool starting…");
    rerender(<StatusToast agent={{ state: "running" }} launch={{ repoName: "tidepool", step: "server" }} />);
    expect(screen.getByRole("status").textContent).toBe("tidepool installing the extension in WSL…");
    rerender(<StatusToast agent={{ state: "running" }} launch={{ repoName: "tidepool", step: "done" }} />);
    expect(screen.getByRole("status").textContent).toBe("tidepool opened");
  });

  it("announces a failed launch as an alert and can be dismissed", () => {
    const onDismiss = vi.fn();
    render(
      <StatusToast
        agent={{ state: "running" }}
        launch={{ repoName: "tidepool", step: "failed", message: "VS Code was not found" }}
        onDismiss={onDismiss}
      />,
    );
    expect(screen.getByRole("alert").textContent).toContain("tidepool VS Code was not found");
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(onDismiss).toHaveBeenCalledOnce();
  });
});

describe("describeError", () => {
  it("reads strings, agent errors and launch errors", () => {
    expect(describeError("boom")).toBe("boom");
    expect(describeError({ kind: "agent", code: 7, message: "no repo" })).toBe("no repo (7)");
    expect(describeError({ kind: "vscodeNotFound", message: "VS Code was not found" })).toBe(
      "VS Code was not found",
    );
    expect(describeError({ kind: "timeout" })).toBe("timeout");
  });
});
