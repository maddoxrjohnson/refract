# Banknote scene: spec

Replaces the Frozen shatter look (spec §3.2, §3.6) with **Banknote**, chosen by Maddox on 2026-10-30
after four rounds of prototypes. The reference is `docs/mockups/gem-studies.html` (opens on Banknote,
key 6). Where this spec and the mockup disagree, this spec wins; where this spec is silent, copy the
mockup. Shader code, cut tables, colours and timings below come from the mockup and should be ported
as they are, not re-invented.

## 1. The look

Each repo is a real cut stone drawn as a banknote engraving: fine wavy line-work on every facet, a
guilloche rosette printed on the sheet under it, a pale wash of the stone's colour, plain paper
around. No glow, no bloom, no glitter, no wireframe edges, no halos.

- **Sheet.** A large paper plane (y = 0). Colour fades from `paper` near the middle to `edge` at
  ~1.5 × the layout radius; faint paper fibre noise (mockup `floorFrag`, fibre 1, grid 0, border 0).
  The page background behind it is `edge`.
- **Stones.** Mockup `stoneVert`/`stoneFrag` with `HATCH 3` (wavy banknote lines), spacing 3.2 px,
  no wobble, no graphite, no hidden edges. Facet edges are drawn from the barycentric attribute
  (`prepare()`), so only real facet edges show. Light from (-0.45, 1, -0.6).
  Line spacing is in CSS pixels (`px` uniform = the drawing pixel ratio), so lines keep their
  size at every resolution.
- **Rosette.** Mockup `underFrag` with `UNDER 3`: six wavy rings, 2.5 × the stone's size, ink at
  28% opacity, on the sheet under each stone.
- **Wash.** `tintAmt` = `wash` from the palette; the stone colour comes from its language (§3).
- **Dark.** Gold ink on deep green, scratchboard style: `invert = 1`, so the lines mark the lit
  facets and the specular glint shows as ink.

### 1.1 Colours (sRGB hex, exactly as the mockup shows them)

| Token | Light | Dark |
|---|---|---|
| paper (sheet middle, panels) | `#eef0e4` | `#0e1713` |
| edge (sheet rim, page background) | `#e0e3d4` | `#08100d` |
| ink (lines, text) | `#1f4a3d` | `#d8c78f` |
| accent (red: needs you) | `#b8322a` | `#ff7a5c` |
| wash (stone tint amount) | 0.20 | 0.14 |

The mockup let three.js convert these sRGB hex values to linear and an OutputPass convert back, so
they appear on screen exactly as written. The port must do the same: a screenshot pixel of bare
paper reads `#eef0e4` (light) / `#0e1713` (dark) within 2/255. A test checks this (§8).

## 2. Status

| State (from `repoStatus`) | Banknote mark |
|---|---|
| Uncommitted changes (`dirty`) | Hand-drawn red ring on the sheet around the stone (mockup `markFrag` `MARK 0`, accent, 3.1 × size). It draws itself in over 0.6 s when it first appears (at 1.3 s into the opening; at once when a repo becomes dirty later). |
| Agent working (`working`) | The rosette turns (0.5 rad/s) and the stone turns faster (0.45 rad/s instead of 0.12). |
| Waiting for you (`waiting`) | Red wash, slow pulse: `accentAmt = 0.08 + 0.30 × (0.5 + 0.5 sin(1.57 t + phase))`, `waitMode 0` (the wash turns red, not the lines). |
| Stopped mid-task (`interrupted`) | Chipped corner: the geometry loses a corner (mockup `chip(geo, (0.9, 0.5, 0.35), 0.42)`, then re-hulled). Static. |
| Not cloned (remote-only) | Stippled: dots instead of lines (`ghost 1`, `ghostMode 0`). Edges stay solid. |
| Clean | Plain line-work. |

States combine: a dirty, waiting, chipped stone shows the ring, the pulse and the chip together.

