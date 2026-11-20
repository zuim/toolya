// WebUSB install of the natartos mod framework — mirrors the CLI (toolya natartos
// install). Manifest-driven, sha256-verified before any
// device write. Release assets are fetched through the same-origin Worker proxy
// (/api/dl) because GitHub asset downloads don't send CORS headers.
//
// Gated: there are no natartos releases yet, so fetchLatestRelease() returns null and
// the Install button stays inert until a release with the anti-brick loader exists.

import type { Adb } from "@yume-chan/adb";
import { run, pushFile, getSelinux, setSelinux } from "./adb";
import { getContract } from "./wasm";

interface Artifact {
  file: string;
  device_path: string;
  mode?: string;
  sha256: string;
}
// a selectable mod: an artifact plus UI/selection metadata
export interface Mod extends Artifact {
  id: string;
  name?: string;
  description?: string;
  provides?: string;
  default?: boolean;
}
export interface Manifest {
  schema: number;
  natartos_version?: string;
  device?: string;
  init_rc?: { device_path: string; content: string };
  loader: Artifact;
  files?: Artifact[];
  mods?: Mod[];
  backup?: string[];
  remove?: string[];
}
export interface Release {
  tag_name: string;
  prerelease: boolean;
  assets: { name: string; browser_download_url: string }[];
}
// a user-supplied mod (not from a signed release → never telemetried)
export interface CustomMod {
  name: string;
  bytes: Uint8Array;
}

export async function fetchLatestRelease(): Promise<Release | null> {
  try {
    const { natartos: nc } = await getContract();
    const r = await fetch(`https://api.github.com/repos/${nc.repo}/releases/latest`, {
      headers: { Accept: "application/vnd.github+json" },
    });
    if (!r.ok) return null;
    return (await r.json()) as Release;
  } catch {
    return null;
  }
}

// download a release asset via the Worker proxy (GitHub asset CORS)
async function dl(assetUrl: string): Promise<Uint8Array> {
  const r = await fetch(`/api/dl?u=${encodeURIComponent(assetUrl)}`);
  if (!r.ok) throw new Error(`download failed (${r.status})`);
  return new Uint8Array(await r.arrayBuffer());
}

