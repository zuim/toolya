import "@xterm/xterm/css/xterm.css";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { enableAdb, isKnownDevice } from "./goggles";
import { stat, getSupportId, askDntRoot } from "./stat";
import { fetchLatestRelease, loadManifest, installFromRelease, startMods, uninstallNatartos, setModEnabled, applyBackground, ensureDoomWad, noverity, downloadDoomWad, pushDoomWad, type Release, type Manifest, type CustomMod } from "./natartos";
import { connectAdb, readUsage, run, runInDir, setUsbMode, usbCfgString, setSelinux, getSelinux, natartosStatus, natartosSafe, natartosDump, pushFile, pullFile } from "./adb";
import { toScreenJpeg, previewInto, bgraToCanvas } from "./image";
import { startStream, type StreamHandle } from "./stream";
import { startPcStream, type PcStreamHandle } from "./pc-stream";
import { startEyepiece, type EyepieceHandle } from "./eyepiece";
import { sendKey, holdKey } from "./input";
import type { Adb } from "@yume-chan/adb";

const $ = (id: string) => document.getElementById(id)!;
const logEl = $("log");
function log(s: string) {
  logEl.textContent += s + "\n";
  logEl.scrollTop = logEl.scrollHeight;
}

let adb: Adb | null = null;
let term: Terminal | null = null;
let heartbeat: ReturnType<typeof setInterval> | null = null;

// Demo mode (set by the Worker when DRY_RUN): simulate enable/connect, no real device,
// no stats — just to preview the UI. See worker/src/index.ts.
const DRY = !!(window as unknown as { __TOOLYA_DRY_RUN__?: unknown }).__TOOLYA_DRY_RUN__;

// First-run risk disclaimer — shown until acknowledged (stored per browser).
if (!localStorage.getItem("toolya.ack")) {
  const modal = $("disclaimer");
  modal.hidden = false;
  const ck = $("disc-ck") as HTMLInputElement;
  const ok = $("disc-ok") as HTMLButtonElement;
  ck.addEventListener("change", () => {
    ok.disabled = !ck.checked;
  });
  ok.addEventListener("click", () => {
    try {
      localStorage.setItem("toolya.ack", "1");
    } catch {
      /* ignore */
    }
    stat("disclaimer_ok");
    modal.hidden = true;
  });
}

// top of the funnel: a landing. (decline = page_view − disclaimer_ok, no separate event)
stat("page_view");

// support ID in footer — first 8 chars of cid, clickable to copy full cid
const sidEl = document.getElementById("sid");
if (sidEl) {
  const sid = getSupportId();
  sidEl.textContent = `ID: ${sid}`;
  sidEl.addEventListener("click", () => {
    navigator.clipboard.writeText(sid).then(() => {
      sidEl.textContent = "copied!";
      setTimeout(() => { sidEl.textContent = `ID: ${sid}`; }, 1500);
    });
  });
}

// live rooted-count badge (from the Worker's Analytics Engine query; optional)
let rootedN: number | null = null;
function showRooted(n: number) {
  rootedN = n;
  const el = $("rooted");
  el.innerHTML = `🔓 <b>${n.toLocaleString()}</b> rooted`;
  el.hidden = false;
}
fetch("/api/rooted")
  .then((r) => r.json())
  .then((d: { rooted?: number }) => {
    if (typeof d.rooted === "number" && d.rooted > 0) showRooted(d.rooted);
  })
  .catch(() => {
    /* badge is optional */
  });

// success flourish after root is confirmed
function rootFx() {
  const fx = $("root-fx");
  fx.classList.remove("go");
  void fx.offsetWidth; // reflow so the animation restarts
  fx.classList.add("go");
  const term = $("term");
  term.classList.remove("rooted-glow");
  void term.offsetWidth;
  term.classList.add("rooted-glow");
  if (rootedN !== null && !DRY) showRooted(rootedN + 1); // optimistic bump (real roots only)
}

// crypto donation address (EVM, any chain) — copy button in the Support card
const CRYPTO_ADDR = "0x5182b19b01714C64811AeC6C59A0880C6F24528d";
$("evm-copy").addEventListener("click", async () => {
  const btn = $("evm-copy");
  const prev = btn.textContent;
  try {
    await navigator.clipboard.writeText(CRYPTO_ADDR);
    btn.textContent = "copied ✓";
  } catch {
    btn.textContent = "copy failed";
  }
  setTimeout(() => {
    btn.textContent = prev;
  }, 1600);
});

function setStatus(text: string, cls = "") {
  const el = $("status");
  el.textContent = text;
  el.dataset.state = cls === "ok" ? "active" : cls === "err" ? "error" : "idle";
}
function btn(id: string, disabled: boolean) {
  ($(id) as HTMLButtonElement).disabled = disabled;
}
// Shift the primary (highlighted) button along the Enable → Connect flow.
function setPrimary(which: "enable" | "connect" | "none") {
  $("btn-enable").classList.toggle("primary", which === "enable");
  $("btn-connect").classList.toggle("primary", which === "connect");
}
// Post-root guidance banner (next steps + adb-from-desktop).
function showDntAsk() {
  if (!navigator.doNotTrack || (navigator.doNotTrack !== "1" && (navigator as unknown as { doNotTrack?: string }).doNotTrack !== "yes")) return;
  const bar = document.createElement("div");
  bar.style.cssText = "position:fixed;bottom:0;left:0;right:0;padding:12px 20px;background:#1a2233;border-top:1px solid #30363d;display:flex;align-items:center;gap:12px;font-size:13px;z-index:999;color:#cfd5de";
  bar.innerHTML = `<span>You rooted! Share an anonymous +1 to the counter? <b>No personal data</b> — just the event.</span>
    <button id="dnt-yes" style="padding:6px 14px;border-radius:6px;border:1px solid #3fb950;background:transparent;color:#3fb950;cursor:pointer;font-size:13px">Yes, count me</button>
    <button id="dnt-no" style="padding:6px 14px;border-radius:6px;border:1px solid #30363d;background:transparent;color:#8b949e;cursor:pointer;font-size:13px">No thanks</button>`;
  document.body.appendChild(bar);
  bar.querySelector("#dnt-yes")!.addEventListener("click", () => { askDntRoot(); bar.remove(); });
  bar.querySelector("#dnt-no")!.addEventListener("click", () => bar.remove());
}

