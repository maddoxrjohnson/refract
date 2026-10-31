import { Color, Mesh, PlaneGeometry, ShaderMaterial, Vector3 } from "three";
import type { ThemeMode } from "../theme";

// The Banknote look (banknote spec §1): stones drawn as a banknote engraving on a paper
// sheet. Shaders are ported from docs/mockups/gem-studies.html with only the switches the
// Banknote look sets (HATCH 3, UNDER 3, MARK 0, stipple for remote). A waiting stone is
// drawn in red line-work and knocks, with red rings spreading from it.
//
// Colours are sRGB hex. THREE.Color converts them to linear, the shaders work in linear
// into a half-float target, and the OutputPass converts back, so bare paper shows on
// screen exactly as written.

export interface Palette {
  /** Sheet middle. */
  paper: number;
  /** Sheet rim and page background. */
  edge: number;
  /** Lines. */
  ink: number;
  /** Needs you: the waiting stone's line-work and the uncommitted-changes ring. */
  accent: number;
  /** How strongly the stone's colour tints its facets. */
  wash: number;
}

export const PALETTES: Record<ThemeMode, Palette> = {
  light: { paper: 0xeef0e4, edge: 0xe0e3d4, ink: 0x1f4a3d, accent: 0xb8322a, wash: 0.2 },
  dark: { paper: 0x0e1713, edge: 0x08100d, ink: 0xd8c78f, accent: 0xff7a5c, wash: 0.14 },
};

/** Uniforms every material shares by reference, so switching mode touches only these. */
export interface SheetUniforms {
  paper: { value: Color };
  edge: { value: Color };
  ink: { value: Color };
  accent: { value: Color };
  wash: { value: number };
  /** 1 in dark mode: lines mark the lit facets (scratchboard) and glints show as ink. */
  invert: { value: number };
  /** The drawing pixel ratio, so line spacing stays in CSS pixels. */
  px: { value: number };
}

export function makeSheetUniforms(): SheetUniforms {
  return {
    paper: { value: new Color() },
    edge: { value: new Color() },
    ink: { value: new Color() },
    accent: { value: new Color() },
    wash: { value: 0 },
    invert: { value: 0 },
    px: { value: 1 },
  };
}

export function applyPalette(u: SheetUniforms, mode: ThemeMode) {
  const p = PALETTES[mode];
  u.paper.value.setHex(p.paper);
  u.edge.value.setHex(p.edge);
  u.ink.value.setHex(p.ink);
  u.accent.value.setHex(p.accent);
  u.wash.value = p.wash;
  u.invert.value = mode === "dark" ? 1 : 0;
}

const GLSL_COMMON = /* glsl */ `
  vec3 hash3(vec3 p){ p = fract(p * vec3(0.1031, 0.1030, 0.0973)); p += dot(p, p.yxz + 33.33); return fract((p.xxy + p.yxx) * p.zyx); }
  float h2(vec2 p){ return fract(sin(dot(p, vec2(12.9898, 78.233))) * 43758.5453); }
  float lineAt(float s, float width){ float f = abs(fract(s) - 0.5) * 2.0, aa = fwidth(s) * 2.0; return 1.0 - smoothstep(width - aa, width + aa, f); }`;

const stoneVert = /* glsl */ `
  attribute vec3 bary;
  varying vec3 vN; varying vec3 vObjN; varying vec3 vW; varying vec3 vBary;
  void main(){
    mat4 m = modelMatrix;
    #ifdef USE_INSTANCING
      m = m * instanceMatrix;
    #endif
    vec4 wp = m * vec4(position, 1.0);
    vW = wp.xyz; vN = normalize(mat3(m) * normal); vObjN = normal; vBary = bary;
    gl_Position = projectionMatrix * viewMatrix * wp;
  }`;

