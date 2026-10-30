/// <reference types="vitest/config" />
import { configDefaults, defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// The WebView2 window on Windows reaches this dev server through WSL's localhost
// forwarding, so it binds to 127.0.0.1 on a fixed port that tauri.conf.json names.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    host: "127.0.0.1",
    port: 1420,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**"] },
  },
  build: {
    target: "es2022",
    // The lazily loaded scene chunk is mostly three.js (about 745 kB on its own) and loads
    // from local disk in the app, so a split would only add requests.
    chunkSizeWarningLimit: 1100,
  },
  test: {
    environment: "jsdom",
    // Playwright's browser tests of the demo build run with `npm run test:e2e`.
    exclude: [...configDefaults.exclude, "e2e/**"],
  },
});
