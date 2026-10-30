import { memo, useEffect, useLayoutEffect, useMemo, useRef } from "react";
import {
  AdditiveBlending,
  Color,
  EdgesGeometry,
  Group,
  IcosahedronGeometry,
  LineBasicMaterial,
  MeshBasicMaterial,
  ShaderMaterial,
  Vector3,
  type Object3D,
  type Texture,
  type Vector2,
} from "three";
import { ConvexGeometry } from "three/examples/jsm/geometries/ConvexGeometry.js";
import type { Tier } from "../quality/tiers";
import { languageColor } from "./languages";
import { hashString, rng } from "./layout";
import { glowFrag, glowVert, gemFrag, gemVert } from "./shaders";

/**
 * Layer for everything drawn on top of the refraction image: shells, halos and edges.
 * The first pass renders layer 0 only, so the glass never samples itself.
 */
export const GLASS_LAYER = 1;

/** What the scene's frame loop animates on each gem. */
export interface GemHandle {
  path: string;
  /** Resting position from the layout, before the scene breathes. */
  base: Vector3;
  radius: number;
  outer: Group;
  spinner: Group;
  shell: ShaderMaterial;
  core: MeshBasicMaterial;
  /** The core's undimmed colour. */
  coreColor: Color;
  /** Blue halo: uncommitted changes. */
  halo: ShaderMaterial | null;
  /** Amber glow: an agent is waiting for input. */
  wait: ShaderMaterial | null;
  /** Amber edges: a session was cut off. */
  edges: LineBasicMaterial | null;
  /** Not cloned: drawn dimmer, with no lit core. */
  remoteOnly: boolean;
  phase: number;
  spin: number;
  /** Focus, eased from 0 to 1. */
  f: number;
  /** Filter match, eased: 1 matches, 0 is filtered out. */
  m: number;
}

export interface GemProps {
  path: string;
  x: number;
  y: number;
  z: number;
  radius: number;
  language: string | null;
  dirty: boolean;
  interrupted: boolean;
  working: boolean;
  waiting: boolean;
  /** Exists only on GitHub or GitLab: dimmed, with no core glow (spec §3.2). */
  remoteOnly: boolean;
  tier: Tier;
  /** The refraction image and its size in pixels, shared by every shell. */
  back: Texture;
  res: Vector2;
  registry: Map<string, GemHandle>;
}

/** A rough-cut hull: `facets` points on a jittered Fibonacci sphere. */
function roughGeometry(r: number, facets: number, rand: () => number): ConvexGeometry {
  const sy = 0.86 + rand() * 0.2;
  const pts: Vector3[] = [];
  for (let i = 0; i < facets; i++) {
    const y = 1 - (2 * (i + 0.5)) / facets;
    const rad = Math.sqrt(1 - y * y);
    const th = i * 2.39996 + (rand() - 0.5) * 0.7;
    const k = r * (0.9 + rand() * 0.12);
    pts.push(new Vector3(Math.cos(th) * rad * k, y * k * sy, Math.sin(th) * rad * k));
  }
  return new ConvexGeometry(pts);
}

const glowMaterial = (color: Color) =>
  new ShaderMaterial({
    vertexShader: glowVert,
    fragmentShader: glowFrag,
    uniforms: { color: { value: color }, k: { value: 0.7 } },
    transparent: true,
    blending: AdditiveBlending,
    depthWrite: false,
  });

const onGlass = (o: Object3D | null) => {
  o?.layers.set(GLASS_LAYER);
};

/** Frees GPU resources when they are replaced or the gem goes away. */
function useDispose(resource: { dispose(): void } | null) {
  useEffect(() => () => resource?.dispose(), [resource]);
}