function showSuccess() {
  const el = $("success");
  el.innerHTML =
    '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg>' +
    "<div><h3>Rooted — you're in 🎉</h3>" +
    "<p>What now? Type commands in the <b>terminal</b> above, or set up <b>natartos</b> for persistent mods. " +
    "Prefer your computer's terminal? The goggles now speak adb over USB — run <code>adb devices</code>, then <code>adb shell</code>.</p>" +
    '<div class="srow">' +
    '<button class="btn sm" id="su-term" type="button">↑ Go to terminal</button>' +
    '<button class="btn sm tonal" id="su-nat" type="button">Set up natartos →</button>' +
    "</div></div>" +
    '<button class="x" id="success-x" type="button" aria-label="dismiss">×</button>';
  el.hidden = false;
  $("success-x").addEventListener("click", () => {
    el.hidden = true;
  });
  $("su-term").addEventListener("click", () => {
    $("term").scrollIntoView({ behavior: "smooth", block: "center" });
    term?.focus();
  });
  $("su-nat").addEventListener("click", () => navTo("/natartos"));
}

// Map raw WebUSB/adb errors to plain-language guidance.
function friendlyError(e: unknown): string {
  const err = e as { name?: string; message?: string };
  const name = err?.name ?? "";
  const msg = err?.message ?? String(e);
  if (name === "NotFoundError" || /no device (selected|found|chosen)/i.test(msg))
    return "No goggles selected. Plug them in and pick the device in the browser prompt, or try reconnecting.";
  if (/claim|in ?use|access denied|busy/i.test(msg))
    return "Couldn't claim the USB interface. Close any running adb server; on Windows install the WinUSB driver (Zadig).";
  if (name === "SecurityError" || /secure context|not allowed in this context/i.test(msg))
    return "Blocked by the browser. Use Chrome/Edge over https or http://localhost.";
  if (/unauthor|auth|rejected|credential/i.test(msg))
    return "The goggles rejected the connection. Try reconnecting; the device may need re-pairing.";
  if (isDisconnect(e)) return "Connection lost — the goggles disconnected.";
  return msg;
}

function isDisconnect(e: unknown): boolean {
  const msg = (e as { message?: string })?.message ?? "";
  return /disconnect|lost|closed|no such device|unavailable|detached|ENOENT/i.test(msg);
}

// Coarse failure bucket for telemetry (where rooting broke) — no raw messages.
function classifyErr(e: unknown): string {
  const err = e as { name?: string; message?: string };
  const name = err?.name ?? "";
  const msg = err?.message ?? String(e);
  if (/webusb not available/i.test(msg)) return "no_webusb";
  if (name === "NotFoundError" || /no device (selected|found|chosen)|no goggles selected/i.test(msg)) return "no_device";
  if (/claim|in ?use|access denied|busy/i.test(msg)) return "claim";
  if (name === "SecurityError" || /secure context/i.test(msg)) return "secure";
  if (/unauthor|auth|rejected|credential/i.test(msg)) return "auth";
  if (/re-enumerate|cmd_id/i.test(msg)) return "no_reenum";
  if (isDisconnect(e)) return "disconnect";
  return "other";
}

// Report an adb-op error in plain language; if it's a drop, reset to disconnected.
function reportAdbError(ctx: string, e: unknown) {
  log(`${ctx}: ${friendlyError(e)}`);
  if (isDisconnect(e)) void disconnect();
}

// WebUSB needs a secure context (https or localhost). Over http://<LAN-IP> the
// browser disables navigator.usb — tell the user instead of failing cryptically.
const canUsb = DRY || (window.isSecureContext && !!navigator.usb);
if (!canUsb) {
  const n = $("notice");
  n.hidden = false;
  n.innerHTML =
    "WebUSB needs a secure context. Open this page over <code>https</code> or " +
    "<code>http://localhost</code> on the machine the goggles are plugged into. " +
    "Over <code>http://&lt;LAN-IP&gt;</code> the browser disables USB.";
  btn("btn-enable", true);
  btn("btn-connect", true);
}

// --- view router: /app (root tool) vs /natartos (on-device mods) — same page, one connection ---
function showView(name: "root" | "natartos") {
  $("view-root").hidden = name !== "root";
  $("view-natartos").hidden = name !== "natartos";
}
function applyRoute(smooth: boolean) {
  const nat = location.pathname.replace(/\/+$/, "") === "/natartos";
  showView(nat ? "natartos" : "root");
  // Re-read status on entering /natartos (not only at connect) so the mod list
  // is current even if connect happened earlier or its first read failed.
  if (nat && adb) void refreshNatartos();
  // natartos: scroll to its section (not the console at top); root: back to top
  if (nat) $("view-natartos").scrollIntoView({ behavior: smooth ? "smooth" : "auto", block: "start" });
  else if (smooth) scrollTo({ top: 0, behavior: "smooth" });
}
function navTo(path: string) {
  history.pushState({}, "", path);
  applyRoute(true);
}
$("to-natartos").addEventListener("click", (e) => { e.preventDefault(); navTo("/natartos"); });
$("to-root").addEventListener("click", (e) => { e.preventDefault(); navTo("/app"); });
addEventListener("popstate", () => applyRoute(false));
applyRoute(false);

$("btn-enable").addEventListener("click", async () => {
  stat("enable_start");
  if (DRY) {
    log("enable ADB — simulated (demo mode).");
    stat("enable_ok");
    setPrimary("connect");
    return;
  }
  try {
    const info = await enableAdb(log);
    stat("enable_ok");
    if (!isKnownDevice(info)) showUntestedNotice(info);
    if (info.alreadyEnabled) {
      // ADB was already on — no re-enumerate to wait for, just connect.
      await doConnect();
    } else {
      setPrimary("connect");
    }
  } catch (e) {
    stat("enable_fail", { stage: classifyErr(e) });
    log("enable: " + friendlyError(e));
  }
});

