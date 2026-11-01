import { Canvas, useFrame, useThree } from "@react-three/fiber";
import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
  type RefObject,
} from "react";
import {
  HalfFloatType,
  InstancedMesh,
  MathUtils,
  Matrix4,
  Quaternion,
  Vector2,
  Vector3,
  WebGLRenderTarget,
  type Camera,
  type PerspectiveCamera,
} from "three";
import { EffectComposer } from "three/examples/jsm/postprocessing/EffectComposer.js";
import { OutputPass } from "three/examples/jsm/postprocessing/OutputPass.js";
import { RenderPass } from "three/examples/jsm/postprocessing/RenderPass.js";
import type { RepoSnapshot } from "../backend/types";
import type { ThemeMode } from "../theme";
import { TIERS, type QualityLevel } from "../quality/tiers";
import { refactorMark } from "../refactors";
import { repoStatus } from "../repoStatus";
import { applyPalette, makeSheet, makeSheetUniforms, makeStoneMaterial } from "./banknote";
import { shardGeometry, prepare } from "./cuts";
import { Gem, type GemHandle } from "./Gem";
import { layoutGems, layoutRadius, rng, type GemPosition } from "./layout";
import { createTicker } from "./ticker";

// The Banknote scene (banknote spec), ported from docs/mockups/gem-studies.html: engraved
// stones on a paper sheet seen from above, a rosette under each, splinters from the opening.
//
// Frames render through three's EffectComposer: the scene into a half-float target
// (multisampled above Low), then an OutputPass to sRGB, so palette colours show exactly.
// Frames are drawn on demand only: on a prop change, pointer movement or resize, and
// while something animates, paced to the tier's frame cap by ./ticker.ts. The loop body
// allocates nothing per frame beyond the style strings of the label and the marks.

export interface SceneProps {
  repos: RepoSnapshot[];
  /** False stops the frame loop: nothing draws while hidden. */
  visible: boolean;
  quality: QualityLevel;
  /** Light or dark palette (banknote spec §1.1); switching never replays the opening. */
  theme: ThemeMode;
  /** No opening, turning or knocking; camera moves still ease, but quickly. */
  reducedMotion: boolean;
  /** Paths of gems that are not cloned; they draw stippled. */
  remoteOnly: ReadonlySet<string>;
  /** Filter result; `null` is no filter. Gems outside it fade towards the paper. */
  matches: ReadonlySet<string> | null;
  /** Path of the gem that lifts, grows a little and shows `focusLabel`. */
  focused: string | null;
  /** Path of the gem the camera flies to, left of centre so a panel fits on the right. */
  zoomed: string | null;
  /** Shown beside the focused gem; the scene moves it each frame. */
  focusLabel?: ReactNode;
  /**
   * Small overlays on gems, by path (the Windows-drive marker, the Move to WSL ring).
   * Each is centred on its gem in the frames the scene draws anyway, with `--r` set to
   * the gem's radius in pixels; they take no pointer events.
   */
  marks?: ReadonlyMap<string, ReactNode>;
  /** Magnet focus: the gem nearest the pointer within ~130 px, or null. */
  onPointerFocus(path: string | null): void;
  /** A click while a gem is in magnet range. */
  onSelect(path: string): void;
  /** A click with no gem in range. */
  onBackgroundClick(): void;
  /**
   * Milliseconds between two drawn frames of continuous animation, for the quality
   * controller. Only reported when the earlier frame asked for the later one, so idle
   * gaps never count; late frames mean the GPU or CPU cannot hold the tier's frame cap.
   */
  onFrameInterval(ms: number): void;
}

const FOV = 28;
const MAGNET_PX = 130;
/** The focused gem wins magnet ties within this many pixels, so focus does not flicker. */
const MAGNET_HYSTERESIS_PX = 10;
/** How far a focused gem rises, and how much it grows. */
const LIFT = 0.3;
const FOCUS_GROW = 0.06;
/** Strength of a gem outside the filter: it fades this far from the paper. */
const FILTER_FADE = 0.22;
/** The overview looks down from this far above the sheet, at this point. */
const ELEVATION = MathUtils.degToRad(56);
const LOOK = new Vector3(0, 0, 0.3);
/** The pointer tilts the view this far at the canvas edges. */
const TILT_ELEVATION = MathUtils.degToRad(4);
const TILT_AZIMUTH = 0.12;
/** A zoomed gem is seen from lower down, so its facets read. */
const ZOOM_ELEVATION = MathUtils.degToRad(35);
/** A zoomed gem's girdle radius as a share of half the view height. */
const ZOOM_FILL = 0.34;
/** Where a zoomed gem sits horizontally, in normalised device coordinates. */
const ZOOM_X = -0.4;
/**
 * How far below its centre a zoomed gem reaches, rosette and ring included, as a share of
 * half the view height: seen from 35°, the near side of the ring lies about 1.5 sizes down.
 * A waiting stone's rings spread further and may pass under the picker as they fade.
 */