export const Gem = memo(function Gem(props: GemProps) {
  const { path, radius, tier, back, res, registry } = props;
  const seed = hashString(path);

  // Shape depends on the tier's facet count; turn and phase do not.
  const geometry = useMemo(
    () => roughGeometry(radius, tier.facets, rng(seed ^ 0x5bd1e995)),
    [radius, tier.facets, seed],
  );
  const edgesGeometry = useMemo(
    () => (props.interrupted ? new EdgesGeometry(geometry, 1) : null),
    [geometry, props.interrupted],
  );
  const coreGeometry = useMemo(() => new IcosahedronGeometry(radius * 0.3, 0), [radius]);
  const haloGeometry = useMemo(() => new IcosahedronGeometry(radius * 1.32, 3), [radius]);
  const waitGeometry = useMemo(() => new IcosahedronGeometry(radius * 1.2, 3), [radius]);
  const motion = useMemo(() => {
    const rand = rng(seed);
    return {
      rotation: [-0.5 + rand() * 1.0, rand() * 6.28, rand() * 0.6] as [number, number, number],
      phase: rand() * 6.28,
      spin: 0.1 + rand() * 0.16,
    };
  }, [seed]);

  const shell = useMemo(
    () =>
      new ShaderMaterial({
        vertexShader: gemVert,
        fragmentShader: gemFrag,
        defines: { SAMPLES: tier.samples },
        uniforms: {
          uBack: { value: back },
          uRes: { value: res },
          uTint: { value: new Color() },
          uChroma: { value: 0 },
          uRefract: { value: 0.32 },
          uFocus: { value: 0 },
          uSparkle: { value: 0 },
          uIrid: { value: 0 },
          uShimmer: { value: 0 },
          uTime: { value: 0 },
          uDim: { value: 1 },
        },
      }),
    [tier.samples, back, res],
  );
  const core = useMemo(() => new MeshBasicMaterial(), []);
  const halo = useMemo(
    () => (props.dirty ? glowMaterial(new Color(0.35, 0.8, 1.6)) : null),
    [props.dirty],
  );
  const wait = useMemo(
    () => (props.waiting ? glowMaterial(new Color(1.6, 0.85, 0.25)) : null),
    [props.waiting],
  );
  const edges = useMemo(
    () =>
      props.interrupted
        ? new LineBasicMaterial({ color: new Color(2.2, 1.25, 0.35), transparent: true })
        : null,
    [props.interrupted],
  );
  useDispose(geometry);
  useDispose(edgesGeometry);
  useDispose(coreGeometry);
  useDispose(haloGeometry);
  useDispose(waitGeometry);
  useDispose(shell);
  useDispose(core);
  useDispose(halo);
  useDispose(wait);
  useDispose(edges);

  const outer = useRef<Group>(null);
  const spinner = useRef<Group>(null);
  const handle = useRef<GemHandle | null>(null);

  // Keep the handle the frame loop reads in step with the latest props.
  useLayoutEffect(() => {
    const tint = languageColor(props.language);
    shell.uniforms.uTint.value.copy(tint).lerp(new Color(1, 1, 1), 0.25);
    shell.uniforms.uChroma.value = tier.chroma;
    shell.uniforms.uSparkle.value = tier.sparkle;
    shell.uniforms.uIrid.value = tier.irid;
    shell.uniforms.uShimmer.value = props.working ? 1 : 0;
    const h: GemHandle = handle.current ?? {
      path,
      base: new Vector3(),
      radius,
      outer: outer.current!,
      spinner: spinner.current!,
      shell,
      core,
      coreColor: new Color(),
      halo,
      wait,
      edges,
      remoteOnly: props.remoteOnly,
      phase: motion.phase,
      spin: motion.spin,
      f: 0,
      m: 1,
    };
    h.base.set(props.x, props.y, props.z);
    h.radius = radius;
    h.shell = shell;
    h.core = core;
    h.coreColor.copy(tint).multiplyScalar(2.6);
    h.halo = halo;
    h.wait = wait;
    h.edges = edges;
    h.remoteOnly = props.remoteOnly;
    handle.current = h;
    registry.set(path, h);
  });
  useLayoutEffect(() => () => void registry.delete(path), [registry, path]);

  return (
    <group ref={outer} dispose={null}>
      <group ref={spinner} rotation={motion.rotation}>
        <mesh ref={onGlass} geometry={geometry} material={shell} />
        {!props.remoteOnly && <mesh geometry={coreGeometry} material={core} />}
        {edgesGeometry && edges && (
          <lineSegments ref={onGlass} geometry={edgesGeometry} material={edges} scale={1.006} />
        )}
      </group>
      {halo && <mesh ref={onGlass} geometry={haloGeometry} material={halo} />}
      {wait && <mesh ref={onGlass} geometry={waitGeometry} material={wait} />}
    </group>
  );
});
