import { expect, test, type Page } from "@playwright/test";

// Browser tests of the public web demo: the real interface over DemoBackend's invented
// repos, with launches shown in a pretend terminal (spec §8, §9).

let errors: string[] = [];

test.beforeEach(async ({ page }) => {
  errors = [];
  page.on("console", (msg) => {
    if (msg.type() === "error") errors.push(msg.text());
  });
  page.on("pageerror", (err) => errors.push(err.message));
  await page.goto("./");
  await expect(page.locator(".scene canvas")).toBeVisible();
  // The scene has drawn: the gems are listed and the usage widget has numbers.
  await expect(page.locator(".rf-widget--usage")).toBeVisible();
});

test.afterEach(() => {
  expect(errors, "console errors").toEqual([]);
});

/** Moves the pointer over the middle of the scene, where the heaviest gems sit. */
async function hoverCentre(page: Page) {
  const { width, height } = page.viewportSize()!;
  // Two moves, so the scene sees real pointer movement inside the canvas.
  await page.mouse.move(width / 2 - 40, height / 2 - 20);
  await page.mouse.move(width / 2, height / 2, { steps: 4 });
  const label = page.locator(".rf-focus-label");
  await expect(label).toBeVisible();
  return (await label.locator(".rf-focus-label__name").textContent())!.trim();
}

test("renders the scene without errors", async ({ page }) => {
  // The WebGL fallback is DOM content inside the canvas, shown only when WebGL fails.
  await expect(page.locator(".scene-fail")).toBeHidden();
  const box = await page.locator(".scene canvas").boundingBox();
  expect(box!.width).toBeGreaterThan(600);
  await expect(page.getByText("Demo with invented repos")).toBeVisible();
});

test("magnet focus shows a label for the gem near the pointer", async ({ page }) => {
  const name = await hoverCentre(page);
  expect(name.length).toBeGreaterThan(0);
  await expect(page.locator(".rf-focus-label .rf-state")).toBeVisible();
});

test("clicking a gem zooms in and opens the detail panel", async ({ page }) => {
  const name = await hoverCentre(page);
  await page.mouse.click(page.viewportSize()!.width / 2, page.viewportSize()!.height / 2);
  const panel = page.getByRole("complementary", { name: `${name} details` });
  await expect(panel).toBeVisible();
  await expect(panel.getByText("Last commit")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(panel).toBeHidden();
});

test("typing filters the gems", async ({ page }) => {
  await page.keyboard.type("led");
  const filter = page.getByRole("search", { name: "Filter" });
  await expect(filter).toBeVisible();
  await expect(filter.getByText("1 of")).toBeVisible();
  await expect(page.locator(".rf-focus-label__name")).toHaveText("ledger-sync");
  await page.keyboard.press("Escape");
  await expect(filter).toBeHidden();
});

test("Enter launches into the simulated terminal", async ({ page }) => {
  // atlas-api has no unfinished session, so the second Enter launches straight away.
  await page.keyboard.type("atlas");
  await page.keyboard.press("Enter");
  await expect(page.getByRole("complementary", { name: "atlas-api details" })).toBeVisible();
  await page.keyboard.press("Enter");
  const terminal = page.getByRole("dialog", { name: "Simulated terminal for atlas-api" });
  await expect(terminal).toBeVisible();
  await expect(terminal.getByTestId("demo-terminal")).toContainText(/claude --session-id [0-9a-f-]{36}/);
  await expect(terminal.getByTestId("demo-terminal")).toContainText("What should we work on?");
  await expect(terminal.getByText("Demo: nothing is launched")).toBeVisible();
  // Escape closes the terminal only; the gem stays zoomed.
  await page.keyboard.press("Escape");
  await expect(terminal).toBeHidden();
  await expect(page.getByRole("complementary", { name: "atlas-api details" })).toBeVisible();
});

test("a repo with unfinished sessions offers the picker, preselecting Resume", async ({ page }) => {
  await page.keyboard.type("tidepool");
  await page.keyboard.press("Enter");
  await expect(page.getByRole("complementary", { name: "tidepool details" })).toBeVisible();
  await page.keyboard.press("Enter");
  const picker = page.getByRole("dialog", { name: "Start an agent in tidepool" });
  await expect(picker).toBeVisible();
  const options = picker.getByRole("option");
  await expect(options).toHaveCount(3);
  await expect(options.first()).toHaveAttribute("aria-selected", "true");
  await page.keyboard.press("Enter");
  const terminal = page.getByRole("dialog", { name: "Simulated terminal for tidepool" });
  await expect(terminal.getByTestId("demo-terminal")).toContainText(
    "claude --resume 5f0c2a1e-0000-4000-8000-000000000002",
  );
  await terminal.getByRole("button", { name: "Close terminal (Escape)" }).click();
  await expect(terminal).toBeHidden();
});

test("the settings panel opens and closes", async ({ page }) => {
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  const settings = page.getByRole("dialog", { name: "Settings", exact: true });
  await expect(settings).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(settings).toBeHidden();
});