function showUntestedNotice(info: { productName: string; productId: number }) {
  const el = $("untested");
  el.hidden = false;
  $("untested-name").textContent = info.productName || `PID 0x${info.productId.toString(16)}`;
  $("untested-send").addEventListener("click", () => {
    stat("device_report", {
      stage: info.productName || "unknown",
      reason: `pid:0x${info.productId.toString(16)}`,
    });
    el.innerHTML = '<span class="muted">Report sent — thank you.</span>';
  }, { once: true });
}

async function doConnect() {
  if (DRY) {
    fakeConnect();
    return;
  }
  try {
    log("connecting adb…");
    setStatus("connecting…");
    adb = await connectAdb();
    log("connected: " + adb.banner.model + " (root shell)");
    setStatus("connected · root", "ok");
    $("devinfo").textContent = `${adb.banner.model} · ${adb.banner.product}`;
    $("usage-hdr").hidden = false;
    btn("btn-disconnect", false);
    btn("btn-connect", true);
    btn("usb-adb", false);
    btn("usb-rndis", false);
    btn("selinux-perm", false);
    btn("selinux-enf", false);
    btn("nat-refresh", false);
    ($("nat-safe") as HTMLInputElement).disabled = false;
    updateNatInstall();
    setDpad(true);
    btn("scr-grab", false);
    btn("ps-stream", false);
    btn("ep-stream", false);
    $("ep-status").textContent = "ready";
    if (bgJpeg) btn("bg-apply", false);
    void startTerminal(adb);
    startHeartbeat();
    let devHash: string | undefined;
    try {
      const serial = (await run(adb, "getprop ro.serialno")).trim();
      if (serial) {
        const buf = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(serial));
        devHash = [...new Uint8Array(buf)].map(b => b.toString(16).padStart(2, "0")).join("").slice(0, 8);
      }
    } catch { /* non-critical */ }
    stat("connect_ok", { deviceId: devHash });
    showDntAsk();
    rootFx();
    setPrimary("none");
    showSuccess();
    $("nat-sec-state").textContent = "ready";
    void refreshSelinux();
    void refreshNatartos();
    await refreshUsage();
  } catch (e) {
    stat("connect_fail", { reason: classifyErr(e) });
    log("connect: " + friendlyError(e));
    setStatus("connect failed", "err");
  }
}
$("btn-connect").addEventListener("click", () => void doConnect());

// Demo-mode connect: paint the connected UI with sample data, no real device.
function fakeConnect() {
  log("connecting adb… (demo mode)");
  adb = null; // stays null — real adb ops remain inert
  setStatus("connected · root", "ok");
  $("devinfo").textContent = "E3T zv902 · demo";
  $("usage-hdr").hidden = false;
  $("stat-power").textContent = "123";
  $("stat-motor").textContent = "957";
  btn("btn-disconnect", false);
  btn("btn-connect", true);
  btn("usb-adb", false);
  btn("usb-rndis", false);
  btn("selinux-perm", false);
  btn("selinux-enf", false);
  btn("nat-refresh", false);
  ($("nat-safe") as HTMLInputElement).disabled = false;
  updateNatInstall();
  setDpad(true);
  btn("scr-grab", false);
  if (bgJpeg) btn("bg-apply", false);
  $("selinux-state").textContent = "Enforcing";
  $("nat-status").textContent = "not installed";
  $("nat-boot").textContent = "0";
  startFakeTerminal();
  setPrimary("none");
  showSuccess();
  $("nat-sec-state").textContent = "ready (demo)";
  rootFx();
  log("connected: E3T zv902 (root shell) — demo mode, no real device");
}

function startFakeTerminal() {
  const t = new Terminal({
    fontSize: 13,
    cursorBlink: true,
    fontFamily: '"Geist Mono", ui-monospace, Menlo, monospace',
    theme: { background: "#0a0c10", foreground: "#e7e9ee", cursor: "#5cc8ff" },
  });
  term = t;
  const fit = new FitAddon();
  t.loadAddon(fit);
  t.open($("term"));
  fit.fit();
  addEventListener("resize", () => fit.fit());
  let line = "";
  const prompt = () => t.write("\r\n\x1b[36m/\x1b[0m $ ");
  t.writeln("toolya root shell (demo) — type anything; no real device is attached.");
  prompt();
  t.focus();
  t.onData((d) => {
    for (const ch of d) {
      if (ch === "\r") {
        t.write("\r\n");
        if (line.trim()) t.write("\x1b[90m(demo) no device — this is a UI preview\x1b[0m\r\n");
        line = "";
        prompt();
      } else if (ch === "\x7f") {
        if (line) { line = line.slice(0, -1); t.write("\b \b"); }
      } else if (ch >= " ") {
        line += ch;
        t.write(ch);
      }
    }
  });
}

$("btn-disconnect").addEventListener("click", () => void disconnect());
$("usb-adb").addEventListener("click", () => void switchUsb("adb"));
$("usb-rndis").addEventListener("click", () => void switchUsb("rndis"));

async function refreshSelinux() {
  if (!adb) return;
  try {
    $("selinux-state").textContent = await getSelinux(adb);
  } catch { /* leave as-is */ }
}
async function toggleSelinux(mode: "permissive" | "enforcing") {
  if (!adb) return;
  if (mode === "permissive" &&
      !confirm("Set SELinux permissive? This pokes kernel memory (devmem). Session-only — resets to enforcing on reboot.")) return;
  log(`SELinux → ${mode}…`);
  try {
    const now = await setSelinux(adb, mode);
    $("selinux-state").textContent = now;
    log(`SELinux: ${now}`);
    if (!now.toLowerCase().startsWith(mode.slice(0, 4))) {
      log("SELinux: no known selinux_state address matched (new kernel build?) — ask the researcher to add its address.");
    }
  } catch (e) {
    reportAdbError("selinux", e);
  }
}
$("selinux-perm").addEventListener("click", () => void toggleSelinux("permissive"));
$("selinux-enf").addEventListener("click", () => void toggleSelinux("enforcing"));

