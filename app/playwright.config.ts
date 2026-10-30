import { defineConfig, devices } from "@playwright/test";

// Browser tests of the public web demo (spec §9). `npm run test:e2e` builds it first;
// the tests run against `vite preview` of `dist-demo`. SwiftShader renders WebGL in
// software, so the scene draws on CI machines without a GPU.
export default defineConfig({
  testDir: "e2e",
  timeout: 60_000,
  // SwiftShader runs GPU work on the CPU, and Chromium flushes it all about a second after
  // load, which pauses the page for a second or two on a small runner. Allow for that.
  expect: { timeout: 15_000 },
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: [["list"]],
  use: {
    baseURL: "http://127.0.0.1:4173/",
    viewport: { width: 1280, height: 760 },
    trace: "retain-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: {
        ...devices["Desktop Chrome"],
        viewport: { width: 1280, height: 760 },
        launchOptions: { args: ["--use-angle=swiftshader", "--enable-unsafe-swiftshader"] },
      },
    },
  ],
  webServer: {
    command: "npm run preview:demo",
    url: "http://127.0.0.1:4173/",
    reuseExistingServer: false,
    timeout: 60_000,
  },
});
