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
  // Low draws only on change. Software WebGL on a small CI runner can't keep up with
  // Medium's constant motion, and the busy main thread then starves the demo's timers.
  await page.addInitScript(() => {
    try {
      localStorage.setItem("refract.quality", "low");
    } catch {
      // Storage may be unavailable; the test then runs at the default level.
    }
  });
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
  // The canvas starts at the default 300 px until R3F measures its container.
  const canvas = page.locator(".scene canvas");
  await expect.poll(async () => (await canvas.boundingBox())!.width).toBeGreaterThan(600);
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

test("a repo with an agent waiting on you preselects going to it", async ({ page }) => {
  await page.keyboard.type("tessera");
  await page.keyboard.press("Enter");
  await expect(page.getByRole("complementary", { name: "tessera details" })).toBeVisible();
  await page.keyboard.press("Enter");
  const picker = page.getByRole("dialog", { name: "Start an agent in tessera" });
  const options = picker.getByRole("option");
  await expect(options).toHaveCount(2);
  await expect(options.first()).toContainText("Go to the running agent");
  await expect(options.first()).toContainText("waiting on you");
  await expect(options.first()).toHaveAttribute("aria-selected", "true");
  await page.keyboard.press("Enter");
  const terminal = page.getByRole("dialog", { name: "Simulated terminal for tessera" });
  await expect(terminal.getByTestId("demo-terminal")).toContainText("Switched to the agent already running in tessera.");
  await expect(terminal.getByTestId("demo-terminal")).not.toContainText("claude --");
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

test("Appearance switches the panels between paper and ink", async ({ page }) => {
  await page.emulateMedia({ colorScheme: "light" });
  const background = () => page.evaluate(() => getComputedStyle(document.body).backgroundColor);
  await expect.poll(background).toBe("rgb(224, 227, 212)");
  await page.getByRole("button", { name: "Settings", exact: true }).click();
  const appearance = page.getByRole("radiogroup", { name: "Appearance" });
  await expect(appearance.getByRole("radio", { name: "System" })).toBeChecked();
  await appearance.getByRole("radio", { name: "Dark" }).check();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  await expect.poll(background).toBe("rgb(8, 16, 13)");
  // Back to System, which follows the browser's (Windows') mode.
  await appearance.getByRole("radio", { name: "System" }).check();
  await expect(page.locator("html")).not.toHaveAttribute("data-theme", /.*/);
  await expect.poll(background).toBe("rgb(224, 227, 212)");
  await page.emulateMedia({ colorScheme: "dark" });
  await expect.poll(background).toBe("rgb(8, 16, 13)");
});

test.describe("at 1280×800", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("the usage widget shows the cost estimate and it fits", async ({ page }) => {
    const widget = page.getByRole("region", { name: "Claude usage" });
    await expect(widget).toContainText("≈ estimated at API list prices");
    const costs = widget.locator(".rf-usage__cost");
    await expect(costs).toHaveCount(3);
    await expect(costs.first()).toHaveText(/^≈ \$[\d,.]+k?$/);
    // Every row stays on one line inside the widget.
    const fits = await widget.evaluate((el) => {
      const box = el.getBoundingClientRect();
      return [...el.querySelectorAll(".rf-usage__totals > div, .rf-usage__note, .rf-limit__line")].every((row) => {
        const r = row.getBoundingClientRect();
        return r.left >= box.left && r.right <= box.right && r.height < 22 && row.scrollWidth <= row.clientWidth;
      });
    });
    expect(fits).toBe(true);
    // The plan limits sit under the rows, one meter per window, and the widget stays
    // compact and clear of the system widget above it.
    await expect(widget.getByRole("meter", { name: "5-hour limit" })).toHaveAttribute(
      "aria-valuetext",
      // A reset after midnight is shown with its weekday.
      /^\d+% used, resets (\w{3} )?\d\d:\d\d$/,
    );
    await expect(widget.getByRole("meter", { name: "Weekly limit" })).toHaveAttribute(
      "aria-valuetext",
      /^\d+% used, resets \w{3} \d\d:\d\d$/,
    );
    const box = (await widget.boundingBox())!;
    const system = (await page.getByRole("region", { name: "System" }).boundingBox())!;
    expect(box.height).toBeLessThanOrEqual(260);
    expect(box.y).toBeGreaterThan(system.y + system.height + 100);
    expect(box.y + box.height).toBeLessThanOrEqual(800 - 16 + 0.5);
  });

  test("the cost estimate settings fit the dialog", async ({ page }) => {
    await page.getByRole("button", { name: "Settings", exact: true }).click();
    const settings = page.getByRole("dialog", { name: "Settings", exact: true });
    const section = settings.getByRole("region", { name: "Cost estimate" });
    await section.scrollIntoViewIfNeeded();
    await expect(section.getByLabel("Estimate cost at API prices")).toBeChecked();
    const table = section.getByRole("table");
    await expect(table).toBeVisible();
    await expect(section.getByLabel("Row 1 model match")).toHaveValue("fable-5-1");
    const sizes = await settings.evaluate((dialog) => {
      const body = dialog.querySelector(".rf-settings__body")!;
      const table = dialog.querySelector(".rf-prices__table")!;
      // The room for text inside an input, less the width of its value in its font.
      const ctx = document.createElement("canvas").getContext("2d")!;
      const spare = (selector: string) =>
        Math.min(
          ...[...dialog.querySelectorAll<HTMLInputElement>(selector)].map((el) => {
            const style = getComputedStyle(el);
            ctx.font = style.font;
            const room = el.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
            return room - ctx.measureText(el.value).width;
          }),
        );
      const remove = dialog.querySelector(".rf-prices__remove")!.getBoundingClientRect();
      return {
        bodyScroll: body.scrollWidth - body.clientWidth,
        tableRight: table.getBoundingClientRect().right,
        bodyRight: body.getBoundingClientRect().right,
        // Every default value: the matches (the longest is "opus-4-2025") and the prices.
        priceSpare: spare(".rf-prices__input[inputmode=decimal]"),
        matchSpare: spare(".rf-prices__input:not([inputmode])"),
        removeTarget: Math.min(remove.width, remove.height),
      };
    });
    expect(sizes.bodyScroll).toBe(0);
    expect(sizes.tableRight).toBeLessThanOrEqual(sizes.bodyRight);
    expect(sizes.priceSpare).toBeGreaterThanOrEqual(2);
    expect(sizes.matchSpare).toBeGreaterThanOrEqual(2);
    expect(sizes.removeTarget).toBeGreaterThanOrEqual(24);
    // Turning it off hides the table and the widget's estimate.
    await section.getByLabel("Estimate cost at API prices").click();
    await expect(table).toBeHidden();
    await page.keyboard.press("Escape");
    await expect(page.getByRole("region", { name: "Claude usage" })).not.toContainText("$");
  });
});

test.describe("Move to WSL at 1280×800", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("moves the demo's Windows repo through the report to the Recycle Bin", async ({ page }) => {
    test.setTimeout(120_000);
    await page.keyboard.type("marigold");
    // The focus label explains the gem's Windows-drive marker.
    await expect(page.locator(".rf-focus-label__drive")).toHaveText(
      "On a Windows drive: git and agents are slower here.",
    );
    await expect(page.locator(".rf-drive-mark")).toBeVisible();
    await page.keyboard.press("Enter");
    const panel = page.getByRole("complementary", { name: "marigold-site details" });
    await panel.getByRole("button", { name: "Move to WSL…" }).click();

    const dialog = page.getByRole("dialog", { name: "Move marigold-site to WSL" });
    await expect(dialog.getByLabel("Destination in WSL")).toHaveValue("/home/demo/projects/marigold-site");
    const start = dialog.getByRole("button", { name: "Start" });
    await expect(start).toBeDisabled();
    await dialog.getByRole("button", { name: /Close VS Code window/ }).click();
    await expect(start).toBeEnabled();
    await start.click();

    // The ring sits on the gem, clear of the dialog and the corner controls.
    await expect(page.locator(".rf-move-ring")).toBeVisible();
    await expect(dialog.getByText(/^Copying [\d.]+ \/ 1\.3 GB$/)).toBeVisible({ timeout: 30_000 });
    const ring = (await page.locator(".rf-move-ring").boundingBox())!;
    const box = (await dialog.boundingBox())!;
    expect(ring.x + ring.width).toBeLessThan(box.x);

    await expect(dialog.getByRole("button", { name: "Delete original" })).toBeVisible({ timeout: 60_000 });
    await expect(dialog.getByRole("button", { name: "Delete original" })).toBeFocused();
    await expect(dialog.locator(".rf-move-check--passed")).toHaveCount(5);
    await dialog.getByRole("button", { name: "Delete original" }).click();

    const done = page.getByRole("dialog", { name: "Moved marigold-site to WSL" });
    await expect(done.getByText("The original is in the Recycle Bin.")).toBeVisible({ timeout: 30_000 });
    await done.getByRole("button", { name: "Done" }).click();
    await expect(done).toBeHidden();
    // The zoom followed the gem to its new place, which has no Windows marker.
    await expect(page.getByRole("complementary", { name: "marigold-site details" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Move to WSL…" })).toBeHidden();
  });

  test("a delete stopped halfway only offers Finish deleting, which finishes", async ({ page }) => {
    test.setTimeout(120_000);
    await page.goto("./?move=unfinished");
    await expect(page.locator(".rf-widget--usage")).toBeVisible();
    await page.keyboard.type("marigold");
    await page.keyboard.press("Enter");
    const panel = page.getByRole("complementary", { name: "marigold-site details" });
    await panel.getByRole("button", { name: "Move to WSL…" }).click();
    const dialog = page.getByRole("dialog", { name: "Move marigold-site to WSL" });
    await dialog.getByRole("button", { name: /Close VS Code window/ }).click();
    await dialog.getByRole("button", { name: "Start" }).click();
    const remove = dialog.getByRole("button", { name: "Delete original" });
    await expect(remove).toBeFocused({ timeout: 60_000 });
    await remove.click();

    const finish = dialog.getByRole("button", { name: "Finish deleting" });
    await expect(finish).toBeFocused({ timeout: 30_000 });
    await expect(dialog.getByText("Deleting the original didn't finish.")).toBeVisible();
    for (const name of ["Keep both", "Discard the copy", "Re-sync"]) {
      await expect(dialog.getByRole("button", { name })).toHaveCount(0);
    }
    // Closed and opened again, it still only offers finishing.
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
    await panel.getByRole("button", { name: "Finish deleting…" }).click();
    await finish.click();
    const done = page.getByRole("dialog", { name: "Moved marigold-site to WSL" });
    await expect(done.getByText("The original is in the Recycle Bin.")).toBeVisible({ timeout: 30_000 });
  });
});

test.describe("Move to WSL stopped once the copy is placed", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("keeps the copy and shows a report that says the install was stopped", async ({ page }) => {
    test.setTimeout(120_000);
    await page.keyboard.type("marigold");
    await page.keyboard.press("Enter");
    const panel = page.getByRole("complementary", { name: "marigold-site details" });
    await panel.getByRole("button", { name: "Move to WSL…" }).click();
    const dialog = page.getByRole("dialog", { name: "Move marigold-site to WSL" });
    await dialog.getByRole("button", { name: /Close VS Code window/ }).click();
    await dialog.getByRole("button", { name: "Start" }).click();
    await expect(dialog.getByText("Cancelling deletes the partial copy. The original is untouched.")).toBeVisible();
    const stop = dialog.getByRole("button", { name: "Stop and keep the copy" });
    await expect(stop).toBeVisible({ timeout: 60_000 });
    await expect(dialog.getByText(/deletes the partial copy/)).toHaveCount(0);
    await stop.click();
    await expect(dialog.getByRole("button", { name: "Delete original" })).toBeVisible({ timeout: 30_000 });
    await expect(dialog.getByText("Stopped before it finished")).toBeVisible();
    await expect(dialog.getByRole("button", { name: "Keep both" })).toBeVisible();
    await expect(dialog.getByRole("button", { name: "Discard the copy" })).toBeVisible();
  });
});

test.describe("at 1280×480", () => {
  test.use({ viewport: { width: 1280, height: 480 } });

  test("the usage widget's breakdown with the plan limits stays clear of the system widget", async ({ page }) => {
    const widget = page.getByRole("region", { name: "Claude usage" });
    await expect(widget.getByRole("meter", { name: "Weekly limit" })).toBeVisible();
    await widget.focus();
    await expect(widget.getByRole("list")).toBeVisible();
    const usage = (await widget.boundingBox())!;
    const system = (await page.getByRole("region", { name: "System" }).boundingBox())!;
    expect(usage.y).toBeGreaterThanOrEqual(system.y + system.height + 4);
  });
});
