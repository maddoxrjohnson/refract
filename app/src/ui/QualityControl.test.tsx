import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  QUALITY_STORAGE_KEY,
  QualityControl,
  loadQualitySetting,
  saveQualitySetting,
} from "./QualityControl";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  localStorage.clear();
});

describe("QualityControl", () => {
  it("checks the current setting and shows the active level under Auto", () => {
    render(<QualityControl setting="auto" level="high" onChange={() => {}} />);
    expect((screen.getByRole("radio", { name: "Auto" }) as HTMLInputElement).checked).toBe(true);
    expect(screen.getByRole("status").textContent).toBe("Drawing at High");
  });

  it("hides the active level for a fixed setting", () => {
    render(<QualityControl setting="low" level="low" onChange={() => {}} />);
    expect((screen.getByRole("radio", { name: "Low" }) as HTMLInputElement).checked).toBe(true);
    expect(screen.getByRole("status").textContent).toBe("");
  });

  it("saves and reports a new choice", () => {
    const onChange = vi.fn();
    render(<QualityControl setting="auto" level="medium" onChange={onChange} />);
    fireEvent.click(screen.getByRole("radio", { name: "High" }));
    expect(onChange).toHaveBeenCalledWith("high");
    expect(localStorage.getItem(QUALITY_STORAGE_KEY)).toBe("high");
    expect(loadQualitySetting()).toBe("high");
  });
});

describe("quality setting storage", () => {
  it("defaults to auto when nothing or something unknown is saved", () => {
    expect(loadQualitySetting()).toBe("auto");
    localStorage.setItem(QUALITY_STORAGE_KEY, "ultra");
    expect(loadQualitySetting()).toBe("auto");
  });

  it("survives storage that throws", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    expect(() => saveQualitySetting("low")).not.toThrow();
    expect(loadQualitySetting()).toBe("auto");
  });
});
