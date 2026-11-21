// posegfx.ts — small 3D view of the goggles' head pose (Eyepiece head tracking).
//
// Plain canvas 2D, no 3D library: a goggles-shaped box with a forward arrow, seen from
// behind and slightly above the wearer, with perspective. Turning the head right turns the
// arrow right; looking up tilts it up; right ear down rolls the box clockwise.
// Pose convention (ep_pose_t): degrees, +yaw right, +pitch up, +roll right ear down.

type V3 = [number, number, number];

// body model: x right, y up, z backwards (forward = -z, away from the viewer)
const W = 0.9, H = 0.36, D = 0.3;                 // half sizes of the goggles box
const BOX: V3[] = [
  [-W, -H, -D], [W, -H, -D], [W, H, -D], [-W, H, -D],   // 0-3 front (lens side)
  [-W, -H, D], [W, -H, D], [W, H, D], [-W, H, D],       // 4-7 back
];
const FACES: { idx: number[]; front?: boolean }[] = [
  { idx: [0, 1, 2, 3], front: true }, { idx: [5, 4, 7, 6] },
  { idx: [4, 0, 3, 7] }, { idx: [1, 5, 6, 2] }, { idx: [3, 2, 6, 7] }, { idx: [4, 5, 1, 0] },
];
const ARROW: V3[] = [[0, 0, -D], [0, 0, -2.2]];
const HEAD: V3[] = [[-0.22, 0, -1.85], [0.22, 0, -1.85]];   // arrowhead barbs

const rad = (d: number) => d * Math.PI / 180;

function rotate(p: V3, yaw: number, pitch: number, roll: number): V3 {
  // R = Ry(-yaw) · Rx(pitch) · Rz(-roll)   (yaw, then pitch, then roll, intrinsic)
  let [x, y, z] = p;
  let c = Math.cos(rad(-roll)), s = Math.sin(rad(-roll));
  [x, y] = [x * c - y * s, x * s + y * c];
  c = Math.cos(rad(pitch)); s = Math.sin(rad(pitch));
  [y, z] = [y * c - z * s, y * s + z * c];
  c = Math.cos(rad(-yaw)); s = Math.sin(rad(-yaw));
  [x, z] = [x * c + z * s, -x * s + z * c];
  return [x, y, z];
}

export class PoseView {
  private ctx: CanvasRenderingContext2D;
  private pose = { yaw: 0, pitch: 0, roll: 0 };
  private queued = false;
  private hasPose = false;

  constructor(private canvas: HTMLCanvasElement) {
    this.ctx = canvas.getContext("2d")!;
    this.draw();
  }

  set(yaw: number, pitch: number, roll: number) {
    this.pose = { yaw, pitch, roll };
    this.hasPose = true;
    if (!this.queued) { this.queued = true; requestAnimationFrame(() => { this.queued = false; this.draw(); }); }
  }

  reset() { this.hasPose = false; this.pose = { yaw: 0, pitch: 0, roll: 0 }; this.draw(); }

  private draw() {
    const cv = this.canvas, dpr = window.devicePixelRatio || 1;
    const cw = cv.clientWidth || 160, ch = cv.clientHeight || 110;
    if (cv.width !== Math.round(cw * dpr) || cv.height !== Math.round(ch * dpr)) {
      cv.width = Math.round(cw * dpr); cv.height = Math.round(ch * dpr);
    }
    const g = this.ctx, css = getComputedStyle(cv);
    const col = (v: string, f: string) => css.getPropertyValue(v).trim() || f;
    const fg = col("--fg", "#ddd"), muted = col("--muted", "#888"), accent = col("--accent", "#4af"),
          line = col("--line", "#444"), panel = col("--panel", "#2a2d31");
    g.setTransform(dpr, 0, 0, dpr, 0, 0);
    g.clearRect(0, 0, cw, ch);

    // camera: behind and a little above/right of the head, looking forward
    const view = (p: V3): V3 => {
      let [x, y, z] = p;
      let c = Math.cos(rad(30)), s = Math.sin(rad(30));     // look down onto it
      [y, z] = [y * c - z * s, y * s + z * c];
      c = Math.cos(rad(0)); s = Math.sin(rad(0));            // centred: left/right turns look the same
      [x, z] = [x * c + z * s, -x * s + z * c];
      return [x, y, z];
    };
    const f = ch * 1.2, dist = 4.6, cx = cw / 2, cy = ch / 2 + ch * 0.08;
    const proj = (p: V3): [number, number, number] => {
      const [x, y, z] = view(p);
      const k = f / (dist - z);                              // camera on +z, behind the head
      return [cx + x * k, cy - y * k, z];
    };

    // reference: horizon ring at chin height and "straight ahead" tick
    g.strokeStyle = line; g.lineWidth = 1;
    g.beginPath();
    for (let i = 0; i <= 48; i++) {
      const a = (i / 48) * Math.PI * 2;
      const [px, py] = proj([Math.sin(a) * 1.5, -0.75, Math.cos(a) * 1.5]);
      if (i === 0) g.moveTo(px, py); else g.lineTo(px, py);
    }
    g.stroke();
    g.beginPath();
    { const [ax, ay] = proj([0, -0.75, -1.5]), [bx, by] = proj([0, -0.75, -1.9]); g.moveTo(ax, ay); g.lineTo(bx, by); }
    g.stroke();

    const { yaw, pitch, roll } = this.pose;
    const R = (p: V3) => rotate(p, yaw, pitch, roll);
    const box = BOX.map(R).map(proj);

    // faces back to front (painter's algorithm, average depth)
    const faces = FACES.map(fc => ({ ...fc, z: fc.idx.reduce((a, i) => a + box[i][2], 0) / 4 }))
      .sort((a, b) => a.z - b.z);                              // far (small z) first
    g.globalAlpha = this.hasPose ? 1 : 0.45;
    for (const fc of faces) {
      g.beginPath();
      fc.idx.forEach((i, n) => (n ? g.lineTo(box[i][0], box[i][1]) : g.moveTo(box[i][0], box[i][1])));
      g.closePath();
      g.fillStyle = fc.front ? accent : panel;
      g.globalAlpha = (this.hasPose ? 1 : 0.45) * (fc.front ? 0.9 : 0.75);
      g.fill();
      g.globalAlpha = this.hasPose ? 1 : 0.45;
      g.strokeStyle = fg; g.lineWidth = 1;
      g.stroke();
    }

    // the arrow's shadow on the ground ring: yaw alone, independent of pitch
    {
      const fw = R([0, 0, -2.2]), len = Math.hypot(fw[0], fw[2]);
      if (len > 0.05) {
        const [sx, sy] = proj([0, -0.75, 0]), [ex, ey] = proj([fw[0] / len * 1.5, -0.75, fw[2] / len * 1.5]);
        g.save(); g.setLineDash([3, 3]); g.strokeStyle = muted; g.lineWidth = 1.5;
        g.beginPath(); g.moveTo(sx, sy); g.lineTo(ex, ey); g.stroke(); g.restore();
      }
    }

    // forward arrow (looking direction)
    const [a0, a1] = ARROW.map(R).map(proj);
    g.strokeStyle = accent; g.lineWidth = 2;
    g.beginPath(); g.moveTo(a0[0], a0[1]); g.lineTo(a1[0], a1[1]); g.stroke();
    const tip = HEAD.map(R).map(proj);
    for (const t of tip) { g.beginPath(); g.moveTo(a1[0], a1[1]); g.lineTo(t[0], t[1]); g.lineWidth = 2; g.stroke(); }
    g.globalAlpha = 1;
  }
}