async function sha256hex(bytes: Uint8Array): Promise<string> {
  const h = await crypto.subtle.digest("SHA-256", bytes as BufferSource);
  return [...new Uint8Array(h)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

const parentDir = (p: string) => p.replace(/\/[^/]+$/, "") || "/";
const te = (s: string) => new TextEncoder().encode(s);
// single-quote a value for safe interpolation into a root shell command
const shq = (s: string) => `'${String(s).replace(/'/g, "'\\''")}'`;
// install only ever writes under /vendor; reject anything else (manifest is untrusted
// until releases are signed — see SECURITY.md).
function vendorPath(p: unknown): string {
  if (typeof p !== "string" || !p.startsWith("/vendor/") || p.includes("..") || /[\n\r\0]/.test(p))
    throw new Error(`refusing unsafe device path in manifest: ${String(p)}`);
  return p;
}
function safeMode(m?: string): string {
  const v = m ?? "0644";
  if (!/^[0-7]{3,4}$/.test(v)) throw new Error(`refusing unsafe mode in manifest: ${v}`);
  return v;
}
function customModPath(name: string): string {
  const base = name.split(/[/\\]/).pop() || "";
  if (!/^[A-Za-z0-9._-]+\.so$/.test(base)) throw new Error(`invalid custom mod name: ${name}`);
  return vendorPath(`/vendor/natartos/mods/${base}`);
}

/** Download + parse a release's manifest.json (so the UI can list selectable mods). */
export async function loadManifest(rel: Release): Promise<Manifest> {
  const a = rel.assets.find((x) => x.name === "manifest.json");
  if (!a) throw new Error("release is missing manifest.json");
  const m = JSON.parse(new TextDecoder().decode(await dl(a.browser_download_url))) as Manifest;
  if (m.schema !== 1) throw new Error(`manifest schema ${m.schema} unsupported — update toolya`);
  return m;
}

/** Install the loader + the selected release mods (by id) + any custom .so, over adb.
 *  Everything is sha256-verified (release mods) and path-validated before any write.
 *  Returns the installed **release** mod ids (for anonymous per-mod telemetry). */
export async function installFromRelease(
  adb: Adb,
  rel: Release,
  manifest: Manifest,
  opts: { modIds: string[]; custom?: CustomMod[] },
  log: (s: string) => void,
): Promise<string[]> {
  const { natartos: nc } = await getContract();
  const asset = (name: string) => rel.assets.find((a) => a.name === name);
  const chosen = (manifest.mods ?? []).filter((m) => opts.modIds.includes(m.id));

  // 1. download + sha256-verify the loader + chosen release mods before touching the device
  const dlVerify = async (art: Artifact): Promise<Uint8Array> => {
    const a = asset(art.file);
    if (!a) throw new Error(`release is missing ${art.file}`);
    log(`downloading ${art.file}…`);
    const bytes = await dl(a.browser_download_url);
    const got = await sha256hex(bytes);
    if (!art.sha256 || got.toLowerCase() !== art.sha256.toLowerCase())
      throw new Error(`sha256 mismatch for ${art.file} — refusing to install`);
    return bytes;
  };
  const loaderBytes = await dlVerify(manifest.loader);
  const fileEntries: { art: Artifact; bytes: Uint8Array }[] = [];
  for (const f of manifest.files ?? []) fileEntries.push({ art: f, bytes: await dlVerify(f) });
  const modFiles: { art: Mod; bytes: Uint8Array }[] = [];
  for (const m of chosen) modFiles.push({ art: m, bytes: await dlVerify(m) });

  // 2. device check
  if (manifest.device) {
    const model = (await run(adb, "getprop ro.product.device; getprop ro.product.model")).toLowerCase();
    const hit = manifest.device.toLowerCase().split(/\s+/).some((t) => model.includes(t));
    if (!hit) throw new Error(`device mismatch: this release targets ${manifest.device}`);
  }

  // 3. validate every path/mode (manifest + custom are untrusted; writes stay under /vendor)
  const rcPath = manifest.init_rc ? vendorPath(manifest.init_rc.device_path) : null;
  const loaderPlan = {
    file: manifest.loader.file,
    path: vendorPath(manifest.loader.device_path),
    mode: safeMode(manifest.loader.mode),
    bytes: loaderBytes,
  };
  const filesPlan = fileEntries.map(({ art, bytes }) => ({
    file: art.file,
    path: vendorPath(art.device_path),
    mode: safeMode(art.mode),
    bytes,
  }));
  const modPlan = modFiles.map(({ art, bytes }) => ({
    file: art.file,
    path: vendorPath(art.device_path),
    mode: safeMode(art.mode),
    bytes,
  }));
  const customPlan = (opts.custom ?? []).map((c) => ({
    file: c.name,
    path: customModPath(c.name), // sanitized → /vendor/natartos/mods/<name>.so
    mode: "0644",
    bytes: c.bytes,
  }));
  const backups = (manifest.backup ?? []).map(vendorPath);
  const removes = (manifest.remove ?? []).map(vendorPath);

  // 4. apply (all interpolated values validated + single-quoted)
  // SELinux must be permissive before touching /vendor — on a fresh (enforcing)
  // device the remount and the writes under /vendor are denied otherwise.
  const sel = await getSelinux(adb);
  if (!sel.toLowerCase().startsWith("perm")) {
    log("SELinux is enforcing — switching to permissive…");
    await setSelinux(adb, "permissive");
  }
  log("remounting /vendor rw…");
  await vendorRw(adb);
  try {
  await run(adb, `mkdir -p ${shq(nc.stock_dir)}`);
  for (const p of backups) {
    const base = p.split("/").pop();
    const orig = `${nc.stock_dir}/${base}.orig`;
    await run(adb, `[ -f ${shq(orig)} ] || cp ${shq(p)} ${shq(orig)}`);
  }
  for (const { file, path, mode, bytes } of [loaderPlan, ...filesPlan, ...modPlan, ...customPlan]) {
    log(`pushing ${file}…`);
    await run(adb, `mkdir -p ${shq(parentDir(path))}`);
    await pushFile(adb, path, bytes);
    await run(adb, `chmod ${mode} ${shq(path)}`);
  }
  for (const p of removes) await run(adb, `rm -f ${shq(p)}`);

  if (manifest.init_rc && rcPath) {
    log("writing vendor init…");
    await pushFile(adb, rcPath, te(manifest.init_rc.content));
    await run(adb, `chmod 644 ${shq(rcPath)}`);
  }
  // record installed state (version + chosen mod ids + custom names) for status/uninstall
  const installed = {
    natartos_version: manifest.natartos_version,
    mods: chosen.map((m) => m.id),
    custom: customPlan.map((c) => c.path.split("/").pop()),
  };
  await pushFile(adb, nc.manifest_device, te(JSON.stringify(manifest)));
  await pushFile(adb, "/vendor/natartos/installed.json", te(JSON.stringify(installed)));

  // auto-enable default mods (opt-in flags for inject.sh)
  await run(adb, "mkdir -p /vendor/natartos/enabled");
  for (const m of chosen) {
    if (m.default) {
      await run(adb, `touch ${shq(`/vendor/natartos/enabled/${m.file}`)}`);
      log(`  enabled (default): ${m.file}`);
    }
  }
  for (const c of customPlan) {
    const base = c.path.split("/").pop()!;
    await run(adb, `touch ${shq(`/vendor/natartos/enabled/${base}`)}`);
    log(`  enabled (custom): ${base}`);
  }

  await run(adb, `rm -f ${nc.bootcount} ${nc.status} ${nc.disabled}`);
  } finally { await vendorRo(adb); }

  log(`natartos ${manifest.natartos_version ?? ""} installed (${chosen.length + customPlan.length} mod(s)) — activating…`);
  return chosen.map((m) => m.id);
}

/** /vendor is rw only while we write: a kernel crash or power loss while it is
 *  mounted rw is the one way these writes could damage boot files. Callers wrap
 *  their writes in try { await vendorRw(adb); … } finally { await vendorRo(adb); }. */
async function vendorRw(adb: Adb): Promise<void> {
  await run(adb, "mount -o remount,rw /vendor");
}
async function vendorRo(adb: Adb): Promise<void> {
  try { await run(adb, "sync; mount -o remount,ro /vendor"); } catch { /* best effort */ }
}

/** Start mods on the device — runs inject.sh without reinstalling. */
export async function startMods(
  adb: Adb,
  log: (s: string) => void,
): Promise<void> {
  const { natartos: nc } = await getContract();
  const injectPath = "/vendor/natartos/inject.sh";
  const exists = (await run(adb, `[ -f ${shq(injectPath)} ] && echo y || echo n`)).trim();
  if (exists !== "y") throw new Error("natartos is not installed (no inject.sh on device)");

  const sel = await getSelinux(adb);
  if (!sel.toLowerCase().startsWith("perm")) {
    log("SELinux is enforcing — switching to permissive…");
    await setSelinux(adb, "permissive");
  }

  log("starting mods…");
  // Clear the loader log and any stale overlay flags / supervisor pid left by a
  // previous session — a leftover fb_stop sends the overlay into a
  // cleanup→restart loop, so dji_glasses never settles with the mods up.
  await run(adb, `rm -f ${nc.log}`);
  await run(
    adb,
    "rm -f /data/local/tmp/fb_stop /data/local/tmp/fb_override /data/local/tmp/natartos_restart /data/local/tmp/natartos_inject.pid",
  );
  await run(adb, "sync && echo 3 > /proc/sys/vm/drop_caches");

  // inject.sh stops dji_glasses, which re-enumerates USB and kills this ADB
  // shell. nohup is not enough — the child stays in the shell's process group
  // and dies with it, leaving init to respawn a STOCK dji_glasses (no mods).
  // setsid detaches it into its own session so it survives the drop (parity
  // with the CLI `cmd_start`).
  // Keep the adb shell alive briefly after backgrounding: if it exits the instant
  // it forks, adbd tears the exec down and kills the child before setsid detaches
  // it into its own session — inject.sh never runs. The trailing sleep lets the
  // detach finish first.
  await run(adb, `setsid sh ${shq(injectPath)} </dev/null >/dev/null 2>&1 & sleep 2`);

  log("waiting for startup (USB may reconnect)…");
  await new Promise((r) => setTimeout(r, 12000));

  const logText = (await run(adb, `cat ${nc.log} 2>/dev/null`)).trim();
  if (logText) log(logText);

  // Verify dji_glasses re-spawned under the loader. The overlay repaints by
  // restarting dji_glasses, so a single live read can land mid-respawn (USB
  // re-enumerating) and report 0 even though the mods loaded. Poll a few times,
  // and accept the loader log itself as proof ("mods loaded: N") when the live
  // read keeps racing.
  const logLoaded = /mods loaded:\s*[1-9]/.test(logText);
  let loaded = "";
  for (let i = 0; i < 6; i++) {
    try {
      loaded = (
        await run(adb, `cat /proc/$(pidof dji_glasses)/maps 2>/dev/null | grep -c natartos_loader`)
      ).trim();
    } catch {
      loaded = ""; // USB likely re-enumerated — can't read this handle
    }
    if (loaded && loaded !== "0") break;
    await new Promise((r) => setTimeout(r, 1500));
  }
  if (!logLoaded && loaded === "0") {
    throw new Error("inject.sh ran but no mods are loaded — check status / log");
  }
  log(loaded && loaded !== "0" ? "mods started — loader mapped" : "mods started — confirmed from loader log");
}

/** Uninstall natartos: restore stock originals + remove all natartos files.
 *  Mirrors the CLI `toolya natartos uninstall`. A reboot returns to fully stock. */
export async function uninstallNatartos(adb: Adb, log: (s: string) => void): Promise<void> {
  const { natartos: nc } = await getContract();
  const sel = await getSelinux(adb);
  if (!sel.toLowerCase().startsWith("perm")) {
    log("SELinux is enforcing — switching to permissive…");
    await setSelinux(adb, "permissive");
  }
  await vendorRw(adb);
  try {
  // Read the applied manifest to restore the exact backed-up files.
  let applied: Manifest | null = null;
  try { applied = JSON.parse((await run(adb, `cat ${nc.manifest_device} 2>/dev/null`)).trim()) as Manifest; }
  catch { /* fall back to convention */ }

  log("restoring stock originals…");
  const backups = (applied?.backup ?? ["/vendor/etc/init/dji_glasses_custom.rc", "/vendor/etc/selinux/precompiled_sepolicy"]).slice();
  if (applied?.init_rc) backups.push(applied.init_rc.device_path);
  for (const path of backups) {
    const base = path.split("/").pop()!;
    const orig = `${nc.stock_dir}/${base}.orig`;
    const has = (await run(adb, `[ -f ${shq(orig)} ] && echo yes || echo no`)).trim();
    if (has === "yes") {
      await run(adb, `cp ${shq(orig)} ${shq(path)}`);
      log(`  restored ${base}`);
    }
  }

  log("removing natartos…");
  // Stop the inject supervisor so it can't respawn dji_glasses with the loader.
  await run(adb, "[ -f /data/local/tmp/natartos_inject.pid ] && kill $(cat /data/local/tmp/natartos_inject.pid) 2>/dev/null; true");
  await run(adb, "rm -rf /vendor/natartos");
  await run(adb, "rm -f /vendor/etc/init/persist_adb.rc");
  await run(adb, `rm -f ${nc.bootcount} ${nc.status} ${nc.disabled}`);
  await run(adb, "rm -f /data/.natartos_* /data/local/tmp/fb_* /data/local/tmp/natartos* /data/local/tmp/vidplane /data/local/tmp/screencap.bin 2>/dev/null; sync");
  } finally { await vendorRo(adb); }
  log("natartos removed — reboot the goggles to return to fully stock.");
}

/** Set a custom disconnect background. The stock UI loads
 *  /system/etc/extern_bitmap/background_disconnect.jpg (read-only /system); the
 *  hook_bitmap mod redirects that open() to /vendor/custom/extern_bitmap/ when
 *  the flag is present. So: push the JPEG there, set the flag, enable the mod,
 *  and restart so the hook takes effect. */
export async function applyBackground(adb: Adb, jpeg: Uint8Array, log: (s: string) => void): Promise<void> {
  const has = (await run(adb, "[ -f /vendor/natartos/mods/hook_bitmap.so ] && echo y || echo n")).trim();
  if (has !== "y") throw new Error("the Custom Background mod (hook_bitmap) isn't installed — reinstall natartos including it");
  const sel = await getSelinux(adb);
  if (!sel.toLowerCase().startsWith("perm")) { log("SELinux is enforcing — switching to permissive…"); await setSelinux(adb, "permissive"); }
  await vendorRw(adb);
  try {
    await run(adb, "mkdir -p /vendor/custom/extern_bitmap");
    log("writing background…");
    await pushFile(adb, "/vendor/custom/extern_bitmap/background_disconnect.jpg", jpeg);
    await run(adb, "touch /data/local/tmp/natartos_background");
    await run(adb, "touch /vendor/natartos/enabled/hook_bitmap.so");
  } finally { await vendorRo(adb); }
  log("background set — restarting mods so it takes effect…");
  await startMods(adb, log);
}

/** Enable or disable one installed mod by touching/removing its enabled/ flag.
 *  Takes effect when mods are next (re)started. */
export async function setModEnabled(adb: Adb, file: string, on: boolean): Promise<void> {
  if (!/^[A-Za-z0-9._-]+\.so$/.test(file)) throw new Error(`invalid mod file: ${file}`);
  const flag = shq(`/vendor/natartos/enabled/${file}`);
  await vendorRw(adb);
  try {
    if (on) await run(adb, `touch ${flag}`);
    else await run(adb, `rm -f ${flag}`);
  } finally { await vendorRo(adb); }
}

const FREEDOOM_ZIP = "https://github.com/freedoom/freedoom/releases/download/v0.13.0/freedoom-0.13.0.zip";
const DOOM_WAD_PATH = "/vendor/natartos/apps/doom/freedoom1.wad";

/** Download Freedoom Phase 1 WAD from GitHub, extract, push to device. */
export async function downloadDoomWad(
  adb: Adb,
  log: (s: string) => void,
): Promise<void> {
  log("downloading Freedoom (~24 MB zip)…");
  const zip = await dl(FREEDOOM_ZIP);
  log("extracting freedoom1.wad…");
  const wad = await extractFromZip(zip, /freedoom1\.wad$/i);
  if (!wad) throw new Error("freedoom1.wad not found in archive");
  log(`pushing freedoom1.wad (${(wad.byteLength / 1048576).toFixed(1)} MB)…`);
  await vendorRw(adb);
  try {
    await run(adb, `mkdir -p ${shq(parentDir(DOOM_WAD_PATH))}`);
    await pushFile(adb, DOOM_WAD_PATH, wad);
  } finally { await vendorRo(adb); }
  log("Doom WAD installed");
}

/** Ensure a Doom WAD is on the device — download Freedoom if there's none yet.
 *  Returns true if it downloaded one, false if a WAD was already present. */
export async function ensureDoomWad(adb: Adb, log: (s: string) => void): Promise<boolean> {
  const has = (await run(adb, `[ -f ${shq(DOOM_WAD_PATH)} ] && echo y || echo n`)).trim();
  if (has === "y") return false;
  await downloadDoomWad(adb, log);
  return true;
}

/** Push a user-supplied WAD file to the device. */
export async function pushDoomWad(
  adb: Adb,
  bytes: Uint8Array,
  log: (s: string) => void,
): Promise<void> {
  log(`pushing WAD (${(bytes.byteLength / 1048576).toFixed(1)} MB)…`);
  await vendorRw(adb);
  try {
    await run(adb, `mkdir -p ${shq(parentDir(DOOM_WAD_PATH))}`);
    await pushFile(adb, DOOM_WAD_PATH, bytes);
  } finally { await vendorRo(adb); }
  log("WAD installed");
}

async function extractFromZip(data: Uint8Array, nameRe: RegExp): Promise<Uint8Array | null> {
  const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
  let eocd = -1;
  for (let i = data.length - 22; i >= Math.max(0, data.length - 65558); i--) {
    if (view.getUint32(i, true) === 0x06054b50) { eocd = i; break; }
  }
  if (eocd < 0) throw new Error("not a valid zip");
  const cdOff = view.getUint32(eocd + 16, true);
  const cdCount = view.getUint16(eocd + 10, true);
  let off = cdOff;
  for (let i = 0; i < cdCount; i++) {
    if (view.getUint32(off, true) !== 0x02014b50) break;
    const method = view.getUint16(off + 10, true);
    const compSize = view.getUint32(off + 20, true);
    const nameLen = view.getUint16(off + 28, true);
    const extraLen = view.getUint16(off + 30, true);
    const commentLen = view.getUint16(off + 32, true);
    const localOff = view.getUint32(off + 42, true);
    const name = new TextDecoder().decode(data.subarray(off + 46, off + 46 + nameLen));
    if (nameRe.test(name)) {
      const lNameLen = view.getUint16(localOff + 26, true);
      const lExtraLen = view.getUint16(localOff + 28, true);
      const dataStart = localOff + 30 + lNameLen + lExtraLen;
      const raw = data.subarray(dataStart, dataStart + compSize);
      if (method === 0) return new Uint8Array(raw);
      if (method === 8) {
        const ds = new DecompressionStream("deflate-raw");
        const w = ds.writable.getWriter();
        void w.write(raw).then(() => w.close());
        const r = ds.readable.getReader();
        const chunks: Uint8Array[] = [];
        for (;;) { const { done, value } = await r.read(); if (done) break; chunks.push(value); }
        const total = chunks.reduce((s, c) => s + c.byteLength, 0);
        const out = new Uint8Array(total);
        let p = 0;
        for (const c of chunks) { out.set(c, p); p += c.byteLength; }
        return out;
      }
      throw new Error(`unsupported zip method ${method}`);
    }
    off += 46 + nameLen + extraLen + commentLen;
  }
  return null;
}

/** Push + run dm_noverity on the device — disables dm-verity for this session. */
export async function noverity(
  adb: Adb,
  binary: Uint8Array,
  log: (s: string) => void,
): Promise<boolean> {
  const dest = "/data/local/tmp/dm_noverity";

  log("pushing dm_noverity…");
  await pushFile(adb, dest, binary);
  await run(adb, `chmod 755 ${dest}`);

  log("running dm_noverity…");
  const output = await run(adb, dest);
  for (const line of output.split("\n")) if (line.trim()) log(`  ${line}`);

  const ok = output.includes("type='linear'");
  if (ok) {
    log("dm-verity disabled for this session");
  } else {
    log("dm_noverity may have failed — check output above");
  }
  return ok;
}
