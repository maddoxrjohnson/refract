import {
  BufferAttribute,
  BufferGeometry,
  CanvasTexture,
  Color,
  Mesh,
  MeshBasicMaterial,
  PlaneGeometry,
  Points,
  PointsMaterial,
  SRGBColorSpace,
} from "three";
import { rng } from "./layout";

// What the glass refracts: a painted nebula far behind the gems and a cloud of dust among
// them. Both from the mockup.

/** A soft coloured plane behind everything, painted once on a 2D canvas. */
export function makeBackdrop(): Mesh<PlaneGeometry, MeshBasicMaterial> {
  const cv = document.createElement("canvas");
  cv.width = 1024;
  cv.height = 512;
  const g = cv.getContext("2d")!;
  const base = g.createRadialGradient(512, 250, 0, 512, 256, 620);
  base.addColorStop(0, "#1d2747");
  base.addColorStop(0.45, "#0e1222");
  base.addColorStop(1, "#040509");
  g.fillStyle = base;
  g.fillRect(0, 0, 1024, 512);
  g.globalCompositeOperation = "lighter";
  const blob = (x: number, y: number, r: number, c: string) => {
    const gr = g.createRadialGradient(x, y, 0, x, y, r);
    gr.addColorStop(0, c);
    gr.addColorStop(1, "rgba(0,0,0,0)");
    g.fillStyle = gr;
    g.fillRect(x - r, y - r, r * 2, r * 2);
  };
  blob(300, 200, 260, "rgba(70,120,255,0.35)");
  blob(760, 300, 240, "rgba(200,70,200,0.25)");
  blob(560, 120, 180, "rgba(255,170,90,0.22)");
  blob(180, 380, 160, "rgba(60,220,210,0.18)");
  blob(880, 120, 140, "rgba(140,110,255,0.25)");
  // A canvas filter costs a blur pass per shape, and software WebGL (SwiftShader) took
  // seconds over 74 of them. Blending is additive, so blurring a set of shapes at once
  // gives the same picture: each set is drawn sharp on a scratch canvas, then blurred on.
  const layer = document.createElement("canvas");
  layer.width = 1024;
  layer.height = 512;
  const l = layer.getContext("2d")!;
  l.globalCompositeOperation = "lighter";
  const blurOn = (px: number) => {
    g.filter = `blur(${px}px)`;
    g.drawImage(layer, 0, 0);
    g.filter = "none";
    l.clearRect(0, 0, 1024, 512);
  };
  l.strokeStyle = "rgba(220,235,255,0.55)";
  const streaks: [number, number, number, number, number][] = [
    [120, 470, 520, 40, 3],
    [480, 500, 980, 70, 2],
    [40, 300, 400, -20, 1.5],
    [620, 512, 1010, 230, 2.5],
  ];
  for (const [x1, y1, x2, y2, w] of streaks) {
    l.lineWidth = w;
    l.beginPath();
    l.moveTo(x1, y1);
    l.lineTo(x2, y2);
    l.stroke();
  }
  blurOn(4);
  const r = rng(3);
  for (let i = 0; i < 70; i++) {
    l.fillStyle = `rgba(${200 + r() * 55},${200 + r() * 55},255,${0.08 + r() * 0.25})`;
    l.beginPath();
    l.arc(r() * 1024, r() * 512, 2 + r() * 9, 0, 6.283);
    l.fill();
  }
  blurOn(2);
  layer.width = 0;
  const tex = new CanvasTexture(cv);
  tex.colorSpace = SRGBColorSpace;
  const m = new Mesh(
    new PlaneGeometry(76, 38),
    new MeshBasicMaterial({ map: tex, color: new Color(1.5, 1.5, 1.6) }),
  );
  m.position.z = -10;
  return m;
}

/** Specks scattered through the cluster, reaching `extent` from the centre. */
export function makeDust(extent: number): Points<BufferGeometry, PointsMaterial> {
  const rand = rng(11);
  const n = 420;
  const dp = new Float32Array(n * 3);
  for (let i = 0; i < n; i++) {
    const r = Math.sqrt(rand()) * (extent + 2);
    const a = rand() * 6.28;
    dp[i * 3] = Math.cos(a) * r;
    dp[i * 3 + 1] = Math.sin(a) * r * 0.65;
    dp[i * 3 + 2] = (rand() - 0.5) * 4;
  }
  return new Points(
    new BufferGeometry().setAttribute("position", new BufferAttribute(dp, 3)),
    new PointsMaterial({
      color: new Color(1.1, 1.25, 1.5),
      size: 0.03,
      transparent: true,
      opacity: 0.8,
    }),
  );
}

/** Frees a mesh or points object's geometry, material and texture. */
export function disposeObject(o: Mesh | Points) {
  o.geometry.dispose();
  const mat = o.material as MeshBasicMaterial;
  mat.map?.dispose();
  mat.dispose();
}
