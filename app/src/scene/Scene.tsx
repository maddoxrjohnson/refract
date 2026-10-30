import { Canvas, useFrame, useThree } from "@react-three/fiber";
import {
  BloomEffect,
  DepthOfFieldEffect,
  EffectComposer,
  EffectPass,
  RenderPass,
  ToneMappingEffect,
  ToneMappingMode,
  type Effect,
} from "postprocessing";
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
  MathUtils,
  Vector2,
  Vector3,
  WebGLRenderTarget,
  type Camera,
  type PerspectiveCamera,
} from "three";
import type { RepoSnapshot } from "../backend/types";
import { TIERS, type QualityLevel } from "../quality/tiers";
import { repoStatus } from "../repoStatus";
import { disposeObject, makeBackdrop, makeDust } from "./backdrop";
import { GLASS_LAYER, Gem, type GemHandle } from "./Gem";
import { linearHex } from "./languages";
import { layoutGems } from "./layout";
import { createTicker } from "./ticker";

// The Frozen shatter scene (spec §3.2), ported from docs/mockups/frozen-shatter.html.
//
// Each drawn frame renders twice: everything but the glass into `back`, then the whole
// scene through the post-processing chain, with the shells sampling `back` to refract.
// Frames are drawn on demand only: on a prop change, pointer movement or resize, and
// while something animates, paced to the tier's frame cap by ./ticker.ts. The loop body
// allocates nothing per frame beyond the label's transform string.

