// eyepiece.ts — "Eyepiece" low-latency PC stream to the goggles over USB.
//
//   getDisplayMedia → (MediaStreamTrackProcessor) VideoFrame → [GPU canvas: 16:9 + black bars]
//   → WebCodecs VideoEncoder (H.264 High, realtime, hardware) → Annex-B
//   → eyepiece protocol over adb.createSocket("tcp:42420") → eyepiece_bridge on the goggles
//   → VC8000D hardware decode → zero-copy scan-out on the display's VIDEO0 plane.
//
// Wire protocol: 8-byte little-endian header (magic 'E','P', type, flags, u32 payload length)
// + payload. Unlike the JPEG "PC Stream" (fb_receiver, CPU decode, ~5–7 fps), the goggles
// decode in hardware.
import type { Adb } from "@yume-chan/adb";
import { run } from "./adb";

const PORT = 42420;
const BRIDGES = ["/data/local/tmp/eyepiece/eyepiece_bridge", "/vendor/natartos/apps/eyepiece/eyepiece_bridge"];
// message types (natartos mods/eyepiece/bridge/proto.h)
const EP_VIDEO = 0x01, EP_PING = 0x02, EP_STREAM = 0x04;
const EP_PONG = 0x82, EP_LOG = 0x83, EP_FRAME = 0x84, EP_INFO = 0x85;
const EP_FLAG_KEY = 0x01;

export interface EyepieceOptions {
  adb: Adb;
  fps: number;              // 60 / 100 (panel runs at 100 Hz)
  bitrate: number;          // bit/s
  gopSeconds?: number;      // keyframe interval (default 2 s)
  metrics?: boolean;        // eyepiece_bridge -M: display push→latch timing in the log
  width?: number; height?: number;   // output (goggles panel: 1920x1080)
  onStatus?: (s: EyepieceStats) => void;
  onLog?: (line: string) => void;
  onStop?: (reason: string) => void;
}

export interface EyepieceStats {
  fps: number; mbps: number;
  sendToShownP50: number; sendToShownP95: number;   // ms, PC send → goggles display push
  captureToSendP50: number | null;                  // ms, capture → encoded+sent (if timestamps usable)
  encodeP50: number;                                // ms, frame into encoder → chunk out
  decodeP50: number;                                // ms, on the goggles
  dropped: number; encDropped: number;
  encoder: string;
}

export interface EyepieceHandle { readonly active: boolean; stop(reason?: string): Promise<void>; }

const pct = (v: number[], q: number) => {
  if (!v.length) return NaN;
  const s = [...v].sort((a, b) => a - b);
  return s[Math.min(s.length - 1, Math.floor(s.length * q))];
};
const nowUs = () => Math.round(performance.now() * 1000);

function msg(type: number, payload: Uint8Array, flags = 0): Uint8Array {
  const b = new Uint8Array(8 + payload.length);
  const dv = new DataView(b.buffer);
  b[0] = 0x45; b[1] = 0x50; b[2] = type; b[3] = flags;   // magic 'E','P'
  dv.setUint32(4, payload.length, true);
  b.set(payload, 8);
  return b;
}

