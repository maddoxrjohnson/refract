import * as fs from "node:fs";
import * as os from "node:os";
import * as vscode from "vscode";
import { claimRequests, release, Scanner, type ClaimedRequest, type ClaimFs } from "./claim";
import { tabsToClose } from "./layout";
import { launchDir, terminalCommand } from "./request";

const nodeFs: ClaimFs = {
  readdir: (dir) => fs.promises.readdir(dir),
  readFile: (file) => fs.promises.readFile(file, "utf8"),
  rename: (from, to) => fs.promises.rename(from, to),
  unlink: (file) => fs.promises.unlink(file),
};

const BARE_LAYOUT_COMMANDS = [
  "workbench.action.closeSidebar",
  "workbench.action.closePanel",
  "workbench.action.closeAuxiliaryBar",
];

export function activate(context: vscode.ExtensionContext): void {
  const folders = vscode.workspace.workspaceFolders;
  if (!folders || folders.length !== 1) {
    return;
  }
  const folder = folders[0]!.uri.path;
  const dir = launchDir(process.env, os.homedir());

  const output = vscode.window.createOutputChannel("Refract");
  context.subscriptions.push(output);
  const log = (message: string) => output.appendLine(message);
  log(`watching ${dir} for workspace folder ${folder}`);

  async function runCommand(command: string): Promise<void> {
    try {
      await vscode.commands.executeCommand(command);
    } catch (err) {
      log(`command ${command} failed: ${String(err)}`);
    }
  }

  async function applyBareLayout(): Promise<void> {
    for (const command of BARE_LAYOUT_COMMANDS) {
      await runCommand(command);
    }
    try {
      const tabs = tabsToClose(
        vscode.window.tabGroups.all,
        (input) => input instanceof vscode.TabInputTerminal,
      );
      await vscode.window.tabGroups.close(tabs);
    } catch (err) {
      log(`closing other tabs failed: ${String(err)}`);
    }
  }

  let layoutApplied = false;

  async function start(claimed: ClaimedRequest, first: boolean): Promise<void> {
    const { request } = claimed;
    const { line, env } = terminalCommand(request.argv);
    const terminal = vscode.window.createTerminal({
      name: request.title,
      cwd: folder,
      env,
      location: vscode.TerminalLocation.Editor,
    });
    terminal.show();
    terminal.sendText(line);
    log(`started ${request.requestId}`);
    await release(nodeFs, claimed, log);
    if (first && !layoutApplied) {
      layoutApplied = true;
      await applyBareLayout();
    }
  }

  const scanner = new Scanner(
    () => claimRequests(nodeFs, dir, folder, Date.now(), log),
    start,
    log,
  );

  void scanner.scan(true);

  let timer: NodeJS.Timeout | undefined;
  const rescan = () => {
    clearTimeout(timer);
    timer = setTimeout(() => void scanner.scan(), 100);
  };

  try {
    fs.mkdirSync(dir, { recursive: true });
    const watcher = fs.watch(dir, rescan);
    watcher.on("error", (err) => log(`watcher error: ${String(err)}`));
    context.subscriptions.push({ dispose: () => watcher.close() });
  } catch (err) {
    log(`cannot watch ${dir}: ${String(err)}`);
  }

  context.subscriptions.push(
    vscode.window.onDidChangeWindowState((state) => {
      if (state.focused) {
        rescan();
      }
    }),
    { dispose: () => clearTimeout(timer) },
  );
}

export function deactivate(): void {}
