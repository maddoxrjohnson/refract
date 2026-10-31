// Records the README's demo loop from the web demo build, frame by frame.
//
//   npm run record:demo        (builds dist-demo, records, then runs make-gif.py)
//
// Software WebGL is far too slow to film in real time, so the page runs on Playwright's
// fake clock: each frame advances time by exactly 1/FPS s and then takes a screenshot.
// Frames go to test-results/demo-frames with frames.json (pointer position per frame);
// scripts/make-gif.py draws the pointer and assembles docs/media/demo.gif. It also saves
// the still docs/media/scene.png. Only DemoBackend's invented repos appear.
//
// RECORD_CHROMIUM_ARGS overrides the browser flags, for example
// "--use-angle=vulkan --enable-features=Vulkan" where Mesa's Vulkan driver is faster.

import { spawn } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "@playwright/test";

const app = join(dirname(fileURLToPath(import.meta.url)), "..");
const framesDir = join(app, "test-results", "demo-frames");
const media = join(app, "..", "docs", "media");
const URL = "http://127.0.0.1:4173/";
const FPS = 12;
const STEP_MS = 1000 / FPS;
const W = 1000;
const H = 600;
const args = (process.env.RECORD_CHROMIUM_ARGS ?? "--use-angle=swiftshader --enable-unsafe-swiftshader")
  .split(" ")
  .filter(Boolean);

/** Seconds of the opening burst before the pointer moves; the loop starts on the burst. */
const LEAD_S = 2.4;

const ease = (t) => (t < 0.5 ? 2 * t * t : 1 - (-2 * t + 2) ** 2 / 2);

/** Pointer waypoints [seconds after the lead, x, y]; the pointer glides between them. */
const PATH = [
  [0, 960, 560],
  [0.6, 880, 470],
  [1.6, 700, 225],
  [2.5, 560, 270],
  [3.4, 395, 235],
  [4.6, 395, 235],
  [5.6, 880, 520],
  [6.6, 985, 590],
];

/** What happens when, in seconds after the lead. */
const ACTIONS = [
  [3.8, "click"],
  [6.4, "Escape"],
  [7.5, "type:atlas"],
  [9.3, "Enter"],
  [10.4, "Enter"],
];
const END_S = LEAD_S + 15.6;

function pointerAt(t) {
  t -= LEAD_S;
  for (let i = 1; i < PATH.length; i++) {
    const [t1, x1, y1] = PATH[i];
    const [t0, x0, y0] = PATH[i - 1];
    if (t <= t1) {
      const k = ease(Math.min(1, Math.max(0, (t - t0) / (t1 - t0))));
      return [x0 + (x1 - x0) * k, y0 + (y1 - y0) * k];
    }
  }
  return PATH[PATH.length - 1].slice(1);
}

async function waitForServer() {
  for (let i = 0; i < 100; i++) {
    try {
      if ((await fetch(URL)).ok) return;
    } catch {
      // not up yet
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`no server at ${URL}`);
}

/**
 * A page on the fake clock, with the first-run hook offer already turned down, `settleMs`
 * after it loads (enough for the opening to finish, or just for the repos to arrive).
 */
async function openDemo(browser, viewport, settleMs = 2500) {
  const page = await browser.newPage({ viewport, deviceScaleFactor: 1 });
  await page.addInitScript(() => {
    localStorage.setItem("refract.hookOfferDismissed", "1");
    localStorage.setItem("refract.quality", "medium");
  });
  await page.clock.install();
  await page.goto(URL);
  await page.locator(".scene canvas").waitFor();
  await page.clock.runFor(settleMs);
  return page;
}

const server = spawn("npm", ["run", "preview:demo"], { cwd: app, stdio: "ignore", detached: true });
try {
  await waitForServer();
  const browser = await chromium.launch({ args });

  rmSync(framesDir, { recursive: true, force: true });
  mkdirSync(framesDir, { recursive: true });
  const page = await openDemo(browser, { width: W, height: H }, 100);
  const frames = [];
  const pending = ACTIONS.map(([at, action]) => [at + LEAD_S, action]);
  let typing = [];
  let clickFrame = -1;
  const total = Math.round(END_S * FPS);
  for (let f = 0; f < total; f++) {
    const t = f / FPS;
    const [x, y] = pointerAt(t);
    await page.mouse.move(x, y);
    while (pending.length && pending[0][0] <= t) {
      const [, action] = pending.shift();
      if (action === "click") {
        await page.mouse.click(x, y);
        clickFrame = f;
      } else if (action.startsWith("type:")) typing = [...action.slice(5)];
      else await page.keyboard.press(action);
    }
    // One character every third frame, like steady typing.
    if (typing.length && f % 3 === 0) await page.keyboard.type(typing.shift());
    await page.clock.runFor(STEP_MS);
    const file = `${String(f).padStart(4, "0")}.png`;
    await page.screenshot({ path: join(framesDir, file), timeout: 120_000 });
    frames.push({ file, x: Math.round(x), y: Math.round(y), click: clickFrame >= 0 && f - clickFrame < 4 });
    if (f % 24 === 0) console.log(`frame ${f}/${total}`);
  }
  writeFileSync(join(framesDir, "frames.json"), JSON.stringify({ fps: FPS, frames }, null, 1));
  await page.close();

  // The still: the whole scene with one stone in magnet focus.
  mkdirSync(media, { recursive: true });
  const still = await openDemo(browser, { width: 1600, height: 900 }, 4000);
  await still.mouse.move(1500, 800);
  await still.mouse.move(800, 430, { steps: 5 });
  await still.clock.runFor(1500);
  await still.screenshot({ path: join(media, "scene.png") });
  await browser.close();
  console.log(`recorded ${total} frames to ${framesDir}`);
} finally {
  process.kill(-server.pid);
}