// Wavy banknote lines per facet, thicker where the facet is darker (lit, when inverted),
// facet edges from the barycentric attribute, dots instead of lines for remote repos.
// `alarm` (an agent waiting on you) draws the line-work in the accent with bold facet
// edges, and a band of extra weight runs up through the stone as `alarmT` advances.
const stoneFrag = /* glsl */ `
  uniform vec3 paper; uniform vec3 ink; uniform vec3 accent; uniform vec3 tint;
  uniform float tintAmt; uniform float invert; uniform float px; uniform float fade;
  uniform float alarm; uniform float alarmT; uniform float ghost; uniform float spacing; uniform vec3 lightDir;
  varying vec3 vN; varying vec3 vObjN; varying vec3 vW; varying vec3 vBary;
  ${GLSL_COMMON}
  void main(){
    float d = min(min(vBary.x, vBary.y), vBary.z);
    vec2 p = gl_FragCoord.xy / px;
    vec3 N = normalize(vN), L = normalize(lightDir), V = normalize(cameraPosition - vW);
    float diff = clamp(dot(N, L), 0.0, 1.0);
    float spec = smoothstep(0.35, 0.7, pow(clamp(dot(reflect(-L, N), V), 0.0, 1.0), 24.0));
    float tone = clamp(1.0 - (0.12 + 0.95 * diff), 0.0, 1.0);
    vec3 h = hash3(floor(vObjN * 9.0 + 9.0));
    float run = pow(0.5 + 0.5 * sin(vW.y * 7.0 - alarmT * 3.0), 3.0);
    float e = 1.0 - smoothstep(0.0, fwidth(d) * (1.3 + alarm * (2.6 + 3.0 * run)), d);
    vec3 base = mix(paper, tint, tintAmt * (0.55 + 0.45 * tone));
    vec3 line = mix(ink, accent, alarm);
    float t = mix(tone, 1.0 - tone, invert);
    float ang = 0.5 + h.x * 2.2;
    vec2 dir = vec2(cos(ang), sin(ang)), perp = vec2(-dir.y, dir.x);
    float sw = dot(p, dir) / spacing + sin(dot(p, perp) * 0.11 + h.y * 6.28) * 0.7;
    float lines = lineAt(sw, 0.06 + t * 0.5 + alarm * (0.08 + 0.12 * run));
    lines = max(lines, step(0.6, t) * lineAt(dot(p, perp) / spacing + sin(dot(p, dir) * 0.09) * 0.7, (t - 0.6) * 0.8));
    float dots = step(h2(floor(p / 1.6)), 0.12 + t * 0.45);
    lines = mix(lines, dots, ghost);
    lines = mix(lines * (1.0 - spec), max(lines, spec), invert);
    vec3 col = mix(base, line, lines * 0.82);
    col = mix(col, line, e * 0.95);
    gl_FragColor = vec4(mix(paper, col, fade), 1.0);
  }`;

const sheetVert = /* glsl */ `
  varying vec3 vW;
  void main(){ vec4 w = modelMatrix * vec4(position, 1.0); vW = w.xyz; gl_Position = projectionMatrix * viewMatrix * w; }`;

// Paper fading to the edge colour, with faint fibre. The middle sits a little behind and
// left of the layout's centre, as in the mockup.
const sheetFrag = /* glsl */ `
  uniform vec3 paper; uniform vec3 edge; uniform float reach;
  varying vec3 vW;
  ${GLSL_COMMON}
  void main(){
    vec2 q = vW.xz - vec2(-0.6, -1.2);
    vec3 c = mix(paper, edge, smoothstep(0.0, 1.0, length(q) / reach));
    c *= 1.0 + (h2(floor(vW.xz * 40.0)) - 0.5) * 0.07;
    gl_FragColor = vec4(c, 1.0);
  }`;

const decalVert = /* glsl */ `
  varying vec2 vUv;
  void main(){ vUv = uv; gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0); }`;

// The guilloche rosette under each stone: six wavy rings; `spin` turns it while an agent works.
const rosetteFrag = /* glsl */ `
  uniform vec3 ink; uniform float fade; uniform float spin;
  varying vec2 vUv;
  void main(){
    float r = length(vUv - 0.5) * 2.0, a = 0.0;
    float ang = atan(vUv.y - 0.5, vUv.x - 0.5) + spin, w = fwidth(r) * 1.1;
    for (int k = 0; k < 6; k++) {
      float fk = float(k);
      float rk = 0.42 + fk * 0.1 + 0.04 * sin(ang * (10.0 + fk * 2.0) + fk * 1.7);
      a = max(a, 1.0 - smoothstep(w * 0.4, w * 1.3, abs(r - rk)));
    }
    gl_FragColor = vec4(ink, a * 0.28 * fade);
  }`;

// The hand-drawn red ring round a stone with uncommitted changes; `draw` sweeps it in.
const ringFrag = /* glsl */ `
  uniform vec3 col; uniform float seed; uniform float draw; uniform float fade;
  varying vec2 vUv;
  void main(){
    vec2 q = vUv - 0.5; float ang = atan(q.y, q.x), r = length(q) * 2.0;
    float a01 = fract((ang + seed) / 6.28318);
    float rr = 0.8 + 0.035 * sin(ang * 2.0 + seed) + 0.03 * a01, w = fwidth(r) * 1.4 + 0.006;
    float a = (1.0 - smoothstep(w * 0.5, w * 1.4, abs(r - rr))) * step(a01, draw);
    gl_FragColor = vec4(col, a * fade);
  }`;

const LIGHT_DIR = new Vector3(-0.45, 1, -0.6);
/** Line spacing in CSS pixels. */
const SPACING = 3.2;