// --- input (evdev D-pad) ---
const DPAD_IDS = ["inp-up", "inp-down", "inp-left", "inp-right", "inp-ok", "inp-back", "inp-back-long"];
function setDpad(enabled: boolean) {
  for (const id of DPAD_IDS) btn(id, !enabled);
}
async function tapKey(name: string) {
  if (!adb) return;
  try { await sendKey(adb, name); log(`input: ${name}`); }
  catch (e) { reportAdbError("input", e); }
}
async function longKey(name: string, ms: number) {
  if (!adb) return;
  try { log(`input: ${name} (hold ${ms}ms)…`); await holdKey(adb, name, ms); log(`input: ${name} released`); }
  catch (e) { reportAdbError("input", e); }
}
$("inp-up").addEventListener("click", () => void tapKey("UP"));
$("inp-down").addEventListener("click", () => void tapKey("DOWN"));
$("inp-left").addEventListener("click", () => void tapKey("LEFT"));
$("inp-right").addEventListener("click", () => void tapKey("RIGHT"));
$("inp-ok").addEventListener("click", () => void tapKey("OK"));
$("inp-back").addEventListener("click", () => void tapKey("BACK"));
$("inp-back-long").addEventListener("click", () => void longKey("BACK", 1500));

// --- natartos section ---
async function refreshNatartos() {
  if (!adb) return;
  try {
    const s = await natartosStatus(adb);
    $("nat-status").textContent = s.installed
      ? `${s.version}${s.running ? " · running" : " · stopped"}`
      : "not installed";
    // Pulse the Install button while natartos isn't on the device yet, so the
    // (easily-missed) primary step stands out. Cleared once installed.
    $("nat-install").classList.toggle("cta", !s.installed);
    $("nat-boot").textContent = s.bootcount;
    ($("nat-safe") as HTMLInputElement).checked = s.disabled;
    const modsEl = $("nat-mods");
    if (s.mods.length > 0) {
      // Each entry is "<name>.so" or "<name>.so (enabled)". Show as chips with
      // enabled ones highlighted, plus a count line.
      const parsed = s.mods.map((m) => {
        const on = / \(enabled\)$/.test(m);
        const file = m.replace(/ \(enabled\)$/, "");
        return { file, name: file.replace(/\.so$/, ""), on };
      });
      const enabled = parsed.filter((p) => p.on).length;
      $("nat-mods-count").textContent = "";
      const total = document.createElement("span");
      total.textContent = `installed · ${parsed.length}`;
      const en = document.createElement("span");
      en.className = "en";
      en.textContent = `${enabled} enabled`;
      $("nat-mods-count").append(total, en);
      const list = $("nat-mods-list");
      list.replaceChildren();
      for (const p of parsed) {
        // Clickable: tap toggles the enabled/ flag; a Restart applies it.
        const chip = document.createElement("button");
        chip.type = "button";
        chip.className = p.on ? "modchip on" : "modchip";
        chip.textContent = p.name; // textContent — never innerHTML
        chip.title = p.on ? "enabled — tap to disable" : "disabled — tap to enable";
        chip.addEventListener("click", () => void toggleMod(p.file, !p.on));
        list.appendChild(chip);
      }
      modsEl.hidden = false;
      $("nat-mods-hint").hidden = false;
    } else {
      modsEl.hidden = true;
      $("nat-mods-hint").hidden = true;
    }
    // Uninstall is only meaningful once natartos is on the device.
    const unin = $("nat-uninstall");
    unin.hidden = !s.installed;
    btn("nat-uninstall", !s.installed);
    // Don't offer "Start" when mods are already running — relabel to Restart.
    $("nat-start").textContent = s.running ? "Restart" : "Start";
  } catch (e) {
    // Don't swallow silently — a failed status read is why the mod list can look
    // empty. Surface it so it's diagnosable instead of a blank panel.
    log("natartos status: " + friendlyError(e));
  }
}
async function toggleMod(file: string, on: boolean) {
  if (!adb) return;
  try {
    await setModEnabled(adb, file, on);
    showNatBanner(`${file.replace(/\.so$/, "")} ${on ? "enabled" : "disabled"} — press Restart to apply.`, "busy");
    await refreshNatartos();
  } catch (e) {
    reportAdbError("toggle mod", e);
  }
}
$("nat-refresh").addEventListener("click", () => void refreshNatartos());
$("nat-uninstall").addEventListener("click", async () => {
  if (!adb) return;
  if (!confirm("Uninstall natartos? Restores the stock originals and removes all natartos files from /vendor. Reboot the goggles afterwards to return fully to stock.")) return;
  btn("nat-uninstall", true);
  showNatBanner("Uninstalling natartos…", "busy");
  try {
    await uninstallNatartos(adb, log);
    await refreshNatartos();
    showNatBanner("natartos uninstalled — reboot the goggles to return to fully stock.", "ok");
  } catch (e) {
    reportAdbError("natartos uninstall", e);
    showNatBanner("Uninstall hit a problem: " + friendlyError(e), "err");
  } finally {
    updateNatInstall();
  }
});
$("nat-safe").addEventListener("change", async (e) => {
  if (!adb) return;
  const on = (e.target as HTMLInputElement).checked;
  log(`natartos safe mode → ${on ? "ON (mods disabled next boot)" : "OFF (mods enabled + bootcount reset)"}`);
  try {
    await natartosSafe(adb, on);
  } catch (err) {
    reportAdbError("natartos safe", err);
  }
});
// natartos install: gated on a real release existing (none yet → stays inert).
let natRelease: Release | null = null;
let natManifest: Manifest | null = null;
const customMods: CustomMod[] = [];
function updateNatInstall() {
  btn("nat-install", !((DRY || adb) && natRelease && natManifest));
  btn("nat-start", !(DRY || adb));
  btn("nat-noverity", !(DRY || adb));
  btn("doom-dl", !(DRY || adb));
  btn("nat-dump", !(DRY || adb));
}
function renderMods(m: Manifest) {
  const list = $("mod-list");
  list.replaceChildren();
  for (const mod of m.mods ?? []) {
    const row = document.createElement("label");
    row.className = "modrow";
    const cb = document.createElement("input");
    cb.type = "checkbox";
    cb.value = mod.id;
    cb.checked = mod.default !== false;
    const text = document.createElement("span");
    const name = document.createElement("span");
    name.className = "mname";
    name.textContent = mod.name || mod.id; // textContent — never innerHTML
    text.appendChild(name);
    if (mod.description) {
      const d = document.createElement("span");
      d.className = "mdesc";
      d.textContent = mod.description;
      text.appendChild(d);
    }
    row.append(cb, text);
    if (mod.provides) {
      const tag = document.createElement("span");
      tag.className = "mtag";
      tag.textContent = mod.provides;
      row.appendChild(tag);
    }
    list.appendChild(row);
  }
  $("mod-picker").hidden = (m.mods ?? []).length === 0 && customMods.length === 0;
}
function setVerOption(label: string) {
  const sel = $("nat-ver") as HTMLSelectElement;
  sel.replaceChildren();
  sel.add(new Option(label)); // new Option → textContent, never innerHTML
}
if (DRY) {
  // demo: a fake release + sample mods so the selector is visible without a real release
  natRelease = { tag_name: "v0.3.0-demo", prerelease: false, assets: [] };
  natManifest = {
    schema: 1,
    natartos_version: "v0.3.0-demo",
    loader: { file: "natartos_loader.so", device_path: "/vendor/natartos/natartos_loader.so", sha256: "" },
    mods: [
      { id: "screen", name: "Screen mirror", description: "Grab/stream the display over adb (LiveView hook).", provides: "screen", default: true, file: "mod_screen.so", device_path: "/vendor/natartos/mods/mod_screen.so", sha256: "" },
      { id: "background", name: "Custom wallpaper", description: "Show your own background image on the display.", provides: "background", default: true, file: "mod_bg.so", device_path: "/vendor/natartos/mods/mod_bg.so", sha256: "" },
      { id: "splash", name: "Boot splash", description: "Replace the boot splash screen.", provides: "splash", default: false, file: "mod_splash.so", device_path: "/vendor/natartos/mods/mod_splash.so", sha256: "" },
      { id: "fan", name: "Fan quiet", description: "Lower fan speed for desk use (with a temp guard).", default: false, file: "mod_fan.so", device_path: "/vendor/natartos/mods/mod_fan.so", sha256: "" },
    ],
  };
  setVerOption("v0.3.0-demo");
  renderMods(natManifest);
  updateNatInstall();
} else {
  fetchLatestRelease().then(async (rel) => {
    natRelease = rel;
    setVerOption(rel ? `${rel.tag_name}${rel.prerelease ? " (pre)" : ""}` : "no releases yet");
    if (rel) {
      try {
        natManifest = await loadManifest(rel);
        renderMods(natManifest);
      } catch {
        /* manifest unavailable — install stays gated */
      }
    }
    updateNatInstall();
  });
}
$("mod-custom").addEventListener("change", async (e) => {
  const files = (e.target as HTMLInputElement).files;
  if (!files) return;
  for (const f of Array.from(files)) {
    customMods.push({ name: f.name, bytes: new Uint8Array(await f.arrayBuffer()) });
  }
  $("mod-custom-list").textContent = customMods.length ? `custom: ${customMods.map((c) => c.name).join(", ")} (untrusted)` : "";
  $("mod-picker").hidden = false;
});
function showNatBanner(html: string, state: "busy" | "ok" | "err") {
  const el = $("nat-banner");
  el.innerHTML = html;
  el.dataset.state = state;
  el.hidden = false;
  el.scrollIntoView({ behavior: "smooth", block: "center" });
}

