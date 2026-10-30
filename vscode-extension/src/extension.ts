import * as fs from "node:fs";
import * as os from "node:os";
import * as vscode from "vscode";
import { claimRequests, promptLate, release, Scanner, type ClaimedRequest, type ClaimFs } from "./claim";
import { FOLDER_VAR, findTerminal } from "./focus";
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

  // Every terminal this window starts names its folder, so the helper can tell which
  // Refract window a running agent is in (see docs/launch.md).
  context.environmentVariableCollection.description = "Names this folder for Refract's Go to the running agent";
  context.environmentVariableCollection.replace(FOLDER_VAR, folder);

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

  // Shows the terminal already running the session. Never creates a terminal and
  // never applies the bare layout; `code` has already brought the window forward.
  async function focusTerminal(claimed: ClaimedRequest): Promise<void> {
    const { request } = claimed;
    const pids = request.focus?.pids ?? [];
    const terminal = await findTerminal(vscode.window.terminals, (t) => t.processId, pids);
    if (terminal) {
      terminal.show();
      log(`showed the terminal of ${request.focus?.sessionId} for ${request.requestId}`);
    } else {
      log(`no terminal runs ${request.focus?.sessionId} (pids ${pids.join(", ")}) for ${request.requestId}`);
    }
    await release(nodeFs, claimed, log);
  }

  // A late request waits for an answer, so the prompt must not hold up the scan
  // queue: a fresh request that arrives meanwhile still starts at once.
  async function handle(claimed: ClaimedRequest, first: boolean): Promise<void> {
    if (claimed.request.focus) {
      return focusTerminal(claimed);
    }
    if (!claimed.late) {
      return start(claimed, first);
    }
    void promptLate(claimed, {
      showMessage: (message, ...buttons) => Promise.resolve(vscode.window.showInformationMessage(message, ...buttons)),
      start: (c) => start(c, first),
      fs: nodeFs,
      log,
    });
  }

  const scanner = new Scanner(
    () => claimRequests(nodeFs, dir, folder, Date.now(), log),
    handle,
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
