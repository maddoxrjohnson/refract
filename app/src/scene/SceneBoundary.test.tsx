import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { SceneBoundary } from "./SceneBoundary";

afterEach(cleanup);

function Boom(): never {
  throw new Error("Failed to fetch dynamically imported module");
}

describe("SceneBoundary", () => {
  it("renders its children when nothing fails", () => {
    render(
      <SceneBoundary>
        <p>scene</p>
      </SceneBoundary>,
    );
    expect(screen.getByText("scene")).toBeTruthy();
  });

  it("shows the scene-fail message in place of a child that throws, and keeps its siblings", () => {
    const log = vi.spyOn(console, "error").mockImplementation(() => {});
    const { container } = render(
      <main>
        <SceneBoundary>
          <Boom />
        </SceneBoundary>
        <p>overlay</p>
      </main>,
    );
    expect(container.querySelector(".scene-fail")?.textContent).toMatch(/could not be loaded/);
    expect(screen.getByText("overlay")).toBeTruthy();
    log.mockRestore();
  });
});
