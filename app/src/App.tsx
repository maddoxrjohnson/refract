import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";
import type { Backend } from "./backend/Backend";
import type { AgentStatus, LaunchMode } from "./backend/types";
import { useVisibility } from "./hooks/useVisibility";
import { createQualityController } from "./quality/controller";
import type { QualityLevel, QualitySetting } from "./quality/tiers";
import { initialRepoState, repoReducer } from "./repoState";
import { layoutGems } from "./scene/layout";
import { Scene } from "./scene/Scene";
import { DetailPanel } from "./ui/DetailPanel";
import { FilterBar } from "./ui/FilterBar";
import { FocusLabel } from "./ui/FocusLabel";
import {
  initialNavState,
  matchingPaths,
  navigate,
  type NavContext,
  type NavEffect,
  type NavInput,
  type NavState,
} from "./ui/navigation";
import { Picker } from "./ui/Picker";
import { loadQualitySetting, QualityControl } from "./ui/QualityControl";
import { describeError, StatusToast, type LaunchProgress } from "./ui/StatusToast";

/** How long a finished launch stays on screen. */
const LAUNCH_TOAST_MS = 4000;
/** How long a failed Open folder or Open on GitHub message stays. */
const OPEN_ERROR_MS = 6000;

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

export function App({ backend }: { backend: Backend }) {
  const visible = useVisibility(backend);
  const [status, setStatus] = useState<AgentStatus | null>(null);
  const [{ repos }, dispatch] = useReducer(repoReducer, initialRepoState);
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
  const running = status?.state === "running";
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
  }, [backend, running, shown]);

  useEffect(() => backend.onAgentEvent((event) => dispatch({ type: "event", event })), [backend]);

  const repoList = useMemo(() => [...repos.values()], [repos]);
  const positions = useMemo(() => layoutGems(repoList), [repoList]);
  const reducedMotion = useReducedMotion();
  const quality = useQuality(backend);

  // One launch at a time: a held or repeated Enter must not start several agents.
  const launching = useRef(false);
  const startLaunch = useCallback(
    (path: string, mode: LaunchMode) => {
      if (launching.current) return;
      launching.current = true;
      const repoName = repos.get(path)?.name ?? path;
      setLaunch({ repoName, step: "starting" });
      backend
        .launch(path, mode)
        .then(
          () => setLaunch({ repoName, step: "done" }),
          (err: unknown) => setLaunch({ repoName, step: "failed", message: describeError(err) }),
        )
        .finally(() => {
          launching.current = false;
        });
    },
    [backend, repos],
  );

  // Navigation is a pure state machine; the ref lets window listeners read the latest
  // state without re-subscribing, and effects run outside the state update.
  const [nav, setNav] = useState<NavState>(initialNavState);
  const navRef = useRef(nav);
  const ctxRef = useRef<NavContext>({ repos, positions });
  ctxRef.current = { repos, positions };
  const runEffects = useRef<(effects: NavEffect[]) => void>(() => {});
  runEffects.current = (effects) => {
    for (const e of effects) startLaunch(e.path, e.mode);
  };
  const send = useCallback((input: NavInput) => {
    const result = navigate(navRef.current, input, ctxRef.current);
    navRef.current = result.state;
    setNav(result.state);
    runEffects.current(result.effects);
    return result.handled;
  }, []);

  useEffect(() => {
    send({ type: "sync" });
  }, [repos, send]);

  useEffect(() => {
    const onKey = (ev: KeyboardEvent) => {
      if (ev.key === "Enter" && ev.repeat) return;
      // Form controls (the quality radios) keep all their keys; buttons keep Enter.
      const target = ev.target as HTMLElement | null;
      if (target?.closest("input, select, textarea")) return;
      if (ev.key === "Enter" && target?.closest("button")) return;
      const handled = send({
        type: "key",
        key: { key: ev.key, ctrl: ev.ctrlKey, meta: ev.metaKey, alt: ev.altKey },
      });
      if (handled) ev.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [send]);

  const onPointerFocus = useCallback((path: string | null) => send({ type: "pointerFocus", path }), [send]);
  const onSelect = useCallback((path: string) => send({ type: "select", path }), [send]);
  const onBackgroundClick = useCallback(() => send({ type: "backgroundClick" }), [send]);

  const matches = useMemo(() => matchingPaths(repoList, nav.filter), [repoList, nav.filter]);
  const focusedRepo = nav.focused ? repos.get(nav.focused) : undefined;
  const focusLabel = useMemo(() => focusedRepo && <FocusLabel repo={focusedRepo} />, [focusedRepo]);
  const zoomedRepo = nav.zoomed ? repos.get(nav.zoomed) : undefined;
  const pickerRepo = nav.picker ? repos.get(nav.picker.path) : undefined;
  const now = Date.now();

  // Open failures show briefly; they don't block anything.
  const [openError, setOpenError] = useState<string | null>(null);
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
      <Scene
        repos={repoList}
        visible={visible}
        quality={quality.level}
        reducedMotion={reducedMotion}
        matches={matches}
        focused={nav.focused}
        zoomed={nav.zoomed}
        focusLabel={focusLabel}
        onPointerFocus={onPointerFocus}
        onSelect={onSelect}
        onBackgroundClick={onBackgroundClick}
        onFrameInterval={quality.onFrameInterval}
      />
      <FilterBar
        filter={nav.filter}
        matches={matches?.size ?? repoList.length}
        total={repoList.length}
        onClear={() => send({ type: "clearFilter" })}
      />
      {zoomedRepo && (
        <DetailPanel
          repo={zoomedRepo}
          now={now}
          onPrimary={() => send({ type: "launch" })}
          onLaunch={(mode) => startLaunch(zoomedRepo.path, mode)}
          onOpenFolder={() => openFolder(zoomedRepo.path)}
          onOpenUrl={openUrl}
          onClose={() => send({ type: "key", key: { key: "Escape" } })}
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
      <QualityControl setting={quality.setting} level={quality.level} onChange={quality.setSetting} />
      <StatusToast
        agent={status}
        launch={launch}
        onDismiss={launch?.step === "failed" ? () => setLaunch(null) : undefined}
      />
      {(error ?? openError) && (
        <p className="app-error" role="alert">
          {error ?? openError}
        </p>
      )}
    </main>
  );
}