$("nat-install").addEventListener("click", async () => {
  if (!natRelease || !natManifest) return;
  const modIds = Array.from(
    $("mod-list").querySelectorAll<HTMLInputElement>('input[type="checkbox"]:checked'),
  ).map((cb) => cb.value);
  const total = modIds.length + customMods.length;
  if (!confirm(`Install natartos ${natRelease.tag_name} with ${total} mod(s)? Writes to /vendor and survives reboot. A boot-loop auto-disables mods (brick-prevention).`)) return;
  if (DRY) {
    log(`demo: would install natartos ${natRelease.tag_name} (${total} mod(s)) — no real device.`);
    return;
  }
  if (!adb) return;
  btn("nat-install", true);
  log(`installing natartos ${natRelease.tag_name}…`);
  showNatBanner("Installing natartos… the goggles may blink or briefly reconnect — don't unplug. This takes up to a minute.", "busy");
  try {
    const installedIds = await installFromRelease(adb, natRelease, natManifest, { modIds, custom: customMods }, log);
    // anonymous per-mod telemetry (release mods only; custom mods never reported)
    for (const id of installedIds) stat("mod_install", { stage: natRelease.tag_name, reason: id });
    // Activate now — installing without starting leaves dji_glasses stock, so the
    // mods look "not enabled". Parity with the CLI (`toolya natartos install`).
    await startMods(adb, log);
    // Doom needs a WAD to launch — fetch Freedoom now so the mod works without a
    // separate step. Non-fatal: a failed download just means "use Doom → Install".
    if (installedIds.includes("doom")) {
      try {
        showNatBanner("Installing the Doom WAD (Freedoom, ~24 MB)…", "busy");
        await ensureDoomWad(adb, log);
      } catch (e) {
        log("doom WAD: " + friendlyError(e));
      }
    }
    void refreshNatartos();
    showNatBanner(`✓ natartos ${natRelease.tag_name} installed — mods active and reboot-safe.`, "ok");
  } catch (e) {
    reportAdbError("natartos install", e);
    showNatBanner("Install hit a problem: " + friendlyError(e) + " — see the log on the tool page.", "err");
  } finally {
    updateNatInstall();
  }
});