export interface SceneProps {
  repos: RepoSnapshot[];
  /** False stops the frame loop: nothing draws while hidden. */
  visible: boolean;
  quality: QualityLevel;
  /** No spin or breathing; camera moves still ease, but quickly. */
  reducedMotion: boolean;
  /** Paths of gems that are not cloned; they draw dimmed, without a lit core. */
  remoteOnly: ReadonlySet<string>;
  /** Filter result; `null` is no filter. Gems outside it dim and shrink a little. */
  matches: ReadonlySet<string> | null;
  /** Path of the gem that lifts, brightens and shows `focusLabel`. */
  focused: string | null;
  /** Path of the gem the camera flies to, left of centre so a panel fits on the right. */
  zoomed: string | null;
  /** Shown beside the focused gem; the scene moves it each frame. */
  focusLabel?: ReactNode;
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

const FOV = 36;
const MAGNET_PX = 130;
/** The focused gem wins magnet ties within this many pixels, so focus does not flicker. */
const MAGNET_HYSTERESIS_PX = 10;
/** How far a focused gem moves towards the camera. */
const LIFT = 0.35;
/** A zoomed gem's radius as a share of half the view height. */
const ZOOM_FILL = 0.34;
/** Where a zoomed gem sits horizontally, in normalised device coordinates. */
const ZOOM_X = -0.4;
/** How far a zoomed gem's facets and glow reach, as a share of half the view height. */
export const ZOOM_REACH = 0.48;
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
const EXPOSURE = 1.1;
const GL_OPTIONS = { antialias: false, powerPreference: "high-performance" } as const;
const CAMERA = { fov: FOV, near: 0.1, far: 120, position: [0, 0, 12] as [number, number, number] };

export function Scene(props: SceneProps) {
  const label = useRef<HTMLDivElement>(null);
  const dpr = Math.min(window.devicePixelRatio || 1, TIERS[props.quality].maxPixelRatio);
  return (
    <div className="scene">
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
        <SceneContents {...props} label={label} />
      </Canvas>
      <div className="scene-label" ref={label} hidden>
        {props.focusLabel}
      </div>
    </div>
  );
}

interface ContentsProps extends SceneProps {
  label: RefObject<HTMLDivElement | null>;
}

interface Post {
  composer: EffectComposer;
  dof: DepthOfFieldEffect | null;
}

/** Mutable state the frame loop and pointer handlers share; never drives a React render. */
interface LoopState {
  first: boolean;
  time: number;
  /** Pointer in CSS pixels over the canvas, and whether it is over it at all. */
  px: number;
  py: number;
  inside: boolean;
  /** Parallax offset, eased towards its target; both in -1 to 1 across the canvas. */
  ptr: Vector2;
  ptrTarget: Vector2;
  /** Where the camera looks, eased. */
  look: Vector3;
  /** The last magnet focus reported to `onPointerFocus`. */
  magnet: string | null;
  /** The pointer moved since the last frame, so the magnet focus may have changed. */
  moved: boolean;
  /** When the last drawn frame started, and whether it asked the ticker for another. */
  lastStart: number;
  followUp: boolean;
  /** The scene's breathing scale as of the last frame. */
  breathe: number;
  gem: GemFrame;
}

/** Per-frame inputs to `updateGem`, passed as `this` so the loop allocates no closure. */
interface GemFrame {
  /** Easing step: frame seconds times the reduced-motion pace. */
  k: number;
  /** Seconds of ambient motion this frame, and the running total. */
  dt: number;
  T: number;
  ambient: boolean;
  breathe: number;
  focused: string | null;
  matches: ReadonlySet<string> | null;
  /** Set when any gem is still easing towards its goal. */
  settling: boolean;
}

const tmp = new Vector3();
const camGoal = new Vector3();
const lookGoal = new Vector3();

/** Share of the way to a goal to move this frame. */
function ease(k: number, rate: number): number {
  return Math.min(1, k * rate);
}

/** Brightness of a gem that is not cloned, before filter dimming; focus lifts it a little. */
const REMOTE_DIM = 0.42;
const REMOTE_FOCUS_LIFT = 0.25;

/** A slow pulse between 0 and 1. */
function wave(T: number, speed: number, phase: number): number {
  return 0.5 + 0.5 * Math.sin(T * speed + phase);
}

function updateGem(this: GemFrame, g: GemHandle) {
  const fGoal = g.path === this.focused ? 1 : 0;
  const mGoal = !this.matches || this.matches.has(g.path) ? 1 : 0;
  g.f += (fGoal - g.f) * ease(this.k, 9);
  g.m += (mGoal - g.m) * ease(this.k, 6);
  if (Math.abs(fGoal - g.f) > 0.002 || Math.abs(mGoal - g.m) > 0.002) this.settling = true;
  else {
    g.f = fGoal;
    g.m = mGoal;
  }
  g.outer.position.copy(g.base).multiplyScalar(this.breathe);
  g.outer.position.z += LIFT * g.f;
  g.outer.scale.setScalar(1 - 0.15 * (1 - g.m));
  g.spinner.rotation.y += g.spin * this.dt;
  g.spinner.scale.setScalar(1 + 0.18 * g.f);
  const dim = (0.25 + 0.75 * g.m) * (g.remoteOnly ? REMOTE_DIM + REMOTE_FOCUS_LIFT * g.f : 1);
  const T = this.T;
  const u = g.shell.uniforms;
  u.uFocus.value = g.f;
  u.uDim.value = dim;
  u.uTime.value = T;
  g.core.color.copy(g.coreColor).multiplyScalar(dim);
  const ambient = this.ambient;
  if (g.halo) g.halo.uniforms.k.value = (ambient ? 0.25 + 0.6 * wave(T, 1.5, g.phase) : 0.7) * dim;
  if (g.wait) g.wait.uniforms.k.value = (ambient ? 0.15 + 0.75 * wave(T, 1.1, g.phase) : 0.6) * dim;
  if (g.edges) g.edges.opacity = (ambient ? 0.25 + 0.75 * wave(T, 2.6, g.phase) : 0.8) * dim;
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
  breathe: number,
  prefer: string | null,
): string | null {
  let best: string | null = null;
  let bestDist = MAGNET_PX;
  for (const g of gems.values()) {
    tmp.copy(g.base).multiplyScalar(breathe).project(camera);
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
  if (!g) {
    el.hidden = true;
    return;
  }
  tmp.setFromMatrixPosition(g.outer.matrixWorld);
  const dist = camera.position.distanceTo(tmp);
  const radiusPx = ((g.radius * g.outer.scale.x * g.spinner.scale.x) / (dist * tv)) * (H / 2);
  tmp.project(camera);
  const x = (tmp.x * 0.5 + 0.5) * W;
  const y = (-tmp.y * 0.5 + 0.5) * H;
  el.hidden = false;
  const lw = el.offsetWidth;
  const lh = el.offsetHeight;
  const lx = Math.max(8, Math.min(W - lw - 8, x + radiusPx + 14));
  const ly = Math.max(8, Math.min(H - lh - 8, y - lh / 2));
  el.style.transform = `translate(${lx}px, ${ly}px)`;
}

function SceneContents(props: ContentsProps) {
  const { repos, remoteOnly, quality, reducedMotion, matches, focused, zoomed, focusLabel, visible } =
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
  const bounds = useMemo(() => {
    let maxX = 0;
    let maxY = 0;
    for (const p of layout.values()) {
      maxX = Math.max(maxX, Math.abs(p.x) + p.radius);
      maxY = Math.max(maxY, Math.abs(p.y) + p.radius);
    }
    return { maxX, maxY };
  }, [layout]);
  const gems = useMemo(() => new Map<string, GemHandle>(), []);

  // The refraction image: everything but the glass, at the tier's share of the canvas.
  const back = useMemo(() => new WebGLRenderTarget(2, 2, { type: HalfFloatType }), []);
  const res = useMemo(() => new Vector2(1, 1), []);
  useEffect(() => () => back.dispose(), [back]);

  const backdrop = useMemo(makeBackdrop, []);
  useEffect(() => () => disposeObject(backdrop), [backdrop]);
  const dust = useMemo(() => makeDust(bounds.maxX), [bounds.maxX]);
  useEffect(() => () => disposeObject(dust), [dust]);

  useEffect(() => {
    scene.background = linearHex(0x040509);
    gl.toneMappingExposure = EXPOSURE;
    return () => {
      scene.background = null;
    };
  }, [scene, gl]);

  // Bloom (Medium, High), depth of field (High) and ACES tone mapping. Built in an effect,
  // not a memo, because disposing a composer empties it.
  const [post, setPost] = useState<Post | null>(null);
  useEffect(() => {
    const composer = new EffectComposer(gl, { frameBufferType: HalfFloatType });
    composer.addPass(new RenderPass(scene, camera));
    const effects: Effect[] = [];
    const dof = tier.depthOfField
      ? new DepthOfFieldEffect(camera, { focusDistance: 12, focusRange: 9, bokehScale: 1 })
      : null;
    if (dof) effects.push(dof);
    if (tier.bloom) {
      effects.push(
        new BloomEffect({
          luminanceThreshold: 0.85,
          luminanceSmoothing: 0.03,
          intensity: tier.depthOfField ? 1.2 : 1.0,
          radius: 0.55,
          mipmapBlur: true,
        }),
      );
    }
    effects.push(new ToneMappingEffect({ mode: ToneMappingMode.ACES_FILMIC }));
    composer.addPass(new EffectPass(camera, ...effects));
    setPost({ composer, dof });
    return () => {
      setPost(null);
      composer.dispose();
    };
  }, [gl, scene, camera, tier]);

  useEffect(() => {
    if (!post) return;
    post.composer.setSize(size.width, size.height, false);
    gl.getDrawingBufferSize(res);
    back.setSize(
      Math.max(2, Math.round(res.x * tier.backScale)),
      Math.max(2, Math.round(res.y * tier.backScale)),
    );
    invalidate();
  }, [post, gl, back, res, size, dpr, tier.backScale, invalidate]);

  const loop = useRef<LoopState>({
    first: true,
    time: 0,
    px: 0,
    py: 0,
    inside: false,
    ptr: new Vector2(),
    ptrTarget: new Vector2(),
    look: new Vector3(),
    magnet: null,
    moved: false,
    lastStart: 0,
    followUp: false,
    breathe: 1,
    gem: {
      k: 0,
      dt: 0,
      T: 0,
      ambient: false,
      breathe: 1,
      focused: null,
      matches: null,
      settling: false,
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
  // while things settle. The label's content is React's job; only whether there is one
  // matters here.
  const hasLabel = focusLabel != null;
  useEffect(() => {
    ticker.request(tier.fpsCap, loop.current.lastStart);
  }, [ticker, tier, repos, remoteOnly, reducedMotion, matches, focused, zoomed, hasLabel, visible]);

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
      const hit = nearestGem(
        gems,
        camera,
        b.width,
        b.height,
        s.px,
        s.py,
        s.breathe,
        latest.current.focused,
      );
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
    if (!post) return;
    const start = performance.now();
    const s = loop.current;
    if (s.followUp) latest.current.onFrameInterval(start - s.lastStart);
    // After an idle pause the clock's delta spans the pause; step as one capped frame.
    const rdt = s.followUp ? Math.min(delta, 0.1) : 1 / tier.fpsCap;
    const ambient = tier.ambientMotion && !reducedMotion;
    const dt = ambient ? rdt : 0;
    s.time += dt;
    const T = s.time;
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

    // The camera fits every gem, or frames the zoomed one left of centre and above the picker.
    const breathe = ambient ? 1 + 0.022 * Math.sin(T * 0.5) : 1;
    s.breathe = breathe;
    const zoomGem = zoomed ? gems.get(zoomed) : undefined;
    if (zoomGem) {
      const dist = zoomGem.radius / (tv * ZOOM_FILL);
      lookGoal.copy(zoomGem.base).multiplyScalar(breathe);
      lookGoal.x -= ZOOM_X * tv * aspect * dist;
      lookGoal.y -= zoomLift(H) * tv * dist;
      camGoal.set(lookGoal.x + s.ptr.x * 0.4, lookGoal.y - s.ptr.y * 0.25, lookGoal.z + dist);
    } else {
      const fit =
        Math.max((bounds.maxY + 0.5) / tv, (bounds.maxX + 0.5) / (tv * aspect)) + 1.0;
      lookGoal.set(0, 0, 0);
      camGoal.set(s.ptr.x * 1.6, -s.ptr.y * 1.0, fit);
    }
    if (s.first) {
      camera.position.copy(camGoal);
      s.look.copy(lookGoal);
      s.first = false;
    }
    camera.position.lerp(camGoal, ease(k, 3));
    s.look.lerp(lookGoal, ease(k, 3));
    if (camera.position.distanceTo(camGoal) > 0.003 || s.look.distanceTo(lookGoal) > 0.003) {
      settling = true;
    }
    camera.lookAt(s.look);

    const gf = s.gem;
    gf.k = k;
    gf.dt = dt;
    gf.T = T;
    gf.ambient = ambient;
    gf.breathe = breathe;
    gf.focused = focused;
    gf.matches = matches;
    gf.settling = false;
    gems.forEach(updateGem, gf);
    if (gf.settling) settling = true;
    dust.scale.setScalar(breathe);
    scene.updateMatrixWorld();
    camera.updateMatrixWorld();

    // Only real pointer movement changes the magnet focus; a camera flight under a still
    // pointer does not.
    if (s.inside && s.moved) {
      s.moved = false;
      const hit = nearestGem(gems, camera, W, H, s.px, s.py, breathe, focused);
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

    if (post.dof) post.dof.cocMaterial.focusDistance = camera.position.distanceTo(s.look);

    // Pass 1: everything except the glass, into the texture the shells refract.
    camera.layers.set(0);
    gl.setRenderTarget(back);
    gl.render(scene, camera);
    gl.setRenderTarget(null);
    // Pass 2: the full scene through glow, depth of field and tone mapping.
    camera.layers.enable(GLASS_LAYER);
    post.composer.render(rdt);

    s.lastStart = start;
    s.followUp = ambient || settling;
    if (s.followUp) ticker.request(tier.fpsCap, start);
    else ticker.cancel();
  }, 1);

  return (
    <>
      <primitive object={backdrop} />
      <primitive object={dust} />
      {repos.map((repo) => {
        const p = layout.get(repo.path)!;
        const st = repoStatus(repo);
        return (
          <Gem
            key={repo.path}
            path={repo.path}
            x={p.x}
            y={p.y}
            z={p.z}
            radius={p.radius}
            language={repo.language}
            dirty={st.dirty}
            interrupted={st.interrupted}
            working={st.working}
            waiting={st.waiting}
            remoteOnly={remoteOnly.has(repo.path)}
            tier={tier}
            back={back.texture}
            res={res}
            registry={gems}
          />
        );
      })}
    </>
  );
}
