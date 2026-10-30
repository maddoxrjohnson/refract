// Finding the terminal a focus request names. Must not import `vscode`.

/** The variable set on every terminal of a Refract window, holding its folder. */
export const FOLDER_VAR = "REFRACT_FOLDER";

/** How long one terminal gets to report its process id. */
export const PROCESS_ID_TIMEOUT_MS = 1000;

/** `value`, or undefined once `ms` have passed or if it rejects. */
function within<T>(value: PromiseLike<T>, ms: number): Promise<T | undefined> {
  return new Promise((resolve) => {
    const timer = setTimeout(() => resolve(undefined), ms);
    Promise.resolve(value).then(
      (v) => {
        clearTimeout(timer);
        resolve(v);
      },
      () => {
        clearTimeout(timer);
        resolve(undefined);
      },
    );
  });
}

/**
 * The first of `terminals` whose shell process is one of `pids` (the session's
 * `claude` process and its ancestors), or undefined. Every process id is asked for
 * at once and each gets `timeoutMs`, so one terminal that never answers cannot
 * hold things up; one that times out, fails or has no process id is no match.
 */
export async function findTerminal<T>(
  terminals: readonly T[],
  processId: (terminal: T) => PromiseLike<number | undefined>,
  pids: readonly number[],
  timeoutMs = PROCESS_ID_TIMEOUT_MS,
): Promise<T | undefined> {
  const wanted = new Set(pids);
  const found = await Promise.all(
    terminals.map((terminal) => {
      let asked: PromiseLike<number | undefined>;
      try {
        asked = processId(terminal);
      } catch {
        return Promise.resolve(undefined);
      }
      return within(asked, timeoutMs);
    }),
  );
  const index = found.findIndex((pid) => pid !== undefined && wanted.has(pid));
  return index >= 0 ? terminals[index] : undefined;
}