$("nat-start").addEventListener("click", async () => {
  if (DRY) { log("demo: would start mods — no real device."); return; }
  if (!adb) return;
  btn("nat-start", true);
  showNatBanner("Starting mods… the goggles may blink or briefly reconnect — don't unplug.", "busy");
  try {
    await startMods(adb, log);
    void refreshNatartos();
    showNatBanner("✓ mods started.", "ok");
  } catch (e) {
    reportAdbError("natartos start", e);
    showNatBanner("Start hit a problem: " + friendlyError(e) + " — see the log on the tool page.", "err");
  } finally {
    btn("nat-start", false);
  }
});
$("nat-noverity").addEventListener("click", async () => {
  if (!confirm(
    "Disable dm-verity for THIS SESSION ONLY?\n\n" +
    "DANGER: Do NOT modify /system and reboot — dm-verity mode 2 will BRICK the device.\n" +
    "No known recovery exists for DJI Goggles 3."
  )) return;
  if (DRY) { log("demo: would run dm_noverity — no real device."); return; }
  if (!adb || !natRelease) return;
  btn("nat-noverity", true);
  try {
    const asset = natRelease.assets.find((a) => a.name === "dm_noverity");
    if (!asset) throw new Error("dm_noverity not found in release assets — build it locally and use the CLI");
    log("downloading dm_noverity…");
    const bytes = await fetch(`/api/dl?u=${encodeURIComponent(asset.browser_download_url)}`)
      .then((r) => { if (!r.ok) throw new Error(`download failed (${r.status})`); return r.arrayBuffer(); })
      .then((b) => new Uint8Array(b));
    await noverity(adb, bytes, log);
  } catch (e) {
    reportAdbError("natartos noverity", e);
  } finally {
    btn("nat-noverity", false);
  }
});

// --- doom WAD ---
$("doom-dl").addEventListener("click", async () => {
  if (DRY) { log("demo: would install Doom (Freedoom WAD) — no real device."); return; }
  if (!adb) return;
  btn("doom-dl", true);
  try {
    await downloadDoomWad(adb, log);
  } catch (e) {
    reportAdbError("doom download", e);
  } finally {
    btn("doom-dl", false);
  }
});
$("doom-file").addEventListener("change", async (e) => {
  const file = (e.target as HTMLInputElement).files?.[0];
  if (!file || !adb) return;
  btn("doom-dl", true);
  try {
    const bytes = new Uint8Array(await file.arrayBuffer());
    await pushDoomWad(adb, bytes, log);
  } catch (err) {
    reportAdbError("doom push", err);
  } finally {
    btn("doom-dl", false);
  }
});

// --- dump (diagnostics for bug reports) ---
$("nat-dump").addEventListener("click", async () => {
  if (!adb) return;
  btn("nat-dump", true);
  log("collecting diagnostic dump…");
  try {
    const text = await natartosDump(adb);
    const ta = $("dump-text") as HTMLTextAreaElement;
    ta.value = text;
    ($("dump-dialog") as HTMLDialogElement).showModal();
  } catch (err) {
    reportAdbError("dump", err);
  } finally {
    btn("nat-dump", false);
  }
});
$("dump-copy").addEventListener("click", () => {
  const ta = $("dump-text") as HTMLTextAreaElement;
  void navigator.clipboard.writeText(ta.value).then(() => {
    $("dump-copied").hidden = false;
    setTimeout(() => { $("dump-copied").hidden = true; }, 2000);
  });
});
$("dump-close").addEventListener("click", () => {
  ($("dump-dialog") as HTMLDialogElement).close();
});

// --- screen (LiveView frame grab / live stream over adb) ---
const scrFps = $("scr-fps");
let stream: StreamHandle | null = null;

const FRAME_BYTES = 1920 * 1080 * 4; // 8294400, raw BGRA
const SCREENCAP_PATH = "/data/local/tmp/screencap.bin"; // where the display hook writes the frame

async function grabScreen() {
  if (!adb) return;
  try {
    await run(adb, "kill -WINCH $(pidof dji_glasses)");
    await new Promise((r) => setTimeout(r, 150));
    const bytes = await pullFile(adb, SCREENCAP_PATH);
    if (bytes.length !== FRAME_BYTES) {
      let tail = "";
      try {
        tail = (await run(adb, "tail -c 400 /data/local/tmp/screencap.log")).trim();
      } catch { /* log may not exist */ }
      log(`screen: got ${bytes.length}B (expected ${FRAME_BYTES}). hook log tail: ${tail || "(empty)"}`);
      return;
    }
    bgraToCanvas(bytes, $("scr-canvas") as HTMLCanvasElement);
    log(`screen frame ok (${(bytes.length / 1e6).toFixed(1)} MB)`);
  } catch (e) {
    reportAdbError("screen grab", e);
  }
}

function stopStream() {
  if (stream) { stream.stop(); stream = null; }
  ($("scr-auto") as HTMLInputElement).checked = false;
  if (scrFps) scrFps.textContent = "";
}

$("scr-fs").addEventListener("click", () => {
  const cv = $("scr-canvas") as HTMLCanvasElement;
  if (document.fullscreenElement) void document.exitFullscreen();
  else cv.requestFullscreen?.().catch((e) => log("fullscreen: " + (e as Error).message));
});

$("scr-grab").addEventListener("click", () => void grabScreen());
$("scr-auto").addEventListener("change", async (e) => {
  if ((e.target as HTMLInputElement).checked) {
    if (!adb) { stopStream(); return; }
    try {
      stream = await startStream(
        adb,
        $("scr-canvas") as HTMLCanvasElement,
        (fps, mode) => {
          if (scrFps) scrFps.textContent = `${fps.toFixed(1)} fps · ${mode}`;
        },
        (err) => {
          reportAdbError("stream", err);
          stopStream();
        },
      );
      log(`stream started (${stream.mode} mode)`);
    } catch (e) {
      reportAdbError("stream start", e);
      stopStream();
    }
  } else {
    stopStream();
    log("stream stopped");
  }
});

