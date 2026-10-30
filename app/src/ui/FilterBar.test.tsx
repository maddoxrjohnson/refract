import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { FilterBar } from "./FilterBar";

afterEach(cleanup);

describe("FilterBar", () => {
  it("renders nothing without a filter", () => {
    const { container } = render(<FilterBar filter="" matches={14} total={14} onClear={() => {}} />);
    expect(container.innerHTML).toBe("");
  });

  it("shows the typed text and the match count", () => {
    render(<FilterBar filter="tide" matches={2} total={14} onClear={() => {}} />);
    expect(screen.getByRole("search").textContent).toContain("tide");
    expect(screen.getByRole("status").textContent).toBe("2 of 14");
  });

  it("says when nothing matches, and clears", () => {
    const onClear = vi.fn();
    render(<FilterBar filter="zzz" matches={0} total={14} onClear={onClear} />);
    expect(screen.getByRole("status").textContent).toBe("No matches");
    fireEvent.click(screen.getByRole("button", { name: /Clear/ }));
    expect(onClear).toHaveBeenCalledOnce();
  });
});