export const ZOOM_REACH = 0.5;
/**
 * Pixels kept clear at the bottom for the launch picker: its 24 px offset, its
 * 300 px maximum height (`.rf-picker` in ui.css) and a gap.
 */
export const PICKER_CLEAR_PX = 340;
/** The least room left above a lifted gem, in pixels. */
export const ZOOM_TOP_MARGIN_PX = 16;

/**
 * How far a zoomed gem moves up from the middle, in normalised device coordinates:
 * enough to clear the launch picker below it, but never past the top edge.
 */
export function zoomLift(height: number): number {
  if (!(height > 0)) return 0;
  const half = height / 2;
  const room = (1 - ZOOM_REACH) * half;
  const px = Math.min(Math.max(0, PICKER_CLEAR_PX - room), Math.max(0, room - ZOOM_TOP_MARGIN_PX));
  return px / half;
}

/** Camera distance that fits a layout `radius` across in both directions (mockup `resize()`). */
export function fitDistance(radius: number, aspect: number): number {
  const vf = MathUtils.degToRad(FOV);
  const hf = 2 * Math.atan(Math.tan(vf / 2) * aspect);
  return ((radius + 0.4) / Math.sin(Math.min(vf * 1.25, hf) / 2)) * 1.05;
}

/** Opening timings in seconds (banknote spec §4). */
const BURST_S = 1.5;
const SHARD_S = 1.4;
const DROP_S = 0.8;
/** A later stone drops from this high. */
const DROP_HEIGHT = 2.5;
/** Rings draw in over RING_S, starting RING_AT into the opening. */
const RING_AT = 1.3;
const RING_S = 0.6;
/** Turning speeds in radians per second; working stones turn faster and spin their rosette. */
const TURN = 0.12;
const TURN_WORKING = 0.45;
const ROSETTE_SPIN = 0.5;

/** The rosette's turn at time `t` (seconds), wrapped to one revolution: float32 in the
 * shader would step visibly after days of uptime, and the guilloche's whole-number
 * wave counts make the pattern repeat exactly every full turn. */
export function rosetteSpin(t: number): number {
  return (t * ROSETTE_SPIN) % (2 * Math.PI);
}
/** The refactor mark's ticks turn slowly; 48 ticks repeat every 1/48 turn, so it wraps there. */
const REFACTOR_SPIN = 0.2;
const REFACTOR_TICK = (2 * Math.PI) / 48;
export function refactorSpin(t: number): number {
  return (t * REFACTOR_SPIN) % REFACTOR_TICK;
}
/**
 * A stone whose agent waits on you knocks once a period: a hop of `KNOCK_LIFT` stone
 * sizes with a wobble of `KNOCK_TILT` radians, over `KNOCK_S` seconds. A ping ring
 * leaves the sheet under it at each knock and half a period later.
 */
const KNOCK_PERIOD = 1.6;
const KNOCK_S = 0.45;
const KNOCK_LIFT = 0.18;
const KNOCK_TILT = 0.16;
/** The smallest layout radius the camera and sheet are fitted to. */
const MIN_RADIUS = 2;

const GL_OPTIONS = { antialias: false, powerPreference: "high-performance" } as const;
const CAMERA = { fov: FOV, near: 0.5, far: 300, position: [0, 20, 14] as [number, number, number] };

export function Scene(props: SceneProps) {
  const root = useRef<HTMLDivElement>(null);
  const label = useRef<HTMLDivElement>(null);
  const markEls = useRef(new Map<string, HTMLDivElement>());
  const dpr = Math.min(window.devicePixelRatio || 1, TIERS[props.quality].maxPixelRatio);
  return (
    <div className="scene" ref={root}>
      <Canvas
        frameloop={props.visible ? "demand" : "never"}
        dpr={dpr}
        flat
        gl={GL_OPTIONS}
        camera={CAMERA}
        fallback={
          <div className="scene-fail">
            This computer could not start WebGL, so the scene cannot be drawn.
          </div>
        }
      >
        <SceneContents {...props} root={root} label={label} markEls={markEls} />
      </Canvas>
      {props.marks &&
        [...props.marks].map(([path, node]) => (
          <div
            key={path}
            className="scene-mark"
            hidden
            ref={(el) => {
              if (!el) return;
              markEls.current.set(path, el);
              return () => void markEls.current.delete(path);
            }}
          >
            {node}
          </div>
        ))}
      <div className="scene-label" ref={label} hidden>
        {props.focusLabel}
      </div>
    </div>
  );
}