// --- background image converter ---
const bgFile = $("bg-file") as HTMLInputElement;
const bgFit = $("bg-fit") as HTMLSelectElement;
let bgJpeg: Uint8Array<ArrayBuffer> | null = null;

async function bgUpdate() {
  const f = bgFile.files?.[0];
  if (!f) return;
  $("bg-fname").textContent = f.name;
  const fit = bgFit.value as "cover" | "contain";
  const err = $("bg-err");
  err.hidden = true;
  try {
    await previewInto($("bg-preview") as HTMLCanvasElement, f, fit);
    bgJpeg = await toScreenJpeg(f, fit);
    btn("bg-download", false);
    btn("bg-apply", !adb);
    log(`background ready: ${(bgJpeg.length / 1e6).toFixed(1)} MB JPEG (1920×1080)`);
  } catch (e) {
    // Decode failed (usually a HEIC/HEIF phone photo the browser can't read):
    // reset state so a bad pick can't leave stale bytes / enabled buttons, and
    // show the reason on the card — the console log is off-screen.
    bgJpeg = null;
    btn("bg-download", true);
    btn("bg-apply", true);
    const m = (e as Error).message;
    err.textContent = /load image/.test(m)
      ? "Couldn't read this image — HEIC/HEIF photos aren't supported. Export it as JPG or PNG and try again."
      : "Couldn't process this image: " + m;
    err.hidden = false;
    log("image error: " + m);
  }
}
bgFile.addEventListener("change", () => void bgUpdate());
bgFit.addEventListener("change", () => void bgUpdate());

$("bg-download").addEventListener("click", () => {
  if (!bgJpeg) return;
  const url = URL.createObjectURL(new Blob([bgJpeg], { type: "image/jpeg" }));
  const a = document.createElement("a");
  a.href = url;
  a.download = "background_disconnect.jpg";
  a.click();
  URL.revokeObjectURL(url);
});

$("bg-apply").addEventListener("click", async () => {
  if (!adb || !bgJpeg) return;
  if (!confirm("Set this as the goggles' disconnect background? Needs natartos installed with the Custom Background mod — it will be enabled and mods restarted.")) return;
  btn("bg-apply", true);
  $("log").scrollIntoView({ behavior: "smooth", block: "center" });
  try {
    await applyBackground(adb, bgJpeg, log);
    log("background applied — shows on the disconnect/idle screen.");
    void refreshNatartos();
  } catch (e) {
    reportAdbError("background apply", e);
  } finally {
    btn("bg-apply", !adb);
  }
});

async function switchUsb(mode: "adb" | "rndis") {
  if (!adb) return;
  if (!confirm(`Switch USB to ${mode}? This re-enumerates USB and drops the connection.`)) return;
  log(`switching USB → ${mode} (sys.usb.config = ${await usbCfgString(mode)})…`);
  await setUsbMode(adb, mode);
  if (mode === "rndis") {
    log("RNDIS requested — on this machine bring up usb0:");
    log("  sudo ip addr add 192.168.60.1/24 dev usb0");
    log("  sudo ip link set usb0 up");
    log("  device: 192.168.60.2");
  }
  log("USB re-enumerated — connection dropped.");
  await disconnect();
}
$("btn-copy").addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText($("log").textContent ?? "");
    log("(log copied)");
  } catch {
    log("clipboard unavailable");
  }
});

function startHeartbeat() {
  stopHeartbeat();
  heartbeat = setInterval(async () => {
    if (!adb) return;
    try {
      await run(adb, "echo ok");
    } catch (e) {
      if (isDisconnect(e)) void disconnect();
    }
  }, 30_000);
}
function stopHeartbeat() {
  if (heartbeat) { clearInterval(heartbeat); heartbeat = null; }
}

async function disconnect() {
  stopHeartbeat();
  // Stop PC Stream before closing adb — it must touch fb_stop over the live link.
  if (pcStream?.active) { try { await pcStream.stop(); } catch { /* ignore */ } }
  if (eyepiece?.active) { try { await eyepiece.stop("disconnect"); } catch { /* ignore */ } }
  try { term?.dispose(); } catch { /* ignore */ }
  term = null;
  $("term").innerHTML = "";
  try { await adb?.close(); } catch { /* ignore */ }
  adb = null;
  updateNatInstall();
  setStatus("not connected");
  $("success").hidden = true;
  $("nat-sec-state").textContent = "connect first";
  setPrimary("connect"); // ready to reconnect
  $("devinfo").textContent = "";
  $("usage-hdr").hidden = true;
  $("stat-power").textContent = "—";
  $("stat-motor").textContent = "—";
  btn("btn-disconnect", true);
  btn("usb-adb", true);
  btn("usb-rndis", true);
  stopStream();
  setDpad(false);
  btn("scr-grab", true);
  btn("ps-stream", true);
  btn("ep-stream", true);
  $("ep-status").textContent = "connect ADB first";
  btn("bg-apply", true);
  btn("nat-start", true);
  btn("nat-noverity", true);
  btn("doom-dl", true);
  btn("nat-dump", true);
  $("nat-mods").hidden = true;
  if (canUsb) btn("btn-connect", false);
  log("disconnected");
}

async function refreshUsage() {
  if (!adb) return;
  log("reading /data/glasses_statistics.json …");
  try {
    const u = await readUsage(adb);
    $("stat-power").textContent = u.powerOns?.toLocaleString() ?? "?";
    $("stat-motor").textContent = u.motorStarts?.toLocaleString() ?? "?";
  } catch (e) {
    reportAdbError("usage", e);
  }
}

// The G3 interactive shell (even shell_v2 PTY) echoes input but returns no output.
// So the terminal is a line editor inside xterm — you type directly into it — backed
// by working one-shot command mode, with cwd tracked so `cd` persists.
const NOISE = /\[sec_debug_state:[^\]]*\][^\r\n]*\r?\n?/g;