**Reduced motion** (system setting): no opening burst (everything starts settled), no turning, no
rosette spin, the waiting wash holds at its middle value (0.08 + 0.30 × 0.6), rings appear fully drawn.

## 3. Stones

- **Cut.** One of the mockup's six cuts (round, emerald, brilliant, oval, princess, cushion; port the
  `CUTS` table and `cutGeometry`), picked from `hashString(path)`, so a repo keeps its cut.
- **Size.** `size = (0.42 + 0.52 × mass) × shrink`, with `shrink = min(1, sqrt(14 / count))` as today.
- **Turn.** Each stone sits with a small seeded tilt (±0.25 rad on x and z) and turns slowly about
  its own vertical axis (0.12 rad/s, seeded direction and phase).
- **Colour and name** by language (the label shows the stone's name, e.g. "GitHub · TypeScript ·
  sapphire"):

| Language | Stone | Hex |
|---|---|---|
| TypeScript | sapphire | `#3d63c9` |
| Python | citrine | `#e0a93a` |
| Rust | mandarin garnet | `#d9672e` |
| Go | aquamarine | `#8fd3d8` |
| C | white topaz | `#e8ecf0` |
| Kotlin | amethyst | `#8a62c8` |
| Swift | rhodolite | `#c23a5a` |
| Shell | peridot | `#9cc650` |
| JavaScript | yellow sapphire | `#e3c34a` |
| C# | tanzanite | `#5a54c4` |
| anything else / none | quartz | `#b9b4a8` |

## 4. Layout, camera, motion

- **Layout** (mockup "Layout"): golden-angle spiral on the sheet (x, z), heaviest first, then 260
  rounds of pull-to-centre plus pairwise push-apart so no two stones' discs (radius 1.22 × size)
  overlap. Sorted by mass then path, so the result does not depend on arrival order. Pure function,
  unit-tested (no overlaps, stable under reordering, bounded radius).
- **Camera.** Perspective, fov 28°, looking at (0, 0, 0.3) from 56° above the sheet; distance fits
  the layout radius in both directions (mockup `resize()`), recomputed on resize and when repos
  change. The pointer tilts the view: elevation ±4°, azimuth ±0.12 rad, eased.
- **Opening.** When the scene first has repos after the app starts: stones and splinters burst from
  just above the centre and settle (stones 1.5 s, splinters 1.4 s, `easeOutExpo`, seeded delays), as
  in the mockup. It plays once per app start, not on every show. A repo that appears later drops into
  its place from above (0.8 s); a repo that goes away fades out. Splinters stay on the sheet as
  paper-coloured engraved shards (no wash), placed around the layout.
- **Focus** (magnet, ~130 px, as today): the focused stone rises 0.3 and grows 6%; the label sits
  beside it.
- **Filter**: stones outside the filter fade towards the paper (`fade` 0.22), rosette and marks too.
- **Zoom** (click, or Enter on a focused stone): the camera flies in and tilts down to about 35°
  above the sheet, framing the stone left of centre (`ZOOM_X`, `ZOOM_FILL`, `zoomLift` keep their
  meaning so the detail panel and picker stay clear). Escape flies back. The other stones stay where
  they are, at full strength.
- Keyboard navigation, the picker, the detail panel, marks (`marks` prop: drive marker, Move to WSL
  ring) and the focus label keep their current behaviour and props. Arrow-key neighbours use the new
  on-screen positions.

## 5. Light and dark

- New setting `theme`: `"system" | "light" | "dark"`, default `"system"` (Rust `Settings.theme`,
  lenient: an unknown value reads as system). System follows the Windows app mode, which WebView2
  reports as `prefers-color-scheme`.
- `src/theme.ts`: `useThemeMode(setting): "light" | "dark"` resolves it, sets `data-theme` on
  `<html>` when overridden (removes it for system), and in the Tauri app also calls the window's
  `setTheme` so the title bar matches. The resolved mode is a `Scene` prop.
- Switching rebuilds materials' colour uniforms only; it must not replay the opening or move the camera.
- Settings panel gets an "Appearance" row: Light / Dark / System radios. The demo has it too.

## 6. Panels and widgets: paper and ink

Every overlay (focus label, detail panel, picker, filter bar, widgets, settings, toasts, hook offer,
Move dialog, demo terminal and badge, gem marks) moves to the palette:

- CSS tokens in `ui.css` under `:root` (light) and `:root[data-theme="dark"]` plus
  `@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) }` (dark):
  `--bg` = edge, `--panel` = paper at 0.86 with a 14 px backdrop blur, `--line` = ink at 0.17,
  `--fg` = ink, `--muted` = paper mixed towards ink (78% light, 62% dark, the least that passes WCAG AA), `--on-fg` = paper, `--accent` = accent.
- **Red means "needs you"**: waiting, stopped mid-task, failures, uncommitted-changes ring, the
  "Go to the running agent" hint when waiting. Everything else is ink. The old blue glass, amber and
  halo colours go away.
- Primary button: ink background, paper text. Secondary: paper with a hairline ink border.
  Radius 12 px for panels, 999 px for pill controls, 8 px for buttons. No glows or coloured shadows;
  at most a soft neutral shadow in light mode.
- Status swatches (label, detail panel): mockup `.sw` classes (ring for clean, red ring for dirty,
  ink dot working, red dot waiting, chipped square interrupted, dashed ring remote).
- Fonts stay IBM Plex Sans / Mono (bundled, offline).
- `.scene` background becomes `var(--bg)`; `color-scheme` follows the mode.

## 7. Quality levels

The look is identical at every level. Levels change only:

| | Low | Medium | High |
|---|---|---|---|
| Pixel ratio cap | 1× | 1.5× | 2× |
| MSAA samples | 0 | 4 | 4 |
| Splinters | 40 | 90 | 90 |
| Frame cap | 30 fps, draws only on change | 30 fps | 60 fps |

"Draws only on change" on Low keeps the existing rule (stones do not turn and the rosette does not
spin, so an idle scene draws nothing while a game runs); the opening, focus, zoom and filter still
animate. Auto selection, GPU counters, frame-interval stepping and the drop to Low under GPU load
stay as they are (`quality/controller.ts` untouched).

MSAA uses three's own `EffectComposer` (`three/examples/jsm`) with `RenderPass` + `OutputPass` into a
multisampled half-float target, as the mockup does; Low may render straight to the canvas if colours
still match §1.1.

## 8. What goes and what stays

Removed: `scene/shaders.ts` (refraction), `scene/backdrop.ts` (backdrop and dust), the two-pass
render and `GLASS_LAYER`, bloom, depth of field, ACES tone mapping, the core glow, halos, amber edges,
the `postprocessing` and `@react-three/postprocessing` dependencies, and the `Tier` fields `samples`,
`chroma`, `backScale`, `sparkle`, `irid`, `facets`, `bloom`, `depthOfField`.

Kept: `Scene` props and callbacks (plus `theme`), `ticker.ts` (on-demand frames, frame cap),
`onFrameInterval`, magnet focus with hysteresis, `marks`, the focus label, lazy loading and
`SceneBoundary`, the WebGL failure message, `frameloop="never"` while hidden, no per-frame allocation.

Tests:
- Unit: layout (no overlaps, stable, bounded), cut picking stable per path, stone table fallback,
  theme resolution (system/light/dark, media change), settings round trip with `theme` (TS and Rust,
  unknown value → system), `zoomLift` unchanged.
- E2E (demo build): existing 7 tests stay green; new test samples a bare-paper pixel of the canvas in
  light and in dark (switched from Settings) against §1.1, and checks the opening has settled.

Docs: spec §3.2 and §3.6 rewritten for Banknote; `docs/architecture.md` scene section; ADR 0017
"Banknote scene replaces Frozen shatter"; `docs/mockups/gem-studies.html` committed; README media
re-recorded at the end.