interface ContentsProps extends SceneProps {
  root: RefObject<HTMLDivElement | null>;
  label: RefObject<HTMLDivElement | null>;
  markEls: RefObject<Map<string, HTMLDivElement>>;
}

/** Mutable state the frame loop and pointer handlers share; never drives a React render. */
interface LoopState {
  first: boolean;
  /** Seconds of ambient motion (turning, knocking). */
  time: number;
  /** Seconds of drawn frames, motion or not: drives the opening and drop-ins. */
  clock: number;
  /** `clock` when the opening started; NaN until there are repos. */
  openAt: number;
  /** The opening has played out, rings included (shown as `data-opening` on `.scene`). */
  openingDone: boolean;
  /** Pointer in CSS pixels over the canvas, and whether it is over it at all. */
  px: number;
  py: number;
  inside: boolean;
  /** Pointer tilt, eased towards its target; both in -1 to 1 across the canvas. */
  ptr: Vector2;
  ptrTarget: Vector2;
  /** Camera, eased: elevation, distance, the point framed, and where it sits on screen (NDC). */
  el: number;
  dist: number;
  target: Vector3;
  ox: number;
  oy: number;
  /** The last magnet focus reported to `onPointerFocus`. */
  magnet: string | null;
  /** The pointer moved since the last frame, so the magnet focus may have changed. */
  moved: boolean;
  /** When the last drawn frame started, and whether it asked the ticker for another. */
  lastStart: number;
  followUp: boolean;
  /** Splinters still flying, the layout radius they sit at (eased once the opening starts; the layout's own before it), and their mesh. */
  shardsLive: boolean;
  shardsRadius: number;
  shardsMesh: InstancedMesh | null;
  gem: GemFrame;
}

/** Per-frame inputs to `updateGem`, passed as `this` so the loop allocates no closure. */
interface GemFrame {
  /** Easing step: frame seconds times the reduced-motion pace. */
  k: number;
  T: number;
  clock: number;
  openAt: number;
  reduced: boolean;
  /** Ambient motion is on (not Low, not reduced motion): waiting stones knock and ping. */
  ambient: boolean;
  focused: string | null;
  matches: ReadonlySet<string> | null;
  /** Set when any gem is still easing towards its goal. */
  settling: boolean;
  /** Set when a leaving gem has faded out. */
  faded: boolean;
}

const tmp = new Vector3();
const composerSize = new Vector2();
const targetGoal = new Vector3();
const look = new Vector3();
const mPos = new Vector3();
const mScale = new Vector3();
const mQuat = new Quaternion();
const mSpin = new Quaternion();
const mMat = new Matrix4();
const AXIS_X = new Vector3(1, 0, 0);

const clamp01 = (x: number) => Math.min(1, Math.max(0, x));
const easeOutExpo = (t: number) => (t >= 1 ? 1 : 1 - Math.pow(2, -10 * t));

/** Share of the way to a goal to move this frame. */
function ease(k: number, rate: number): number {
  return Math.min(1, k * rate);
}

/** Splinters, seeded once: where each lands (angle, share of the layout radius), size and flight. */
const SHARDS = (() => {
  const r = rng(21);
  return Array.from({ length: Math.max(...Object.values(TIERS).map((t) => t.splinters)) }, () => {
    const a = r() * Math.PI * 2;
    const reach = Math.pow(r(), 0.6);
    const s = 0.06 + Math.pow(r(), 2) * 0.22;
    const q = new Quaternion().setFromAxisAngle(new Vector3(0, 1, 0), a + (r() - 0.5) * 1.2);
    q.multiply(new Quaternion().setFromAxisAngle(AXIS_X, (r() - 0.5) * 0.2));
    return { a, reach, s, q, delay: r() * 0.08, spin: (r() - 0.5) * 12, hop: 0.6 + r() * 1.2 };
  });
})();