function startTerminal(adb: Adb) {
  const t = new Terminal({
    fontSize: 13,
    convertEol: false,
    cursorBlink: true,
    fontFamily: '"Geist Mono", ui-monospace, Menlo, monospace',
    theme: { background: "#0a0c10", foreground: "#e7e9ee", cursor: "#5cc8ff" },
  });
  term = t;
  const fit = new FitAddon();
  t.loadAddon(fit);
  t.open($("term"));
  fit.fit();
  addEventListener("resize", () => fit.fit());

  let cwd = "/";
  let line = "";
  let busy = false;
  const prompt = () => t.write(`\r\n\x1b[36m${cwd}\x1b[0m $ `);
  t.writeln("toolya root shell — type a command and press Enter. (cd persists.)");
  prompt();
  t.focus();

  t.onData(async (data) => {
    if (busy) return;
    for (const ch of data) {
      if (ch === "\r") {
        t.write("\r\n");
        const cmd = line;
        line = "";
        if (!cmd.trim()) { prompt(); return; }
        busy = true;
        try {
          const { output, cwd: nc } = await runInDir(adb, cmd, cwd);
          cwd = nc;
          const clean = output.replace(NOISE, "").replace(/\n/g, "\r\n");
          if (clean.trim()) t.write(clean.endsWith("\r\n") ? clean : clean + "\r\n");
        } catch (err) {
          t.write(`\x1b[31m${friendlyError(err)}\x1b[0m\r\n`);
          if (isDisconnect(err)) { setStatus("disconnected", "err"); void disconnect(); return; }
        } finally {
          busy = false;
        }
        prompt();
        return;
      } else if (ch === "\x7f") {
        if (line.length) { line = line.slice(0, -1); t.write("\b \b"); }
      } else if (ch === "\x03") {
        line = "";
        t.write("^C");
        prompt();
      } else if (ch >= " ") {
        line += ch;
        t.write(ch);
      }
    }
  });
  log("terminal ready — root shell");
}

// --- PC Stream (browser → goggles over USB/ADB) ---
let pcStream: PcStreamHandle | null = null;
let eyepiece: EyepieceHandle | null = null;

$("ep-stream").addEventListener("click", async () => {
  const b = $("ep-stream") as HTMLButtonElement;
  if (eyepiece?.active) { await eyepiece.stop(); return; }
  if (!adb) { log("eyepiece: connect ADB first"); return; }
  b.textContent = "Stop Eyepiece";
  $("ep-status").textContent = "starting…";
  try {
    eyepiece = await startEyepiece({
      adb,
      fps: parseInt(($("ep-fps") as HTMLSelectElement).value),
      bitrate: parseInt(($("ep-br") as HTMLSelectElement).value),
      metrics: ($("ep-metrics") as HTMLInputElement).checked,
      onLog: (l) => log(l),
      onStatus: (s) => {
        const f = (v: number) => Number.isFinite(v) ? v.toFixed(1) : "–";
        $("ep-status").textContent = "streaming";
        $("ep-stats").textContent =
          `${s.fps.toFixed(0)} fps · ${s.mbps.toFixed(1)} Mbit/s · send→shown ${f(s.sendToShownP50)}/${f(s.sendToShownP95)} ms` +
          (s.captureToSendP50 !== null ? ` · capture→send ${f(s.captureToSendP50)} ms` : "") +
          ` · enc ${f(s.encodeP50)} · dec ${f(s.decodeP50)} ms · drop ${s.dropped}+${s.encDropped}`;
        log(`eyepiece: ${$("ep-stats").textContent}`);
      },
      onStop: (reason) => {
        log(`eyepiece: ${reason}`);
        $("ep-status").textContent = "stopped";
        b.textContent = "Start Eyepiece";
        eyepiece = null;
      },
    });
  } catch (e) {
    log(`eyepiece: ${(e as Error).message}`);
    $("ep-status").textContent = "error";
    b.textContent = "Start Eyepiece";
  }
});

const psQuality = $("ps-quality") as HTMLInputElement;
const psFpsSlider = $("ps-fps") as HTMLInputElement;
const psRes = $("ps-res") as HTMLSelectElement;
const psStats = $("ps-stats");

psQuality.addEventListener("input", () => { $("ps-qval").textContent = psQuality.value + "%"; });
psFpsSlider.addEventListener("input", () => { $("ps-fpsval").textContent = psFpsSlider.value; });

$("ps-stream").addEventListener("click", async () => {
  if (pcStream?.active) {
    pcStream.stop();
    return;
  }
  if (!adb) { log("pc-stream: connect ADB first"); return; }

  const streamBtn = $("ps-stream") as HTMLButtonElement;
  streamBtn.textContent = "Stop streaming";
  $("ps-status").textContent = "starting…";
  log("pc-stream: starting…");

  try {
    pcStream = await startPcStream({
      adb,
      quality: parseInt(psQuality.value) / 100,
      fpsCap: parseInt(psFpsSlider.value),
      height: parseInt(psRes.value),
      preview: $("ps-preview") as HTMLCanvasElement,
      onStatus: (info) => {
        psStats.textContent = `${info.fps} fps · ${info.kbps} KB/s · ${info.encMs}ms enc · ${info.dropped} drop`;
        $("ps-status").textContent = "streaming";
      },
      onStop: (reason) => {
        log(`pc-stream: ${reason}`);
        $("ps-status").textContent = "stopped";
        streamBtn.textContent = "Start streaming";
        psStats.textContent = "";
        pcStream = null;
      },
      onError: (err) => {
        log(`pc-stream: ${err}`);
      },
    });
    log(`pc-stream: active (JPEG over ADB stdin → fb_receiver)`);
  } catch (e) {
    log(`pc-stream: ${(e as Error).message}`);
    $("ps-status").textContent = "error";
    streamBtn.textContent = "Start streaming";
  }
});
