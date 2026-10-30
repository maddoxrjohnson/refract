import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { DemoBackend } from "./backend/DemoBackend";
import { TauriBackend } from "./backend/TauriBackend";
import "./styles.css";

// Outside the Tauri window (a plain browser on `npm run dev`), serve invented repos.
const backend = "__TAURI_INTERNALS__" in window ? new TauriBackend() : new DemoBackend();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App backend={backend} />
  </StrictMode>,
);
