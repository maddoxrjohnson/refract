// Makes sure src-tauri/resources/refract-agent exists. The app bundles it
// (tauri.conf.json) and installs it into WSL on first run. The release workflow puts
// the Linux build of refract-agent there; elsewhere an empty placeholder is enough to
// build, and the app then uses the helper already installed (or REFRACT_AGENT).
// To bundle a local build: cargo build --release -p refract-agent, then copy
// target/release/refract-agent over the placeholder.
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const resources = join(dirname(fileURLToPath(import.meta.url)), "..", "src-tauri", "resources");
const helper = join(resources, "refract-agent");

if (!existsSync(helper)) {
  console.log(`ensure-helper: ${helper} is missing; writing an empty placeholder`);
  mkdirSync(resources, { recursive: true });
  writeFileSync(helper, "");
}
