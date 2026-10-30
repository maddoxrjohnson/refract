import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { DemoBackend } from "./backend/DemoBackend";
import { TauriBackend } from "./backend/TauriBackend";
import { DemoBadge, DemoTerminal } from "./ui/DemoTerminal";
import "./styles.css";

// The public web demo (`npm run build:demo`) always serves invented repos, as does a
// plain browser on `npm run dev`; only the Tauri window talks to the real helper.
const demo = import.meta.env.MODE === "demo" || !("__TAURI_INTERNALS__" in window);
const backend = demo ? new DemoBackend() : new TauriBackend();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App backend={backend} />
    {demo && (
      <>
        <DemoTerminal backend={backend} />
        <DemoBadge />
      </>
    )}
  </StrictMode>,
);