export async function startEyepiece(o: EyepieceOptions): Promise<EyepieceHandle> {
  const W = o.width ?? 1920, H = o.height ?? 1080, fps = o.fps;
  const gop = Math.max(1, Math.round((o.gopSeconds ?? 2) * fps));
  const say = (s: string) => o.onLog?.(s);

  if (!("VideoEncoder" in window)) throw new Error("WebCodecs VideoEncoder not available (use Chrome/Chromium)");

  // ---- encoder config: H.264 High, level 5.1 (1080p100), realtime, hardware ----
  const base: VideoEncoderConfig = {
    codec: "avc1.640033", width: W, height: H, bitrate: o.bitrate, framerate: fps,
    latencyMode: "realtime", bitrateMode: "constant", avc: { format: "annexb" },
  };
  let cfg: VideoEncoderConfig | null = null;
  for (const hw of ["prefer-hardware", "no-preference"] as HardwareAcceleration[]) {
    const c = { ...base, hardwareAcceleration: hw };
    if ((await VideoEncoder.isConfigSupported(c)).supported) { cfg = c; break; }
  }
  if (!cfg) throw new Error("H.264 1080p realtime encoding not supported by this browser");
  const encName = `H.264 ${cfg.hardwareAcceleration === "prefer-hardware" ? "hardware" : "any"}`;

  // ---- capture (user picks screen/window) ----
  const media = await navigator.mediaDevices.getDisplayMedia({
    video: { frameRate: { ideal: fps, max: fps }, width: { ideal: W }, height: { ideal: H } } as MediaTrackConstraints,
    audio: false,
  });
  const track = media.getVideoTracks()[0];
  try { (track as MediaStreamTrack & { contentHint: string }).contentHint = "motion"; } catch { /* ignore */ }

  // ---- bridge on the goggles ----
  let bridge = "";
  for (const p of BRIDGES) {
    if ((await run(o.adb, `[ -x ${p} ] && echo y || echo n`)).includes("y")) { bridge = p; break; }
  }
  if (!bridge) { media.getTracks().forEach(t => t.stop()); throw new Error("eyepiece_bridge not found on the goggles (install natartos with the eyepiece mod)"); }
  await run(o.adb, `pid=$(pidof eyepiece_bridge); [ -n "$pid" ] && kill $pid; sleep 0.3; ` +
    `nohup ${bridge} -1 ${o.metrics ? "-M" : ""} >/dev/null 2>&1 </dev/null &`);
  let sock = null as Awaited<ReturnType<Adb["createSocket"]>> | null;
  for (let i = 0; i < 25 && !sock; i++) {
    await new Promise(r => setTimeout(r, 200));
    try { sock = await o.adb.createSocket(`tcp:${PORT}`); } catch { /* not listening yet */ }
  }
  if (!sock) { media.getTracks().forEach(t => t.stop()); throw new Error(`could not connect to eyepiece_bridge on tcp:${PORT}`); }
  say(`eyepiece: connected to ${bridge}, encoder ${encName}, ${W}x${H}@${fps}, ${(o.bitrate / 1e6).toFixed(0)} Mbit/s`);
  const writer = sock.writable.getWriter();

  let running = true;
  let offsetUs: number | null = null, bestRtt = Infinity;
  const st = { sent: 0, bytes: 0, lat: [] as number[], cap: [] as number[], enc: [] as number[], dec: [] as number[],
               dropped: 0, encDropped: 0 };
  const capTs = new Map<number, number>();     // frame timestamp → performance.now() µs at encode()

  const send = (b: Uint8Array) => writer.write(b).catch(e => { if (running) stop("socket: " + (e as Error).message); });

  // ---- receive: PONG (clock sync), FRAME (timings), LOG, INFO ----
  (async () => {
    const reader = sock!.readable.getReader();
    let buf = new Uint8Array(0);
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        const nb = new Uint8Array(buf.length + value.length); nb.set(buf); nb.set(value, buf.length); buf = nb;
        while (buf.length >= 8) {
          const dv = new DataView(buf.buffer, buf.byteOffset);
          const len = dv.getUint32(4, true);
          if (buf.length < 8 + len) break;
          const type = buf[2], p = buf.subarray(8, 8 + len);
          const pv = new DataView(p.buffer, p.byteOffset, p.byteLength);
          if (type === EP_PONG && len >= 16) {
            const tPc = Number(pv.getBigUint64(0, true)), tDev = Number(pv.getBigUint64(8, true));
            const rtt = nowUs() - tPc;
            if (rtt < bestRtt) { bestRtt = rtt; offsetUs = tDev - (tPc + rtt / 2); }
          } else if (type === EP_FRAME && len >= 40) {
            const pts = Number(pv.getBigUint64(0, true)), tShown = Number(pv.getBigUint64(16, true));
            st.dec.push(pv.getUint32(24, true) / 1000);
            st.dropped = pv.getUint32(36, true);
            if (offsetUs !== null) st.lat.push((tShown - offsetUs - pts) / 1000);
          } else if (type === EP_LOG) {
            say("goggles: " + new TextDecoder().decode(p));
          } else if (type === EP_INFO && len >= 12) {
            say(`eyepiece: bridge v${pv.getUint32(0, true)}, decoder=${["none", "software", "hardware"][p[8]] ?? p[8]}`);
          }
          buf = buf.slice(8 + len);
        }
      }
    } catch { /* closed */ }
    if (running) stop("bridge closed the connection");
  })();

  const ping = setInterval(() => {
    const b = new Uint8Array(8); new DataView(b.buffer).setBigUint64(0, BigInt(nowUs()), true);
    void send(msg(EP_PING, b));
  }, 1000);
  { const b = new Uint8Array(8); new DataView(b.buffer).setBigUint64(0, BigInt(nowUs()), true); void send(msg(EP_PING, b)); }

  // stream header: the hardware decoder allocates buffers from it
  { const b = new Uint8Array(8), dv = new DataView(b.buffer);
    dv.setUint16(0, W, true); dv.setUint16(2, H, true); b[4] = 0; b[5] = Math.min(255, fps);
    void send(msg(EP_STREAM, b)); }

  // ---- encoder ----
  const enc = new VideoEncoder({
    output: (chunk) => {
      const t = nowUs();
      const tIn = capTs.get(chunk.timestamp);
      capTs.delete(chunk.timestamp);
      if (tIn !== undefined) st.enc.push((t - tIn) / 1000);
      // capture → send, if the frame timestamp is on the performance.now() clock (Chromium: yes)
      const age = t - chunk.timestamp;
      if (age > 0 && age < 500_000) st.cap.push(age / 1000);
      const p = new Uint8Array(12 + chunk.byteLength);
      const dv = new DataView(p.buffer);
      dv.setBigUint64(0, BigInt(t), true); p[8] = 0;
      chunk.copyTo(p.subarray(12));
      void send(msg(EP_VIDEO, p, chunk.type === "key" ? EP_FLAG_KEY : 0));
      st.sent++; st.bytes += chunk.byteLength;
    },
    error: (e) => { if (running) stop("encoder: " + e.message); },
  });
  enc.configure(cfg);

  // ---- frames: 16:9 + black bars on a GPU canvas when the source isn't W×H ----
  const canvas = new OffscreenCanvas(W, H);
  const ctx = canvas.getContext("2d", { alpha: false, desynchronized: true })!;
  let n = 0;
  const encodeFrame = (f: VideoFrame) => {
    if (!running) { f.close(); return; }
    if (enc.encodeQueueSize > 1) { st.encDropped++; f.close(); return; }   // never queue: drop
    let frame = f;
    if (f.displayWidth !== W || f.displayHeight !== H) {
      const s = Math.min(W / f.displayWidth, H / f.displayHeight);
      const w = Math.round(f.displayWidth * s), h = Math.round(f.displayHeight * s);
      ctx.fillStyle = "#000"; ctx.fillRect(0, 0, W, H);
      ctx.drawImage(f, (W - w) >> 1, (H - h) >> 1, w, h);
      frame = new VideoFrame(canvas, { timestamp: f.timestamp });
      f.close();
    }
    capTs.set(frame.timestamp, nowUs());
    enc.encode(frame, { keyFrame: n++ % gop === 0 });
    frame.close();
  };

  let readerAbort: (() => void) | null = null;
  const MSTP = (window as unknown as { MediaStreamTrackProcessor?: new (o: { track: MediaStreamTrack }) => { readable: ReadableStream<VideoFrame> } }).MediaStreamTrackProcessor;
  if (MSTP) {
    const r = new MSTP({ track }).readable.getReader();
    readerAbort = () => { void r.cancel(); };
    (async () => {
      try { for (;;) { const { value, done } = await r.read(); if (done) break; encodeFrame(value); } } catch { /* stopped */ }
    })();
  } else {
    const v = document.createElement("video");
    v.srcObject = media; v.muted = true; await v.play();
    const cb = () => { if (!running) return; encodeFrame(new VideoFrame(v, { timestamp: nowUs() })); v.requestVideoFrameCallback(cb); };
    v.requestVideoFrameCallback(cb);
  }
  track.addEventListener("ended", () => stop("screen share ended"));

  // ---- stats ----
  let tLast = performance.now(), sentLast = 0, bytesLast = 0;
  const statT = setInterval(() => {
    const t = performance.now(), dt = (t - tLast) / 1000;
    o.onStatus?.({
      fps: (st.sent - sentLast) / dt, mbps: (st.bytes - bytesLast) * 8 / dt / 1e6,
      sendToShownP50: pct(st.lat, 0.5), sendToShownP95: pct(st.lat, 0.95),
      captureToSendP50: st.cap.length ? pct(st.cap, 0.5) : null,
      encodeP50: pct(st.enc, 0.5), decodeP50: pct(st.dec, 0.5),
      dropped: st.dropped, encDropped: st.encDropped, encoder: encName,
    });
    tLast = t; sentLast = st.sent; bytesLast = st.bytes;
    st.lat = []; st.cap = []; st.enc = []; st.dec = [];
  }, 3000);

  async function stop(reason = "stopped") {
    if (!running) return;
    running = false;
    clearInterval(ping); clearInterval(statT);
    readerAbort?.();
    media.getTracks().forEach(t => t.stop());
    try { enc.close(); } catch { /* ignore */ }
    try { await writer.close(); } catch { /* ignore */ }
    try { await sock!.close(); } catch { /* ignore */ }   // bridge runs with -1: exits → UI restored
    o.onStop?.(reason);
  }

  return { get active() { return running; }, stop };
}
