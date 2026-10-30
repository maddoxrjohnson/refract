// Glass and halo shaders, ported from docs/mockups/frozen-shatter.html.
//
// Screen-space refraction: the scene minus the shells is drawn to `uBack` first, then each
// facet bends its lookup into that image, with red, green and blue bent by slightly different
// amounts (dispersion). Reflections come from a procedural studio of softboxes. SAMPLES is a
// define, so changing it means a new material.

export const gemVert = /* glsl */ `
  varying vec3 vWorldPos; varying vec3 vViewPos;
  void main(){
    vec4 wp = modelMatrix * vec4(position, 1.0);
    vWorldPos = wp.xyz;
    vec4 mv = viewMatrix * wp;
    vViewPos = mv.xyz;
    gl_Position = projectionMatrix * mv;
  }`;

// Additions to the mockup's shader: uShimmer and uTime (a light band sweeping the facets
// while an agent works) and uDim (gems outside the filter fade).
export const gemFrag = /* glsl */ `
  uniform sampler2D uBack; uniform vec2 uRes; uniform vec3 uTint;
  uniform float uChroma; uniform float uRefract; uniform float uFocus; uniform float uSparkle; uniform float uIrid;
  uniform float uShimmer; uniform float uTime; uniform float uDim;
  varying vec3 vWorldPos; varying vec3 vViewPos;
  vec3 studio(vec3 r){
    vec3 c = vec3(0.012, 0.014, 0.022);
    c += vec3(4.2) * smoothstep(0.93, 0.985, dot(r, normalize(vec3(-0.35, 0.85, 0.4))));
    c += vec3(0.7, 0.95, 2.0) * smoothstep(0.86, 0.96, dot(r, normalize(vec3(0.75, 0.25, -0.6))));
    c += vec3(2.0, 1.05, 0.55) * smoothstep(0.88, 0.975, dot(r, normalize(vec3(0.85, -0.1, 0.5))));
    c += vec3(1.1) * smoothstep(0.975, 0.995, 1.0 - abs(r.y - 0.12)) * step(0.0, r.z);
    return c;
  }
  void main(){
    vec3 Nw = normalize(cross(dFdx(vWorldPos), dFdy(vWorldPos)));
    vec3 V = normalize(cameraPosition - vWorldPos);
    if (dot(Nw, V) < 0.0) Nw = -Nw;
    vec3 eye = normalize(vViewPos);
    vec3 Nv = normalize(cross(dFdx(vViewPos), dFdy(vViewPos)));
    if (dot(Nv, -eye) < 0.0) Nv = -Nv;
    vec2 uv = gl_FragCoord.xy / uRes;
    float iR = mix(1.0 / 1.18, 1.0 / 1.13, uChroma);
    float iG = 1.0 / 1.18;
    float iB = mix(1.0 / 1.18, 1.0 / 1.24, uChroma);
    vec3 col = vec3(0.0);
    for (int i = 0; i < SAMPLES; i++) {
      float s = SAMPLES > 1 ? float(i) / float(SAMPLES - 1) : 0.5;
      float slide = (s - 0.5) * 0.12 * uChroma;
      col.r += texture2D(uBack, uv + refract(eye, Nv, iR).xy * (uRefract + slide)).r;
      col.g += texture2D(uBack, uv + refract(eye, Nv, iG).xy * (uRefract + slide * 1.6)).g;
      col.b += texture2D(uBack, uv + refract(eye, Nv, iB).xy * (uRefract + slide * 2.2)).b;
    }
    col /= float(SAMPLES);
    col *= mix(vec3(1.0), uTint, 0.62);
    col += uTint * 0.05;
    float fres = pow(1.0 - max(dot(Nw, V), 0.0), 3.0);
    vec3 R = reflect(-V, Nw);
    vec3 irid = 0.5 + 0.5 * cos(6.2832 * (fres * 1.3 + vec3(0.0, 0.33, 0.67)));
    col = col * (1.0 - fres * 0.45) + studio(R) * (0.07 + fres * 0.9) + irid * fres * 0.3 * uIrid;
    float g = pow(max(dot(R, normalize(vec3(-0.35, 0.85, 0.4))), 0.0), 260.0)
            + pow(max(dot(R, normalize(vec3(0.6, 0.45, 0.65))), 0.0), 340.0)
            + pow(max(dot(R, normalize(vec3(-0.7, -0.2, 0.7))), 0.0), 400.0);
    col += vec3(7.0) * g * uSparkle;
    float band = pow(0.5 + 0.5 * sin(dot(vWorldPos, vec3(2.1, 1.4, 0.8)) * 2.4 - uTime * 2.2), 8.0);
    col += (uTint + 0.35) * band * 0.55 * uShimmer;
    col *= 1.0 + uFocus * 0.4;
    col *= uDim;
    gl_FragColor = vec4(col, 1.0);
  }`;

export const glowVert = /* glsl */ `varying vec3 vN; varying vec3 vV;
  void main(){ vN = normalize(normalMatrix*normal); vec4 mv = modelViewMatrix*vec4(position,1.0); vV = normalize(-mv.xyz); gl_Position = projectionMatrix*mv; }`;

export const glowFrag = /* glsl */ `uniform vec3 color; uniform float k; varying vec3 vN; varying vec3 vV;
  void main(){ float f = pow(1.0 - abs(dot(vN, vV)), 1.8); gl_FragColor = vec4(color * k * f, f * k); }`;