function updateGem(this: GemFrame, g: GemHandle) {
  if (Number.isNaN(g.born)) {
    // A gem first seen already leaving (it should keep its handle, but in case) only fades.
    g.born = g.leaving ? -Infinity : this.clock;
    g.burst = !g.leaving && this.clock === this.openAt;
  }
  const fGoal = !g.leaving && g.path === this.focused ? 1 : 0;
  const mGoal = !this.matches || this.matches.has(g.path) ? 1 : 0;
  const lGoal = g.leaving ? 0 : 1;
  g.f += (fGoal - g.f) * ease(this.k, 9);
  g.m += (mGoal - g.m) * ease(this.k, 6);
  g.life += (lGoal - g.life) * ease(this.k, 5);
  g.home.lerp(g.base, ease(this.k, 4));
  if (
    Math.abs(fGoal - g.f) > 0.002 ||
    Math.abs(mGoal - g.m) > 0.002 ||
    Math.abs(lGoal - g.life) > 0.002 ||
    g.home.distanceToSquared(g.base) > 1e-6
  ) {
    this.settling = true;
  } else {
    g.f = fGoal;
    g.m = mGoal;
    g.life = lGoal;
    g.home.copy(g.base);
  }
  if (g.leaving && g.life < 0.01) this.faded = true;

  // The opening burst flies in from above the centre; later stones drop into place.
  const e = this.clock - g.born;
  const p = this.reduced ? 1 : easeOutExpo(clamp01(g.burst ? (e - g.delay) / BURST_S : e / DROP_S));
  if (p < 1) this.settling = true;
  const h = g.holder;
  const grow = (1 + FOCUS_GROW * g.f) * g.life;
  if (g.burst) {
    h.position.lerpVectors(g.start, g.home, p);
    h.position.y = g.home.y + (1 - p) * g.start.y + g.f * LIFT;
    h.rotation.y = (1 - p) * g.spinIn;
    h.scale.setScalar((0.35 + 0.65 * p) * grow);
  } else {
    h.position.copy(g.home);
    h.position.y += (1 - p) * DROP_HEIGHT + g.f * LIFT;
    h.rotation.y = 0;
    h.scale.setScalar(grow);
  }
  g.spot.position.set(g.home.x, 0, g.home.z);
  g.spot.scale.setScalar(g.life);
  // Ambient time only runs while motion is on, so turning off motion freezes everything
  // where it is rather than snapping it back.
  const T = this.T;
  g.spinner.rotation.y = g.phase + T * (g.working ? TURN_WORKING : TURN) * g.spinDir;
  // The knock moves the stone inside its holder, so the label and marks, placed from the
  // holder, stay still. Without ambient motion there is no knock (it would freeze mid-hop).
  const knock = g.waiting && this.ambient ? ((T + g.phase) % KNOCK_PERIOD) / KNOCK_S : 1;
  if (knock < 1) {
    g.spinner.position.y = Math.sin(Math.PI * knock) * KNOCK_LIFT * g.size;
    g.spinner.rotation.x = Math.sin(knock * Math.PI * 5) * (1 - knock) * KNOCK_TILT;
  } else {
    g.spinner.position.y = 0;
    g.spinner.rotation.x = 0;
  }

  const fade = (FILTER_FADE + (1 - FILTER_FADE) * g.m) * g.life;
  const su = g.stone.uniforms;
  su.fade.value = fade;
  // Waiting draws the stone in red at once; the band of weight only runs while ambient
  // time does, so reduced motion and Low show it still, and the rings not at all. Both
  // phases wrap here, as float32 in the shader would step visibly after a day or so.
  su.alarm.value = g.waiting ? 1 : 0;
  su.alarmT.value = (T + g.phase) % ((2 * Math.PI) / 3);
  if (g.ping) {
    g.ping.uniforms.t.value = ((T + g.phase) / KNOCK_PERIOD) % 1;
    g.ping.uniforms.fade.value = this.ambient ? fade : 0;
  }
  g.rosette.uniforms.fade.value = fade;
  if (g.refactor) {
    // Ambient time stops on Low and with reduced motion, so the ticks stand still there.
    const mark = g.refactorMark;
    const ru = g.refactor.uniforms;
    ru.fade.value = fade;
    ru.kind.value = mark === "ready" ? 2 : mark === "plain" ? 0 : 1;
    ru.red.value = mark === "alert" ? 1 : 0;
    ru.spin.value = mark === "turning" ? refactorSpin(T) : 0;
  }
  g.rosette.uniforms.spin.value = g.working ? rosetteSpin(T) : 0;
  if (g.ring) {
    // Rings of stones in the opening draw in partway through it; later ones at once.
    if (Number.isNaN(g.ringAt)) {
      g.ringAt = g.leaving
        ? -Infinity
        : g.burst && this.clock < this.openAt + RING_AT
          ? this.openAt + RING_AT
          : this.clock;
    }
    const draw = this.reduced ? 1 : clamp01((this.clock - g.ringAt) / RING_S);
    if (draw < 1) this.settling = true;
    g.ring.uniforms.draw.value = draw;
    g.ring.uniforms.fade.value = fade;
  } else {
    g.ringAt = Number.NaN;
  }
}

/**
 * The gem whose centre is nearest (`x`, `y`) on screen, within the magnet radius. Measures
 * from resting positions, not lifted ones, so focusing a gem does not move its target; the
 * gem in `prefer` wins near-ties.
 */
function nearestGem(
  gems: Map<string, GemHandle>,
  camera: Camera,
  width: number,
  height: number,
  x: number,
  y: number,
  prefer: string | null,
): string | null {
  let best: string | null = null;
  let bestDist = MAGNET_PX;
  for (const g of gems.values()) {
    if (g.leaving) continue;
    tmp.copy(g.base).project(camera);
    if (tmp.z > 1) continue;
    const d =
      Math.hypot((tmp.x * 0.5 + 0.5) * width - x, (-tmp.y * 0.5 + 0.5) * height - y) -
      (g.path === prefer ? MAGNET_HYSTERESIS_PX : 0);
    if (d < bestDist) {
      bestDist = d;
      best = g.path;
    }
  }
  return best;
}

