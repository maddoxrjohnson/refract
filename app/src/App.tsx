import {
  lazy,
  Suspense,
  useCallback,
  useEffect,
  useMemo,
  useReducer,
  useRef,
  useState,
  type ReactNode,
} from "react";
import type { Backend } from "./backend/Backend";
import type {
  AgentStatus,
  LaunchMode,
  LaunchTicket,
  RemoteList,
  RemoteRef,
  RepoSnapshot,
  Settings,
  SystemStats,
  UsageSummary,
} from "./backend/types";
import { useHook } from "./hooks/useHook";
import { useMigrations } from "./hooks/useMigrations";
import { useRefactorNotices } from "./hooks/useRefactorNotices";
import { isWindowsDrive, stepText } from "./migrations";
import { useRemoteDetails } from "./hooks/useRemoteDetails";
import { useVisibility } from "./hooks/useVisibility";
import { hideKey, mergeProjects } from "./projects";
import { createQualityController } from "./quality/controller";
import type { QualityLevel, QualitySetting } from "./quality/tiers";
import { NOTICE_TEXT } from "./refactors";
import { initialRepoState, repoReducer } from "./repoState";
import { rememberedTheme, useThemeMode } from "./theme";
import { layoutGems } from "./scene/layout";
import { SceneBoundary } from "./scene/SceneBoundary";
import { DetailPanel } from "./ui/DetailPanel";
import { FilterBar } from "./ui/FilterBar";
import { FocusLabel } from "./ui/FocusLabel";
import { GemMark } from "./ui/GemMark";
import { MoveDialog } from "./ui/MoveDialog";
import { MoveSection } from "./ui/MoveSection";
import { HookOffer, loadHookOfferDismissed, saveHookOfferDismissed } from "./ui/HookOffer";
import {
  initialNavState,
  matchingPaths,
  navigate,
  newAgentMode,
  type NavContext,
  type NavEffect,
  type NavInput,
  type NavState,
} from "./ui/navigation";
import { Picker } from "./ui/Picker";
import { RefactorDialog } from "./ui/RefactorDialog";
import { RefactorSection } from "./ui/RefactorSection";
import { loadQualitySetting, QualityControl } from "./ui/QualityControl";
import { SettingsPanel } from "./ui/SettingsPanel";
import { describeError, StatusToast, type LaunchProgress } from "./ui/StatusToast";
import { SystemWidget } from "./ui/SystemWidget";
import { UsageWidget } from "./ui/UsageWidget";
import { DEFAULT_PRICES } from "./usage/cost";

// The scene (three.js and its effects, most of the bundle) loads in its own chunk, so the
// overlays and the data requests start without waiting for it. A chunk that fails to load
// is asked for once more before SceneBoundary gives up on it.
const loadScene = () => import("./scene/Scene").then((m) => ({ default: m.Scene }));
const Scene = lazy(() => loadScene().catch(loadScene));

/** How long a finished launch stays on screen. */
const LAUNCH_TOAST_MS = 4000;
/** How long a failed Open folder or Open on GitHub message stays. */
const OPEN_ERROR_MS = 6000;
/** How often the usage widget asks for totals while the window is shown. */
const USAGE_POLL_MS = 2000;

/**
 * Until the saved settings arrive: nothing hidden, forks and archived repos left out, no
 * cost estimate.
 */
const DEFAULT_SETTINGS: Settings = {
  gitlabUrl: null,
  showForks: false,
  showArchived: false,
  hidden: [],
  costEstimate: { enabled: false, prices: [...DEFAULT_PRICES] },
  theme: "system",
};

/** The host name (no port) of a GitLab address, or `null` when it does not parse. */
function hostOf(url: string): string | null {
  try {
    return new URL(url).hostname.toLowerCase();
  } catch {
    return null;
  }
}

/** Follows the system's reduced-motion setting. */
function useReducedMotion(): boolean {
  const [reduced, setReduced] = useState(
    () => window.matchMedia("(prefers-reduced-motion: reduce)").matches,
  );
  useEffect(() => {
    const mq = window.matchMedia("(prefers-reduced-motion: reduce)");
    const change = () => setReduced(mq.matches);
    mq.addEventListener("change", change);
    return () => mq.removeEventListener("change", change);
  }, []);
  return reduced;
}

