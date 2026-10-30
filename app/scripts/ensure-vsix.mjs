// Packages vscode-extension/refract.vsix if it is missing. The app bundles it
// (tauri.conf.json), and it is not committed, so a fresh clone has to build it first.
import { execSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const extension = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "vscode-extension");
const vsix = join(extension, "refract.vsix");

if (!existsSync(vsix)) {
  console.log(`ensure-vsix: ${vsix} is missing; packaging it`);
  const run = (command) => execSync(command, { cwd: extension, stdio: "inherit" });
  if (!existsSync(join(extension, "node_modules"))) run("npm ci");
  run("npm run package");
}