/**
 * Projects a gem's centre into `tmp` (pixels in x and y) and returns its radius in pixels,
 * or NaN when it is behind the camera.
 */
function screenCircle(g: GemHandle, camera: Camera, W: number, H: number, tv: number): number {
  tmp.copy(g.holder.position);
  const dist = camera.position.distanceTo(tmp);
  tmp.project(camera);
  if (!(dist > 0) || tmp.z > 1) return Number.NaN;
  tmp.set((tmp.x * 0.5 + 0.5) * W, (-tmp.y * 0.5 + 0.5) * H, 0);
  return ((g.size * g.holder.scale.x) / (dist * tv)) * (H / 2);
}

/** Centres each mark on its gem and sets `--r` to the gem's radius in pixels; hides marks without one. */
function placeMarks(
  els: Map<string, HTMLDivElement>,
  gems: Map<string, GemHandle>,
  camera: Camera,
  W: number,
  H: number,
  tv: number,
) {
  for (const [path, el] of els) {
    const g = gems.get(path);
    const radiusPx = g && !g.leaving ? screenCircle(g, camera, W, H, tv) : Number.NaN;
    if (Number.isNaN(radiusPx)) {
      el.hidden = true;
      continue;
    }
    el.hidden = false;
    el.style.transform = `translate(${tmp.x.toFixed(1)}px, ${tmp.y.toFixed(1)}px)`;
    el.style.setProperty("--r", `${radiusPx.toFixed(1)}px`);
  }
}

/** Puts the label beside the gem, clear of its edge and inside the canvas; hides it without one. */
function placeLabel(
  el: HTMLDivElement | null,
  g: GemHandle | undefined,
  camera: Camera,
  W: number,
  H: number,
  tv: number,
) {
  if (!el) return;
  const radiusPx = g && !g.leaving ? screenCircle(g, camera, W, H, tv) : Number.NaN;
  if (Number.isNaN(radiusPx)) {
    el.hidden = true;
    return;
  }
  el.hidden = false;
  const lw = el.offsetWidth;
  const lh = el.offsetHeight;
  const lx = Math.max(8, Math.min(W - lw - 8, tmp.x + radiusPx + 14));
  const ly = Math.max(8, Math.min(H - lh - 8, tmp.y - lh / 2));
  el.style.transform = `translate(${lx}px, ${ly}px)`;
}

/** A repo that went away, drawn where it was until it has faded out. */
interface Leaving {
  repo: RepoSnapshot;
  pos: GemPosition;
}