/**
 * The active graphics level: the user's setting, or in auto what the controller picks
 * from frame times and other programs' GPU use.
 */
function useQuality(backend: Backend) {
  const [initial] = useState(() => createQualityController(loadQualitySetting()));
  const controller = useRef(initial);
  const [setting, setSettingState] = useState<QualitySetting>(controller.current.setting);
  const [level, setLevel] = useState<QualityLevel>(controller.current.level);

  useEffect(
    () =>
      backend.onGpuUsage((usage) =>
        setLevel(controller.current.gpu(performance.now(), usage)),
      ),
    [backend],
  );

  // Called between frames of continuous animation; setLevel bails out when the level is unchanged.
  const onFrameInterval = useCallback(
    (ms: number) => setLevel(controller.current.frame(performance.now(), ms)),
    [],
  );
  const setSetting = useCallback((next: QualitySetting) => {
    setSettingState(next);
    setLevel(controller.current.setSetting(next));
  }, []);
  return { setting, level, setSetting, onFrameInterval };
}

/**
 * The widgets' numbers (spec §3.5): system stats as the backend sends them and usage
 * asked for every 2 s, both only while the window is shown.
 */
function useWidgets(backend: Backend, visible: boolean, running: boolean) {
  const [stats, setStats] = useState<SystemStats | null>(null);
  const [usage, setUsage] = useState<UsageSummary | null>(null);

  useEffect(() => {
    if (!visible) return;
    return backend.onSystemStats(setStats);
  }, [backend, visible]);

  // The next request goes out once the last has answered, so slow answers never pile up.
  useEffect(() => {
    if (!visible || !running) return;
    let live = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = () => {
      backend
        .usage()
        .then(
          (u) => live && setUsage(u),
          (err: unknown) => console.warn("usage failed", err),
        )
        .finally(() => {
          if (live) timer = setTimeout(poll, USAGE_POLL_MS);
        });
    };
    poll();
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [backend, visible, running]);

  return { stats, usage };
}

export function App({ backend }: { backend: Backend }) {
  const visible = useVisibility(backend);
  const [status, setStatus] = useState<AgentStatus | null>(null);
  const [{ repos: localRepos, loading: reposLoading }, dispatch] = useReducer(repoReducer, initialRepoState);
  const [error, setError] = useState<string | null>(null);
  const [launch, setLaunch] = useState<LaunchProgress | null>(null);

  useEffect(
    () =>
      backend.onLaunchProgress((step) =>
        setLaunch((current) => current && current.step !== "failed" ? { ...current, step } : current),
      ),
    [backend],
  );

  // A finished launch fades after a moment; failures stay until dismissed.
  useEffect(() => {
    if (launch?.step !== "done") return;
    const timer = setTimeout(() => setLaunch(null), LAUNCH_TOAST_MS);
    return () => clearTimeout(timer);
  }, [launch]);

  useEffect(() => {
    let live = true;
    const stop = backend.onAgentStatus((s) => live && setStatus(s));
    backend.agentStatus().then(
      (s) => live && setStatus((current) => current ?? s),
      (err: unknown) => live && setError(describeError(err)),
    );
    return () => {
      live = false;
      stop();
    };
  }, [backend]);

  // Counts shows. WebView2 is suspended while hidden, so events can be held back or
  // lost; each show refetches.
  const [shown, setShown] = useState(0);
  useEffect(() => {
    if (visible) setShown((n) => n + 1);
  }, [visible]);

  // Refetch whenever the helper (re)enters running: a restarted helper starts empty.
  // `relist` asks again after a repo moved to WSL.
  const running = status?.state === "running";
  const [relist, setRelist] = useState(0);
  useEffect(() => {
    if (!running) return;
    let live = true;
    dispatch({ type: "fetchStarted" });
    backend.listRepos().then(
      (list) => {
        if (!live) return;
        dispatch({ type: "listed", list });
        setError(null);
      },
      (err: unknown) => live && setError(describeError(err)),
    );
    return () => {
      live = false;
    };
  }, [backend, running, shown, relist]);

  useEffect(() => backend.onAgentEvent((event) => dispatch({ type: "event", event })), [backend]);
  // Move to WSL (spec §2): pending reports are asked for again on each show.
  const moves = useMigrations(backend, running, shown);

  const widgets = useWidgets(backend, visible, running);
  // The status hook (spec §6.2): offered once, after the repos are in, until installed
  // or turned down.
  const hook = useHook(backend, running, shown);
  const [offerDismissed, setOfferDismissed] = useState(loadHookOfferDismissed);
  const dismissOffer = useCallback(() => {
    saveHookOfferDismissed();
    setOfferDismissed(true);
  }, []);
  // Once installed from anywhere (settings included), the first-run offer has done its job.
  const hookInstalled = hook.info?.installed === true;
  useEffect(() => {
    if (hookInstalled && !offerDismissed) dismissOffer();
  }, [hookInstalled, offerDismissed, dismissOffer]);

  // Settings and remote repos (spec §5), fetched on each show; Refresh skips the cache.
  const [settings, setSettings] = useState<Settings | null>(null);
  const settingsRef = useRef(settings);
  settingsRef.current = settings;
  const [remoteList, setRemoteList] = useState<RemoteList | null>(null);
  const [remotesLoading, setRemotesLoading] = useState(false);
  const remoteSeq = useRef(0);
  // Open, hide and remote failures show briefly; they don't block anything.
  const [openError, setOpenError] = useState<string | null>(null);

  const loadRemotes = useCallback(
    (refresh: boolean) => {
      const id = ++remoteSeq.current;
      setRemotesLoading(true);
      backend
        .listRemotes(refresh)
        .then(
          (list) => id === remoteSeq.current && setRemoteList(list),
          (err: unknown) => id === remoteSeq.current && setOpenError(describeError(err)),
        )
        .finally(() => id === remoteSeq.current && setRemotesLoading(false));
    },
    [backend],
  );

  useEffect(() => {
    if (shown === 0) return;
    let live = true;
    const queued = saveQueue.current;
    backend.getSettings().then(
      (s) => live && queued === saveQueue.current && setSettings(s),
      (err: unknown) => live && setOpenError(describeError(err)),
    );
    loadRemotes(false);
    return () => {
      live = false;
    };
  }, [backend, shown, loadRemotes]);

  /**
   * Saves settings, showing them at once; puts the old ones back if saving fails.
   * Saves run one after another, so the file ends up with the newest settings.
   */
  const saveQueue = useRef<Promise<unknown>>(Promise.resolve());
  const saveSettings = useCallback(
    (next: Settings) => {
      const previous = settingsRef.current;
      setSettings(next);
      const save = saveQueue.current
        .catch(() => {})
        .then(() => backend.setSettings(next))
        .catch((err: unknown) => {
          setSettings((current) => (current === next ? previous : current));
          throw err;
        });
      saveQueue.current = save;
      return save;
    },
    [backend],
  );

  const projects = useMemo(
    () => mergeProjects([...localRepos.values()], remoteList, settings ?? DEFAULT_SETTINGS),
    [localRepos, remoteList, settings],
  );
  const repoList = projects.repos;
  const repos = useMemo(() => new Map(repoList.map((r) => [r.path, r])), [repoList]);
  const positions = useMemo(() => layoutGems(repoList), [repoList]);
  const reducedMotion = useReducedMotion();
  const themeMode = useThemeMode(settings?.theme ?? rememberedTheme(), backend);
  const quality = useQuality(backend);

  // One launch at a time: a held or repeated Enter must not start several agents.
  // Starting and resuming a refactor are launches too, with the same progress and errors.
  const launching = useRef(false);
  const [launchBusy, setLaunchBusy] = useState(false);
  const runLaunch = useCallback(
    (path: string, start: () => Promise<LaunchTicket>) => {
      if (launching.current) return;
      launching.current = true;
      setLaunchBusy(true);
      const repoName = repos.get(path)?.name ?? path;
      setLaunch({ repoName, step: "starting" });
      start()
        .then(
          () => setLaunch({ repoName, step: "done" }),
          (err: unknown) => setLaunch({ repoName, step: "failed", message: describeError(err) }),
        )
        .finally(() => {
          launching.current = false;
          setLaunchBusy(false);
        });
    },
    [repos],
  );
  const startLaunch = useCallback(
    (path: string, mode: LaunchMode) => runLaunch(path, () => backend.launch(path, mode)),
    [backend, runLaunch],
  );

  // Clone and launch (spec §4.4): one job with the launch, under the same guard. The zoom
  // follows the gem to its new path once the clone shows up as a local repo.
  const sendRef = useRef<(input: NavInput) => boolean>(() => false);
  const startClone = useCallback(
    (key: string) => {
      const remote = projects.remotes.get(key);
      const gem = repos.get(key);
      if (launching.current || !remote || !gem) return;
      launching.current = true;
      setLaunchBusy(true);
      const repoName = remote.name;
      setLaunch({ repoName, step: "cloning" });
      backend
        .cloneRepo(remote)
        .then((path) => {
          sendRef.current({ type: "rekey", from: key, to: path });
          setLaunch({ repoName, step: "starting" });
          return backend.launch(path, newAgentMode(gem));
        })
        .then(
          () => setLaunch({ repoName, step: "done" }),
          (err: unknown) => setLaunch({ repoName, step: "failed", message: describeError(err) }),
        )
        .finally(() => {
          launching.current = false;
          setLaunchBusy(false);
        });
    },
    [backend, projects, repos],
  );

  const hide = (repo: RepoSnapshot) => {
    const current = settingsRef.current;
    if (!current) return;
    const key = hideKey(repo);
    if (current.hidden.includes(key)) return;
    saveSettings({ ...current, hidden: [...current.hidden, key] }).catch((err: unknown) =>
      setOpenError(describeError(err)),
    );
  };

  // The settings dialog takes every key while open. Closing with the mouse returns focus
  // to the gear; Escape hands the keyboard back to the scene, so a following Enter zooms
  // rather than pressing the gear again.
  const [settingsOpen, setSettingsOpen] = useState(false);
  const settingsOpenRef = useRef(settingsOpen);
  settingsOpenRef.current = settingsOpen;
  const gear = useRef<HTMLButtonElement>(null);
  const closeSettings = useCallback((byKey: boolean) => {
    setSettingsOpen(false);
    if (byKey) (document.activeElement as HTMLElement | null)?.blur();
    else gear.current?.focus();
  }, []);

  // The Move to WSL dialog, by source path; it takes the detail panel's place while open.
  const [moveSource, setMoveSource] = useState<string | null>(null);
  const moveOpenRef = useRef(false);
  moveOpenRef.current = moveSource !== null;
  const closeMove = useCallback(() => {
    setMoveSource(null);
    (document.activeElement as HTMLElement | null)?.blur();
  }, []);
  // Keep both: the original stays on Windows and its gem is hidden, by path, so the WSL
  // copy with the same remote still shows.
  const hidePath = (path: string) => {
    const current = settingsRef.current;
    if (!current || current.hidden.includes(path)) return;
    saveSettings({ ...current, hidden: [...current.hidden, path] }).catch((err: unknown) =>
      setOpenError(describeError(err)),
    );
  };

  // The Refactor dialog, by repo path; window keys stay with it while it is open.
  const [refactorFor, setRefactorFor] = useState<string | null>(null);
  // Set where the dialog renders, so keys go back to the scene whenever it is not shown.
  const refactorOpenRef = useRef(false);
  const closeRefactor = useCallback(() => {
    setRefactorFor(null);
    (document.activeElement as HTMLElement | null)?.blur();
  }, []);
  const openRefactor = (path: string) =>
    backend.openRefactor(path).catch((err: unknown) => setOpenError(describeError(err)));
  // A repo that goes away (removed, or moved to WSL) takes its dialog with it.
  useEffect(() => {
    if (refactorFor && !repos.has(refactorFor)) setRefactorFor(null);
  }, [refactorFor, repos]);

  // Navigation is a pure state machine; the ref lets window listeners read the latest
  // state without re-subscribing, and effects run outside the state update.
  const [nav, setNav] = useState<NavState>(initialNavState);
  const navRef = useRef(nav);
  const ctxRef = useRef<NavContext>({ repos, positions, remoteOnly: projects.remoteOnly });
  ctxRef.current = { repos, positions, remoteOnly: projects.remoteOnly };
  const runEffects = useRef<(effects: NavEffect[]) => void>(() => {});
  runEffects.current = (effects) => {
    for (const e of effects) {
      if (e.kind === "clone") startClone(e.key);
      else if (e.kind === "refactorDialog") setRefactorFor(e.path);
      else if (e.kind === "openRefactor") void openRefactor(e.path);
      else startLaunch(e.path, e.mode);
    }
  };
  const send = useCallback((input: NavInput) => {
    const result = navigate(navRef.current, input, ctxRef.current);
    navRef.current = result.state;
    setNav(result.state);
    runEffects.current(result.effects);
    return result.handled;
  }, []);
  sendRef.current = send;

  useEffect(() => {
    send({ type: "sync" });
  }, [repos, send]);

  useEffect(() => {
    const onKey = (ev: KeyboardEvent) => {
      if (ev.key === "Enter" && ev.repeat) return;
      // The Move to WSL and Refactor dialogs handle their own keys, Escape included.
      if (moveOpenRef.current) return;
      if (refactorOpenRef.current) {
        // Escape from outside the dialog (it handles its own) still closes it.
        if (ev.key === "Escape") {
          ev.preventDefault();
          closeRefactor();
        }
        return;
      }
      if (settingsOpenRef.current) {
        // Escape from outside the dialog (the dialog handles its own) still closes it.
        if (ev.key === "Escape") {
          ev.preventDefault();
          closeSettings(true);
        }
        return;
      }
      // Form controls (the quality radios) keep all their keys; buttons keep Enter.
      const target = ev.target as HTMLElement | null;
      if (target?.closest("input, select, textarea")) return;
      if (ev.key === "Enter" && target?.closest("button")) return;
      const handled = send({
        type: "key",
        key: { key: ev.key, code: ev.code, ctrl: ev.ctrlKey, meta: ev.metaKey, alt: ev.altKey },
      });
      if (handled) ev.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [send, closeSettings, closeRefactor]);

  const onPointerFocus = useCallback((path: string | null) => send({ type: "pointerFocus", path }), [send]);
  const onSelect = useCallback((path: string) => send({ type: "select", path }), [send]);
  const onBackgroundClick = useCallback(() => send({ type: "backgroundClick" }), [send]);

  const matches = useMemo(() => matchingPaths(repoList, nav.filter), [repoList, nav.filter]);
  const focusedRepo = nav.focused ? repos.get(nav.focused) : undefined;
  const focusNotCloned = !!focusedRepo && projects.remoteOnly.has(focusedRepo.path);
  const focusMigration = focusedRepo ? moves.state.get(focusedRepo.path) : undefined;
  const focusMove = focusMigration ? stepText(focusMigration.phase) : null;
  const focusLabel = useMemo(
    () =>
      focusedRepo && (
        <FocusLabel
          repo={focusedRepo}
          notCloned={focusNotCloned}
          windowsDrive={!focusNotCloned && isWindowsDrive(focusedRepo.path)}
          move={focusMove}
        />
      ),
    [focusedRepo, focusNotCloned, focusMove],
  );
  // The Windows-drive marker and the Move to WSL ring, on cloned gems only.
  const marks = useMemo(() => {
    const out = new Map<string, ReactNode>();
    for (const repo of repoList) {
      if (projects.remoteOnly.has(repo.path)) continue;
      const migration = moves.state.get(repo.path);
      const windows = isWindowsDrive(repo.path);
      if (windows || migration) out.set(repo.path, <GemMark windows={windows} migration={migration} />);
    }
    return out;
  }, [repoList, projects.remoteOnly, moves.state]);
  const zoomedRepo = nav.zoomed ? repos.get(nav.zoomed) : undefined;
  const refactorNotices = useRefactorNotices(backend, nav.zoomed);
  const notice = refactorNotices.notice;
  const refactorRepo = refactorFor && !moveSource ? repos.get(refactorFor) : undefined;
  refactorOpenRef.current = refactorRepo !== undefined;
  const zoomedRemote = zoomedRepo ? (projects.remotes.get(zoomedRepo.path) ?? null) : null;
  const zoomedRemoteOnly = !!zoomedRepo && projects.remoteOnly.has(zoomedRepo.path);
  // Details come from the listed remote, or from a local origin on a host Refract asks.
  const gitlabHost = settings?.gitlabUrl ? hostOf(settings.gitlabUrl) : null;
  const origin = zoomedRepo?.remote;
  const detailsRef: RemoteRef | null =
    zoomedRemote ??
    (origin && (origin.host === "github.com" || origin.host === gitlabHost) ? origin : null);
  const details = useRemoteDetails(backend, detailsRef, visible);
  const pickerRepo = nav.picker ? repos.get(nav.picker.path) : undefined;
  const now = Date.now();
  const showOffer =
    !offerDismissed &&
    hook.info?.installed === false &&
    localRepos.size > 0 &&
    !reposLoading &&
    !settingsOpen &&
    !moveSource &&
    // Out of the way while a repo is zoomed or the picker is up.
    !nav.zoomed &&
    !nav.picker;

  useEffect(() => {
    if (!openError) return;
    const timer = setTimeout(() => setOpenError(null), OPEN_ERROR_MS);
    return () => clearTimeout(timer);
  }, [openError]);
  const openFolder = (path: string) =>
    backend.openFolder(path).catch((err: unknown) => setOpenError(describeError(err)));
  const openUrl = (url: string) =>
    backend.openUrl(url).catch((err: unknown) => setOpenError(describeError(err)));

  return (
    <main className="app">
      {/* Until the scene's chunk arrives, its empty dark backdrop, without a spinner. */}
      <SceneBoundary>
        <Suspense fallback={<div className="scene" />}>
          <Scene
            repos={repoList}
            remoteOnly={projects.remoteOnly}
            visible={visible}
            quality={quality.level}
            theme={themeMode}
            reducedMotion={reducedMotion}
            matches={matches}
            focused={nav.focused}
            zoomed={nav.zoomed}
            focusLabel={focusLabel}
            marks={marks}
            onPointerFocus={onPointerFocus}
            onSelect={onSelect}
            onBackgroundClick={onBackgroundClick}
            onFrameInterval={quality.onFrameInterval}
          />
        </Suspense>
      </SceneBoundary>
      <FilterBar
        filter={nav.filter}
        matches={matches?.size ?? repoList.length}
        total={repoList.length}
        onClear={() => send({ type: "clearFilter" })}
      />
      {zoomedRepo && !moveSource && (
        <DetailPanel
          repo={zoomedRepo}
          now={now}
          onPrimary={() => send({ type: "launch" })}
          onLaunch={(mode) => startLaunch(zoomedRepo.path, mode)}
          onOpenFolder={() => openFolder(zoomedRepo.path)}
          onOpenUrl={openUrl}
          onClose={() => send({ type: "key", key: { key: "Escape" } })}
          onHide={() => hide(zoomedRepo)}
          remote={zoomedRemote}
          remoteOnly={zoomedRemoteOnly}
          details={details.state}
          onRefreshDetails={details.refresh}
          refactor={
            !zoomedRemoteOnly && (
              <RefactorSection
                key={zoomedRepo.path}
                repo={zoomedRepo}
                launching={launchBusy}
                onStart={() => {
                  if (nav.picker) send({ type: "pickerClose" });
                  setRefactorFor(zoomedRepo.path);
                }}
                onOpen={() => void openRefactor(zoomedRepo.path)}
                onResume={(fix) =>
                  runLaunch(zoomedRepo.path, () => backend.resumeRefactor(zoomedRepo.path, fix))
                }
                onMerge={() => backend.mergeRefactor(zoomedRepo.path)}
                onDiscard={() => backend.discardRefactor(zoomedRepo.path)}
              />
            )
          }
          move={
            !zoomedRemoteOnly &&
            (isWindowsDrive(zoomedRepo.path) || moves.state.has(zoomedRepo.path)) && (
              <MoveSection
                migration={moves.state.get(zoomedRepo.path)}
                onOpen={() => {
                  if (nav.picker) send({ type: "pickerClose" });
                  setMoveSource(zoomedRepo.path);
                }}
                onCancel={() => moves.cancel(zoomedRepo.path)}
              />
            )
          }
        />
      )}
      {moveSource && (
        <MoveDialog
          backend={backend}
          source={moveSource}
          name={repos.get(moveSource)?.name ?? moveSource.split("/").pop() ?? moveSource}
          repo={repos.get(moveSource)}
          migration={moves.state.get(moveSource)}
          moves={moves}
          onClose={closeMove}
          onKeep={hidePath}
          onMoved={(source, dest) => {
            send({ type: "rekey", from: source, to: dest });
            setRelist((n) => n + 1);
          }}
        />
      )}
      {refactorRepo && (
        <RefactorDialog
          repo={refactorRepo}
          hook={hook}
          busy={launchBusy}
          onClose={closeRefactor}
          onConfirm={(focus) => {
            const path = refactorRepo.path;
            if (launching.current) return;
            closeRefactor();
            runLaunch(path, () => backend.startRefactor(path, focus || undefined));
          }}
        />
      )}
      {nav.picker && pickerRepo && (
        <Picker
          repoName={pickerRepo.name}
          options={nav.picker.options}
          index={nav.picker.index}
          now={now}
          onMove={(index) => send({ type: "pickerMove", index })}
          onAccept={(index) => send({ type: "pickerAccept", index })}
          onClose={() => send({ type: "pickerClose" })}
        />
      )}
      <div className="rf-corner">
        <div className="rf-corner__row">
          <QualityControl setting={quality.setting} level={quality.level} onChange={quality.setSetting} />
          <button
            ref={gear}
            type="button"
            className="rf-glass rf-gear"
            aria-label="Settings"
            aria-haspopup="dialog"
            aria-expanded={settingsOpen}
            onClick={() => (settingsOpen ? closeSettings(false) : setSettingsOpen(true))}
          >
            <svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true">
              <path
                fill="currentColor"
                fillRule="evenodd"
                d="M19.4 13a7.6 7.6 0 0 0 0-2l2.1-1.6-2-3.4-2.5 1a7.4 7.4 0 0 0-1.7-1L15 3.3h-4l-.4 2.7a7.4 7.4 0 0 0-1.7 1l-2.5-1-2 3.4L6.6 11a7.6 7.6 0 0 0 0 2l-2.1 1.6 2 3.4 2.5-1a7.4 7.4 0 0 0 1.7 1l.4 2.7h4l.4-2.7a7.4 7.4 0 0 0 1.7-1l2.5 1 2-3.4zM13 15.5a3.5 3.5 0 1 1 0-7 3.5 3.5 0 0 1 0 7z"
                transform="translate(-1 0)"
              />
            </svg>
          </button>
        </div>
        {settingsOpen && (
          <SettingsPanel
            backend={backend}
            settings={settings}
            sources={remoteList?.sources ?? []}
            remotesLoading={remotesLoading}
            onChange={saveSettings}
            onRefresh={() => loadRemotes(true)}
            onClose={closeSettings}
            hook={hook}
          />
        )}
        {showOffer && <HookOffer hook={hook} onDismiss={dismissOffer} />}
      </div>
      <SystemWidget stats={widgets.stats} away={!!zoomedRepo} />
      <UsageWidget usage={widgets.usage} away={!!zoomedRepo} costEstimate={settings?.costEstimate} />
      <StatusToast
        agent={status}
        launch={launch}
        onDismiss={launch?.step === "failed" ? () => setLaunch(null) : undefined}
        notice={
          notice && {
            repoName: notice.repoName,
            text: NOTICE_TEXT[notice.alert],
            onShow: () => {
              refactorNotices.dismiss();
              send({ type: "select", path: notice.path });
            },
          }
        }
      />
      {(error ?? openError) && (
        <p className="app-error" role="alert">
          {error ?? openError}
        </p>
      )}
    </main>
  );
}
