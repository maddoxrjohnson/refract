// Bare-layout tab selection. Must not import `vscode`.

interface TabLike {
  input: unknown;
  isDirty: boolean;
}

/**
 * Tabs to close for the bare layout: every clean tab that is not a terminal,
 * in any group. Selecting by identity means a terminal that is not yet active
 * (or a dirty file that would prompt to save) is never touched.
 */
export function tabsToClose<T extends TabLike>(
  groups: readonly { tabs: readonly T[] }[],
  isTerminal: (input: unknown) => boolean,
): T[] {
  return groups.flatMap((g) => g.tabs.filter((t) => !isTerminal(t.input) && !t.isDirty));
}
