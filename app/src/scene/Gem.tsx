import { memo, useEffect, useLayoutEffect, useMemo, useRef } from "react";
import { Color, Group, Vector3, type Mesh, type ShaderMaterial } from "three";
import { stoneFor } from "../stones";
import {
  makeDecalGeometry,
  makeRingMaterial,
  makeRosetteMaterial,
  makeStoneMaterial,
  type RingUniforms,
  type RosetteUniforms,
  type SheetUniforms,
  type StoneUniforms,
} from "./banknote";
import { cutFor, stoneGeometry } from "./cuts";
import { hashString, rng } from "./layout";

/** How high a resting stone's centre sits above the sheet, as a share of its size. */
export const REST_HEIGHT = 0.92;

/** What the scene's frame loop animates on each stone. */
export interface GemHandle {
  path: string;
  /** Resting centre from the layout; `home` eases towards it when the layout changes. */
  base: Vector3;
  home: Vector3;
  size: number;
  /** Lifts, scales and flies in; holds the tilted, turning stone. */
  holder: Group;
  spinner: Mesh;
  /** On the sheet under the stone: the rosette and the ring. */
  spot: Group;
  stone: ShaderMaterial & { uniforms: StoneUniforms };
  rosette: ShaderMaterial & { uniforms: RosetteUniforms };
  ring: (ShaderMaterial & { uniforms: RingUniforms }) | null;
  working: boolean;
  waiting: boolean;
  /** Gone from the repos; fades out, then the scene drops it. */
  leaving: boolean;
  /** Seeded motion: turn phase and direction, opening delay, where the burst starts. */
  phase: number;
  spinDir: number;
  delay: number;
  start: Vector3;
  spinIn: number;
  /** Scene clock when the loop first saw it (NaN before), and whether it came in with the opening. */
  born: number;
  burst: boolean;
  /** Scene clock when the ring starts drawing in; NaN while there is no ring. */
  ringAt: number;
  /** Focus, eased from 0 to 1. */
  f: number;
  /** Filter match, eased: 1 matches, 0 is filtered out. */
  m: number;
  /** Presence, eased: 1 shown, 0 gone. */
  life: number;
}

export interface GemProps {
  path: string;
  /** Resting place on the sheet: x to the right, z towards the viewer. */
  x: number;
  z: number;
  size: number;
  language: string | null;
  dirty: boolean;
  interrupted: boolean;
  working: boolean;
  waiting: boolean;
  /** Exists only on GitHub or GitLab: stippled instead of lined. */
  remoteOnly: boolean;
  leaving: boolean;
  sheet: SheetUniforms;
  registry: Map<string, GemHandle>;
}

/** Frees GPU resources when they are replaced or the stone goes away. */
function useDispose(resource: { dispose(): void } | null) {
  useEffect(() => () => resource?.dispose(), [resource]);
}

export const Gem = memo(function Gem(props: GemProps) {
  const { path, size, sheet, registry } = props;
  const seed = hashString(path);

  const geometry = useMemo(() => stoneGeometry(cutFor(path), props.interrupted), [path, props.interrupted]);
  const decal = useMemo(makeDecalGeometry, []);
  const motion = useMemo(() => {
    const r = rng(seed);
    return {
      tilt: [(r() - 0.5) * 0.5, 0, (r() - 0.5) * 0.5] as [number, number, number],
      phase: r() * 6.28,
      spinDir: r() < 0.5 ? -1 : 1,
      delay: r() * 0.06,
      start: new Vector3((r() - 0.5) * 0.4, 1.6 + r(), (r() - 0.5) * 0.4),
      spinIn: (r() - 0.5) * 10,
      ringSeed: r() * 6.28,
    };
  }, [seed]);
  const stone = useMemo(() => makeStoneMaterial(sheet, new Color()), [sheet]);
  const rosette = useMemo(() => makeRosetteMaterial(sheet), [sheet]);
  const ring = useMemo(
    () => (props.dirty ? makeRingMaterial(sheet, motion.ringSeed) : null),
    [props.dirty, sheet, motion.ringSeed],
  );
  useDispose(geometry);
  useDispose(decal);
  useDispose(stone);
  useDispose(rosette);
  useDispose(ring);

  const holder = useRef<Group>(null);
  const spinner = useRef<Mesh>(null);
  const spot = useRef<Group>(null);
  const handle = useRef<GemHandle | null>(null);

  // Keep the handle the frame loop reads in step with the latest props.
  useLayoutEffect(() => {
    stone.uniforms.tint.value.setHex(stoneFor(props.language).hex);
    stone.uniforms.ghost.value = props.remoteOnly ? 1 : 0;
    const h: GemHandle = handle.current ?? {
      path,
      base: new Vector3(),
      home: new Vector3(),
      size,
      holder: holder.current!,
      spinner: spinner.current!,
      spot: spot.current!,
      stone,
      rosette,
      ring,
      working: false,
      waiting: false,
      leaving: false,
      phase: motion.phase,
      spinDir: motion.spinDir,
      delay: motion.delay,
      start: motion.start,
      spinIn: motion.spinIn,
      born: Number.NaN,
      burst: false,
      ringAt: Number.NaN,
      f: 0,
      m: 1,
      life: 1,
    };
    h.base.set(props.x, size * REST_HEIGHT, props.z);
    if (!handle.current) h.home.copy(h.base);
    h.size = size;
    h.spinner = spinner.current!;
    h.stone = stone;
    h.rosette = rosette;
    h.ring = ring;
    h.working = props.working;
    h.waiting = props.waiting;
    h.leaving = props.leaving;
    handle.current = h;
    registry.set(path, h);
  });
  useLayoutEffect(() => () => void registry.delete(path), [registry, path]);

  return (
    <>
      <group ref={holder} dispose={null}>
        <group rotation={motion.tilt}>
          <mesh ref={spinner} geometry={geometry} material={stone} scale={size} />
        </group>
      </group>
      <group ref={spot} dispose={null}>
        <mesh geometry={decal} material={rosette} position-y={0.004} scale={size * 2.5} />
        {ring && <mesh geometry={decal} material={ring} position-y={0.006} scale={size * 3.1} />}
      </group>
    </>
  );
});