function SceneContents(props: ContentsProps) {
  const { repos, remoteOnly, quality, theme, reducedMotion, matches, focused, zoomed, focusLabel, visible, marks } =
    props;
  const tier = TIERS[quality];
  const gl = useThree((s) => s.gl);
  const scene = useThree((s) => s.scene);
  const camera = useThree((s) => s.camera) as PerspectiveCamera;
  const size = useThree((s) => s.size);
  const dpr = useThree((s) => s.viewport.dpr);
  const invalidate = useThree((s) => s.invalidate);

  // Callbacks for the DOM listeners, always the latest props.
  const latest = useRef(props);
  useLayoutEffect(() => {
    latest.current = props;
  });

  const layout = useMemo(() => layoutGems(repos), [repos]);
  const radius = useMemo(() => Math.max(MIN_RADIUS, layoutRadius(layout)), [layout]);
  const gems = useMemo(() => new Map<string, GemHandle>(), []);

  // Repos that went away stay drawn until they fade out (adjusting state on a prop change).
  const [leaving, setLeaving] = useState<ReadonlyMap<string, Leaving>>(() => new Map());
  const [prev, setPrev] = useState({ repos, layout });
  if (prev.repos !== repos) {
    const now = new Set(repos.map((r) => r.path));
    const next = new Map(leaving);
    for (const r of prev.repos) {
      const pos = prev.layout.get(r.path);
      if (!now.has(r.path) && pos) next.set(r.path, { repo: r, pos });
    }
    for (const path of now) next.delete(path);
    setLeaving(next);
    setPrev({ repos, layout });
  }

  // Palette uniforms shared by every material; a mode switch changes only their values.
  const sheetUniforms = useMemo(makeSheetUniforms, []);
  useLayoutEffect(() => {
    applyPalette(sheetUniforms, theme);
  }, [sheetUniforms, theme]);
  useLayoutEffect(() => {
    sheetUniforms.px.value = dpr;
  }, [sheetUniforms, dpr]);

  const sheet = useMemo(() => makeSheet(sheetUniforms), [sheetUniforms]);
  useEffect(
    () => () => {
      sheet.geometry.dispose();
      sheet.material.dispose();
    },
    [sheet],
  );
  useLayoutEffect(() => {
    sheet.material.uniforms.reach.value = radius * 1.5;
  }, [sheet, radius]);

  useEffect(() => {
    scene.background = sheetUniforms.edge.value;
    return () => {
      scene.background = null;
    };
  }, [scene, sheetUniforms]);

  // Splinters: engraved paper shards, without a wash.
  const shardGeo = useMemo(() => prepare(shardGeometry(rng(5), 1, 0.32, 0.06)), []);
  const shardMat = useMemo(
    () => makeStoneMaterial(sheetUniforms, sheetUniforms.paper.value, { value: 0 }),
    [sheetUniforms],
  );
  const shards = useMemo(() => {
    const m = new InstancedMesh(shardGeo, shardMat, tier.splinters);
    // Instances move after the bounds would be computed, so never cull.
    m.frustumCulled = false;
    m.visible = false;
    return m;
  }, [shardGeo, shardMat, tier.splinters]);
  useEffect(() => () => shardGeo.dispose(), [shardGeo]);
  useEffect(() => () => shardMat.dispose(), [shardMat]);
  useEffect(() => () => shards.dispose(), [shards]);

  // The scene into a half-float target, multisampled above Low, then out to sRGB. Built in
  // an effect, not a memo, because disposing a composer empties it.
  const [composer, setComposer] = useState<EffectComposer | null>(null);
  useEffect(() => {
    const target = new WebGLRenderTarget(1, 1, { type: HalfFloatType, samples: tier.msaa });
    const c = new EffectComposer(gl, target);
    const render = new RenderPass(scene, camera);
    const output = new OutputPass();
    c.addPass(render);
    c.addPass(output);
    // Sized now, so the first frame after a tier change does not render at 1×1.
    gl.getSize(composerSize);
    c.setPixelRatio(gl.getPixelRatio());
    c.setSize(composerSize.x, composerSize.y);
    setComposer(c);
    return () => {
      setComposer(null);
      render.dispose();
      output.dispose();
      c.dispose();
    };
  }, [gl, scene, camera, tier.msaa]);

  useEffect(() => {
    if (!composer) return;
    composer.setPixelRatio(dpr);
    composer.setSize(size.width, size.height);
    invalidate();
  }, [composer, size, dpr, invalidate]);

  const loop = useRef<LoopState>({
    first: true,
    time: 0,
    clock: 0,
    openAt: Number.NaN,
    openingDone: false,
    px: 0,
    py: 0,
    inside: false,
    ptr: new Vector2(),
    ptrTarget: new Vector2(),
    el: ELEVATION,
    dist: 20,
    target: new Vector3(),
    ox: 0,
    oy: 0,
    magnet: null,
    moved: false,
    lastStart: 0,
    followUp: false,
    shardsLive: false,
    shardsRadius: Number.NaN,
    shardsMesh: null,
    gem: {
      k: 0,
      T: 0,
      clock: 0,
      openAt: Number.NaN,
      reduced: false,
      ambient: false,
      focused: null,
      matches: null,
      settling: false,
      faded: false,
    },
  });
  const ticker = useMemo(() => createTicker(invalidate), [invalidate]);
  useEffect(() => {
    const s = loop.current;
    const stop = () => {
      ticker.cancel();
      // The next frame starts after a gap: neither a measured interval nor a real delta.
      s.followUp = false;
    };
    if (!visible) stop();
    return stop;
  }, [ticker, visible]);

  // Any change worth showing draws a frame, within the frame cap; the loop keeps going
  // while things settle. The label's and the marks' content is React's job; only whether
  // there is a label, and which gems have marks, matter here.
  const hasLabel = focusLabel != null;
  const markKeys = marks ? [...marks.keys()].join("\n") : "";
  useEffect(() => {
    ticker.request(tier.fpsCap, loop.current.lastStart);
  }, [ticker, tier, repos, leaving, remoteOnly, theme, reducedMotion, matches, focused, zoomed, hasLabel, markKeys, visible]);

  useEffect(() => {
    const el = gl.domElement;
    const s = loop.current;
    const place = (ev: MouseEvent) => {
      const b = el.getBoundingClientRect();
      s.px = ev.clientX - b.left;
      s.py = ev.clientY - b.top;
      return b;
    };
    // Pointer frames go through the ticker too, so moving the mouse keeps the frame cap.
    const move = (ev: PointerEvent) => {
      const b = place(ev);
      s.inside = true;
      s.moved = true;
      s.ptrTarget.set((s.px / b.width) * 2 - 1, (s.py / b.height) * 2 - 1);
      ticker.request(TIERS[latest.current.quality].fpsCap, s.lastStart);
    };
    const leave = () => {
      s.inside = false;
      s.ptrTarget.set(0, 0);
      if (s.magnet !== null) {
        s.magnet = null;
        latest.current.onPointerFocus(null);
      }
      ticker.request(TIERS[latest.current.quality].fpsCap, s.lastStart);
    };
    const click = (ev: MouseEvent) => {
      const b = place(ev);
      const hit = nearestGem(gems, camera, b.width, b.height, s.px, s.py, latest.current.focused);
      if (hit) latest.current.onSelect(hit);
      else latest.current.onBackgroundClick();
    };
    el.addEventListener("pointermove", move);
    el.addEventListener("pointerleave", leave);
    el.addEventListener("click", click);
    return () => {
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerleave", leave);
      el.removeEventListener("click", click);
    };
  }, [gl, camera, gems, ticker]);

  // Priority 1 takes over rendering from R3F.
  useFrame((_, delta) => {
    if (!composer) return;
    const start = performance.now();
    const s = loop.current;
    if (s.followUp) latest.current.onFrameInterval(start - s.lastStart);
    // After an idle pause the clock's delta spans the pause; step as one capped frame.
    const rdt = s.followUp ? Math.min(delta, 0.1) : 1 / tier.fpsCap;
    const ambient = tier.ambientMotion && !reducedMotion;
    if (ambient) s.time += rdt;
    s.clock += rdt;
    if (Number.isNaN(s.openAt) && repos.length > 0) s.openAt = s.clock;
    // Reduced motion keeps the easing but makes it quick.
    const k = rdt * (reducedMotion ? 4 : 1);
    const W = size.width;
    const H = size.height;
    // A collapsed canvas (zero height) is drawn as if square rather than with NaNs.
    const aspect = H > 0 ? W / H : 1;
    const tv = Math.tan(MathUtils.degToRad(FOV / 2));
    let settling = false;

    s.ptr.lerp(s.ptrTarget, ease(k, 6));
    if (s.ptr.distanceTo(s.ptrTarget) > 0.002) settling = true;

    // The camera fits the layout from 56° up, or flies in to frame the zoomed gem from 35°,
    // left of centre and above the picker.
    const zoomGem = zoomed ? gems.get(zoomed) : undefined;
    let elGoal = ELEVATION;
    let distGoal = fitDistance(radius, aspect);
    let oxGoal = 0;
    let oyGoal = 0;
    targetGoal.copy(LOOK);
    if (zoomGem) {
      elGoal = ZOOM_ELEVATION;
      distGoal = zoomGem.size / (tv * ZOOM_FILL);
      oxGoal = ZOOM_X;
      oyGoal = zoomLift(H);
      targetGoal.copy(zoomGem.base);
    }
    if (s.first) {
      s.el = elGoal;
      s.dist = distGoal;
      s.ox = oxGoal;
      s.oy = oyGoal;
      s.target.copy(targetGoal);
      s.first = false;
    }
    const kc = ease(k, 3);
    s.el += (elGoal - s.el) * kc;
    s.dist += (distGoal - s.dist) * kc;
    s.ox += (oxGoal - s.ox) * kc;
    s.oy += (oyGoal - s.oy) * kc;
    s.target.lerp(targetGoal, kc);
    if (
      Math.abs(elGoal - s.el) > 1e-4 ||
      Math.abs(distGoal - s.dist) > 0.003 ||
      Math.abs(oxGoal - s.ox) > 1e-4 ||
      Math.abs(oyGoal - s.oy) > 1e-4 ||
      s.target.distanceTo(targetGoal) > 0.003
    ) {
      settling = true;
    }
    // Orbit the framed point by the eased angles plus the pointer's tilt, offsetting along
    // the camera's own right and up so the framed point keeps its place on screen.
    const el = s.el - s.ptr.y * TILT_ELEVATION;
    const az = s.ptr.x * TILT_AZIMUTH;
    const ce = Math.cos(el);
    const se = Math.sin(el);
    const ca = Math.cos(az);
    const sa = Math.sin(az);
    const hx = s.ox * tv * aspect * s.dist;
    const hy = s.oy * tv * s.dist;
    look.set(
      s.target.x - ca * hx + sa * se * hy,
      s.target.y - ce * hy,
      s.target.z + sa * hx + ca * se * hy,
    );
    camera.position.set(look.x + sa * ce * s.dist, look.y + se * s.dist, look.z + ca * ce * s.dist);
    camera.lookAt(look);

    const gf = s.gem;
    gf.k = k;
    gf.T = s.time;
    gf.clock = s.clock;
    gf.openAt = s.openAt;
    gf.reduced = reducedMotion;
    gf.ambient = ambient;
    gf.focused = focused;
    gf.matches = matches;
    gf.settling = false;
    gf.faded = false;
    gems.forEach(updateGem, gf);
    if (gf.settling) settling = true;
    if (gf.faded) {
      const done: string[] = [];
      for (const g of gems.values()) if (g.leaving && g.life < 0.01) done.push(g.path);
      setLeaving((m) => {
        const next = new Map(m);
        for (const path of done) next.delete(path);
        return next;
      });
    }

    // Splinters fly out with the opening, then rest; when the layout grows or shrinks they
    // ease out or in with it.
    const opened = !Number.isNaN(s.openAt);
    shards.visible = opened;
    // Until the opening starts there is nothing to ease from: the splinters fly straight to
    // wherever the layout is when it does.
    if (!opened || Number.isNaN(s.shardsRadius)) s.shardsRadius = radius;
    const shardsMoving = Math.abs(radius - s.shardsRadius) > 1e-3;
    if (shardsMoving) {
      s.shardsRadius += (radius - s.shardsRadius) * ease(k, 4);
      settling = true;
    } else {
      s.shardsRadius = radius;
    }
    if (opened && (s.shardsLive || shardsMoving || s.shardsMesh !== shards)) {
      const e = s.clock - s.openAt;
      let done = true;
      for (let i = 0; i < shards.count; i++) {
        const d = SHARDS[i];
        const p = reducedMotion ? 1 : easeOutExpo(clamp01((e - d.delay) / SHARD_S));
        if (p < 1) done = false;
        const reach = 0.8 + d.reach * (s.shardsRadius + 0.6);
        const y = d.s * 0.03;
        mPos.set(
          Math.cos(d.a) * reach * p,
          y + d.hop * 4 * p * (1 - p) * (1 - p) + (1 - p) * 1.4,
          Math.sin(d.a) * reach * 0.85 * p,
        );
        mQuat.copy(d.q).multiply(mSpin.setFromAxisAngle(AXIS_X, (1 - p) * d.spin));
        shards.setMatrixAt(i, mMat.compose(mPos, mQuat, mScale.setScalar(d.s)));
      }
      shards.instanceMatrix.needsUpdate = true;
      s.shardsLive = !done;
      s.shardsMesh = shards;
      if (!done) settling = true;
    }
    // Tests and recordings wait on this rather than on a guessed delay.
    const openingDone = opened && (reducedMotion || s.clock - s.openAt >= RING_AT + RING_S);
    if (opened && (openingDone !== s.openingDone || !props.root.current?.dataset.opening)) {
      s.openingDone = openingDone;
      if (props.root.current) props.root.current.dataset.opening = openingDone ? "done" : "playing";
    }
    // Keep drawing until it is done, even when nothing visibly moves (Low, clean repos).
    if (opened && !openingDone) settling = true;
    scene.updateMatrixWorld();
    camera.updateMatrixWorld();

    // Only real pointer movement changes the magnet focus; a camera flight under a still
    // pointer does not.
    if (s.inside && s.moved) {
      s.moved = false;
      const hit = nearestGem(gems, camera, W, H, s.px, s.py, focused);
      if (hit !== s.magnet) {
        s.magnet = hit;
        latest.current.onPointerFocus(hit);
      }
    }
    placeLabel(
      props.label.current,
      focused && focused !== zoomed && focusLabel != null ? gems.get(focused) : undefined,
      camera,
      W,
      H,
      tv,
    );
    placeMarks(props.markEls.current, gems, camera, W, H, tv);

    composer.render(rdt);

    s.lastStart = start;
    s.followUp = ambient || settling;
    if (s.followUp) ticker.request(tier.fpsCap, start);
    else ticker.cancel();
  }, 1);

  const gemFor = (repo: RepoSnapshot, p: GemPosition, gone: boolean) => {
    const st = repoStatus(repo);
    return (
      <Gem
        key={repo.path}
        path={repo.path}
        x={p.x}
        z={-p.y}
        size={p.size}
        language={repo.language}
        dirty={st.dirty}
        interrupted={st.interrupted}
        working={st.working}
        waiting={st.waiting}
        refactor={refactorMark(repo.refactor)}
        remoteOnly={remoteOnly.has(repo.path)}
        leaving={gone}
        sheet={sheetUniforms}
        registry={gems}
      />
    );
  };

  return (
    <>
      <primitive object={sheet} />
      <primitive object={shards} />
      {/* One keyed list, so a repo that leaves keeps its Gem and handle while it fades. */}
      {[
        ...repos.map((repo) => gemFor(repo, layout.get(repo.path)!, false)),
        ...[...leaving.values()].map((l) => gemFor(l.repo, l.pos, true)),
      ]}
    </>
  );
}