export type StoneUniforms = {
  tint: { value: Color };
  fade: { value: number };
  /** 1 while an agent waits on you: red line-work and bold edges. */
  alarm: { value: number };
  /** Advances while ambient motion runs, moving the band of weight up the stone. */
  alarmT: { value: number };
  ghost: { value: number };
};

/**
 * The engraved stone material. `tintAmt` defaults to the palette's wash; splinters pass
 * their own zero so they stay paper.
 */
export function makeStoneMaterial(
  u: SheetUniforms,
  tint: Color,
  tintAmt: { value: number } = u.wash,
): ShaderMaterial & { uniforms: StoneUniforms } {
  return new ShaderMaterial({
    vertexShader: stoneVert,
    fragmentShader: stoneFrag,
    uniforms: {
      paper: u.paper,
      ink: u.ink,
      accent: u.accent,
      invert: u.invert,
      px: u.px,
      tintAmt,
      tint: { value: tint },
      fade: { value: 1 },
      alarm: { value: 0 },
      alarmT: { value: 0 },
      ghost: { value: 0 },
      spacing: { value: SPACING },
      lightDir: { value: LIGHT_DIR },
    },
  }) as ShaderMaterial & { uniforms: StoneUniforms };
}

export type RosetteUniforms = { fade: { value: number }; spin: { value: number } };

export function makeRosetteMaterial(u: SheetUniforms): ShaderMaterial & { uniforms: RosetteUniforms } {
  return new ShaderMaterial({
    vertexShader: decalVert,
    fragmentShader: rosetteFrag,
    transparent: true,
    depthWrite: false,
    uniforms: { ink: u.ink, fade: { value: 1 }, spin: { value: 0 } },
  }) as ShaderMaterial & { uniforms: RosetteUniforms };
}

// Red rings spreading over the sheet from a stone whose agent waits on you, two per
// period (`t` counts periods), engraved as a double line and fading as they widen.
const pingFrag = /* glsl */ `
  uniform vec3 col; uniform float t; uniform float fade;
  varying vec2 vUv;
  void main(){
    float r = length(vUv - 0.5) * 2.0, w = fwidth(r), a = 0.0;
    for (int k = 0; k < 2; k++) {
      float p = fract(t + float(k) * 0.5);
      float rk = 0.3 + 0.7 * p;
      float band = max(1.0 - smoothstep(w * 0.5, w * 1.6, abs(r - rk)),
                       0.7 * (1.0 - smoothstep(w * 0.4, w * 1.3, abs(r - rk + 0.035))));
      a = max(a, band * pow(1.0 - p, 1.4));
    }
    gl_FragColor = vec4(col, a * 0.9 * fade);
  }`;

export type PingUniforms = { fade: { value: number }; t: { value: number } };

export function makePingMaterial(u: SheetUniforms): ShaderMaterial & { uniforms: PingUniforms } {
  return new ShaderMaterial({
    vertexShader: decalVert,
    fragmentShader: pingFrag,
    transparent: true,
    depthWrite: false,
    uniforms: { col: u.accent, t: { value: 0 }, fade: { value: 1 } },
  }) as ShaderMaterial & { uniforms: PingUniforms };
}

export type RingUniforms = { fade: { value: number }; draw: { value: number } };

export function makeRingMaterial(u: SheetUniforms, seed: number): ShaderMaterial & { uniforms: RingUniforms } {
  return new ShaderMaterial({
    vertexShader: decalVert,
    fragmentShader: ringFrag,
    transparent: true,
    depthWrite: false,
    uniforms: { col: u.accent, seed: { value: seed }, draw: { value: 0 }, fade: { value: 1 } },
  }) as ShaderMaterial & { uniforms: RingUniforms };
}

/** A unit square lying flat on the sheet, for the rosette and the rings. */
export function makeDecalGeometry(): PlaneGeometry {
  return new PlaneGeometry(1, 1).rotateX(-Math.PI / 2);
}

/** Large enough that its rim is never in view, whatever the layout. */
const SHEET_SIZE = 240;

/** The paper sheet at y = 0; `reach` is where it has faded to the edge colour. */
export function makeSheet(u: SheetUniforms): Mesh<PlaneGeometry, ShaderMaterial & { uniforms: { reach: { value: number } } }> {
  const material = new ShaderMaterial({
    vertexShader: sheetVert,
    fragmentShader: sheetFrag,
    uniforms: { paper: u.paper, edge: u.edge, reach: { value: 6 } },
  }) as ShaderMaterial & { uniforms: { reach: { value: number } } };
  return new Mesh(new PlaneGeometry(SHEET_SIZE, SHEET_SIZE).rotateX(-Math.PI / 2), material);
}
