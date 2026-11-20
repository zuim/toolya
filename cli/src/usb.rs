//! Native USB transport for DUML (DJI Goggles 3). Pure Rust via nusb.
//!
//! Uses `toolya_core::enable_plan()` — same plan the Web client executes over
//! WebUSB. The transport differs (nusb vs WebUSB), the protocol is shared.

use std::future::Future;

use toolya_core::{DumlPort, EnablePlan};

pub const VID: u16 = 0x2CA3;
pub const PID: u16 = 0x0020;

/// A DUML data pipe: an interface number plus its OUT/IN bulk endpoints.
struct Pipe {
    iface_num: u8,
    ep_out: u8,
    ep_in: u8, // IN address already OR'd with 0x80
    /// True when this pipe was resolved to an interface that actually exposes
    /// the wanted DUML endpoints (vs falling back to the plan's nominal iface).
    matched: bool,
}

pub struct Goggles {
    dev: nusb::Device,
    serial: Option<String>,
    pipes: Vec<Pipe>,
    adb_mode: bool,
    /// Human-readable interface/endpoint layout, for precise diagnostics.
    iface_summary: String,
    /// True when at least one pipe matched real DUML endpoints — i.e. the device
    /// actually exposes a DUML data interface (a Goggles 3 in DUML mode does).
    duml_eps_found: bool,
}

impl Goggles {
    /// Find and open the goggles in DUML (pre-adb) mode, resolving the candidate
    /// DUML pipes. The right pipe is picked later by handshake (`run_enable`).
    pub fn open(ports: &[DumlPort]) -> Result<Self, String> {
        let di = nusb::list_devices()
            .map_err(|e| format!("list usb: {e}"))?
            .find(|d| d.vendor_id() == VID && d.product_id() == PID)
            .ok_or_else(|| format!(
                "goggles not found (VID {VID:#06x} PID {PID:#06x}) — plugged in, in DUML mode?"
            ))?;
        let serial = di.serial_number().map(|s| s.to_string());
        let dev = di.open().map_err(|e| format!("open device: {e}"))?;
        // No macOS driver matches the DUML device, so it can sit unconfigured —
        // then writes go nowhere and reads stay silent. WebUSB selects config 1;
        // mirror that before claiming (harmless if it is already active).
        if dev.active_configuration().map(|c| c.configuration_value()).unwrap_or(0) != 1 {
            let _ = dev.set_configuration(1);
        }

        // 8-interface DUML mode: the data pipe is a specific interface, and which
        // one carries DUML differs across firmware builds. Resolve each plan port
        // to the interface that actually exposes its endpoints (like a known-good
        // probe), and keep all candidates — the handshake decides which is live.
        let debug = std::env::var("TOOLYA_USB_DEBUG").is_ok();
        let mut pipes: Vec<Pipe> = Vec::new();
        let mut adb_mode = false;
        let mut iface_summary = String::new();
        if let Ok(cfg) = dev.active_configuration() {
            // ADB already enabled: the device exposes the Android ADB interface
            // (vendor-specific class 0xff / subclass 0x42 / protocol 0x01). DUML
            // mode has only CDC interfaces, so this cleanly tells the modes apart.
            // Build the interface/endpoint summary in the same pass, for diagnostics.
            for g in cfg.interfaces() {
                for alt in g.alt_settings() {
                    if alt.class() == 0xff && alt.subclass() == 0x42 && alt.protocol() == 0x01 {
                        adb_mode = true;
                    }
                    let eps: Vec<String> = alt.endpoints().map(|e| format!("{:#04x}", e.address())).collect();
                    if !iface_summary.is_empty() { iface_summary.push_str("; "); }
                    iface_summary.push_str(&format!(
                        "iface {} {:#04x}/{:#04x}/{:#04x} eps=[{}]",
                        alt.interface_number(), alt.class(), alt.subclass(), alt.protocol(), eps.join(",")));
                }
            }
            if debug {
                eprintln!("[usb] config {} — {}", cfg.configuration_value(), iface_summary);
            }
            for p in ports {
                let want_out = p.ep_out;
                let want_in = p.ep_in | 0x80;
                let mut iface_num = None;
                'scan: for g in cfg.interfaces() {
                    for alt in g.alt_settings() {
                        let mut out = false;
                        let mut inp = false;
                        for e in alt.endpoints() {
                            if e.address() == want_out { out = true; }
                            if e.address() == want_in { inp = true; }
                        }
                        if out && inp {
                            iface_num = Some(alt.interface_number());
                            break 'scan;
                        }
                    }
                }
                // Did this port resolve to an interface actually exposing its
                // endpoints? If not, we fall back to the nominal — but a device
                // where *no* port matches is not a Goggles 3 in DUML mode.
                let matched = iface_num.is_some();
                let iface_num = iface_num.unwrap_or(p.iface);
                if !pipes.iter().any(|q| q.iface_num == iface_num) {
                    pipes.push(Pipe { iface_num, ep_out: want_out, ep_in: want_in, matched });
                }
            }
        }
        // Fallback when the config could not be read: trust the plan's nominals.
        if pipes.is_empty() {
            for p in ports {
                pipes.push(Pipe { iface_num: p.iface, ep_out: p.ep_out, ep_in: p.ep_in | 0x80, matched: false });
            }
        }
        let duml_eps_found = pipes.iter().any(|p| p.matched);
        Ok(Self { dev, serial, pipes, adb_mode, iface_summary, duml_eps_found })
    }

    pub fn serial(&self) -> Option<&str> {
        self.serial.as_deref()
    }

    /// True when the goggles already expose the Android ADB interface — ADB is
    /// enabled, so the caller should connect instead of running the enable plan.
    pub fn adb_mode(&self) -> bool {
        self.adb_mode
    }

    /// Execute the adb-enable plan. The pipe whose handshake returns `</dji>` is
    /// the live DUML interface; the WMS cmd_set 0x07 candidate scan runs on it.
    /// If no pipe answers, report a precise diagnosis instead of firing unknown
    /// commands at the device.
    pub fn run_enable(&self, plan: &EnablePlan, scan: bool, require_fw: Option<&str>, log: impl Fn(&str)) -> Result<(), String> {
        let mut any_data = false;
        let mut version: Option<String> = None;
        for pipe in &self.pipes {
            let iface = match self.dev.claim_interface(pipe.iface_num) {
                Ok(i) => i,
                Err(_) => continue,
            };
            drain(&iface, pipe.ep_in);
            let blob = handshake(&iface, pipe, plan);
            if !blob.is_empty() { any_data = true; }
            if std::env::var("TOOLYA_USB_DEBUG").is_ok() && !blob.is_empty() {
                let n = blob.len().min(512);
                let hx: String = blob[..n].iter().map(|b| format!("{b:02x}")).collect();
                log(&format!("[usb] iface {} blob ({} B, first {}): {}", pipe.iface_num, blob.len(), n, hx));
                log(&format!("[usb] iface {} reassembled: {}", pipe.iface_num, printable_ascii(&reassemble_duml(&blob)).trim()));
            }
            if version.is_none() { version = parse_dji_version(&blob); }
            if contains_subslice(&blob, &plan.expect) {
                log(&format!("handshake ok (iface {})", pipe.iface_num));
                if let Some(v) = &version { log(&format!("device: {v}")); }
                if let Some(want) = require_fw {
                    match &version {
                        Some(v) if v.contains(want) => {}
                        Some(v) => return Err(format!("firmware check failed: device reports '{v}', expected it to contain '{want}' — nothing sent")),
                        None => return Err(format!("firmware check failed: could not read firmware version from the handshake (wanted '{want}') — nothing sent")),
                    }
                }
                return enable(&iface, pipe, plan, scan, &log);
            }
            log(&format!("iface {}: no </dji>", pipe.iface_num));
        }
        Err(self.diagnose(any_data, version))
    }

    /// Turn a failed handshake into a precise message instead of a blanket
    /// "not answering DUML": separate "wrong/old device" from "wrong pipe" from
    /// "silent", using the enumerated descriptors and whatever bytes came back.
    fn diagnose(&self, any_data: bool, version: Option<String>) -> String {
        if self.adb_mode {
            return "ADB already enabled (Android ADB interface present) — run `adb devices`".into();
        }
        if !self.duml_eps_found {
            return format!(
                "no DUML data interface (expected bulk endpoints 0x04/0x85 or 0x01/0x81): [{}]. \
                 Not a Goggles 3 in DUML mode — older goggles (G2/ZV900) or a non-DUML USB mode \
                 do not expose the adb-enable handler.",
                self.iface_summary);
        }
        if let Some(v) = version {
            return format!(
                "DUML interface answered (device: {v}) but never returned </dji> — adb-enable \
                 (WMS cmd_set 0x07) may be unsupported on this firmware. [{}]",
                self.iface_summary);
        }
        if any_data {
            return format!(
                "DUML interface returned data but no </dji> — wrong pipe or timing, not the \
                 adb-enable target. [{}]",
                self.iface_summary);
        }
        format!(
            "goggles silent on every DUML pipe (no bytes back). Check the cable and that the \
             goggles are in DUML (pre-ADB) mode. [{}]",
            self.iface_summary)
    }
}

fn bulk_out(iface: &nusb::Interface, ep: u8, data: &[u8]) -> Result<(), String> {
    pollster::block_on(async {
        iface
            .bulk_out(ep, data.to_vec())
            .await
            .into_result()
            .map(|_| ())
            .map_err(|e| format!("bulk out: {e}"))
    })
}

/// Bulk IN with a deadline. A quiet endpoint never completes the transfer, so
/// without a timeout the whole enable hangs (the drain read, with no preceding
/// write, is the first to block). On timeout the transfer future is dropped —
/// nusb cancels it, so nothing leaks onto the endpoint.
fn bulk_in(iface: &nusb::Interface, ep: u8, max: usize, timeout_ms: u64) -> In {
    let fut = async {
        iface
            .bulk_in(ep, nusb::transfer::RequestBuffer::new(max))
            .await
            .into_result()
            .map_err(|e| format!("bulk in: {e}"))
    };
    match block_on_timeout(fut, timeout_ms) {
        Some(Ok(data)) => In::Data(data),
        Some(Err(e)) => In::Closed(e),
        None => In::Timeout,
    }
}

fn drain(iface: &nusb::Interface, ep_in: u8) {
    for _ in 0..15 {
        if !matches!(bulk_in(iface, ep_in, 65536, DRAIN_MS), In::Data(_)) { break; }
    }
}

/// Send the handshake packets on this pipe and return every byte read back.
/// Stops early once `</dji>` appears. The caller checks the blob for `</dji>`
/// and can parse the device version out of it for diagnostics.
fn handshake(iface: &nusb::Interface, pipe: &Pipe, plan: &EnablePlan) -> Vec<u8> {
    let mut blob = Vec::new();
    for (i, pkt) in plan.handshake.iter().enumerate() {
        if bulk_out(iface, pipe.ep_out, pkt).is_err() { break; }
        let delay = if i < plan.init_count { 50 } else { 60 };
        std::thread::sleep(std::time::Duration::from_millis(delay));
        if let In::Data(data) = bulk_in(iface, pipe.ep_in, 65536, HS_MS) {
            blob.extend_from_slice(&data);
            if contains_subslice(&blob, &plan.expect) { return blob; }
        }
    }
    for _ in 0..20 {
        if let In::Data(data) = bulk_in(iface, pipe.ep_in, 65536, HS_MS) {
            blob.extend_from_slice(&data);
            if contains_subslice(&blob, &plan.expect) { return blob; }
        }
    }
    blob
}

/// Enable adb on this (handshake-confirmed) pipe. Primary: one packet with the
/// empirically-confirmed cmd_id 0x64, waiting ~5s for the re-enumeration. If
/// that doesn't switch (other firmware), fall back to the full 0x60–0x6F scan.
fn enable(iface: &nusb::Interface, pipe: &Pipe, plan: &EnablePlan, scan_fallback: bool, log: &impl Fn(&str)) -> Result<(), String> {
    std::thread::sleep(std::time::Duration::from_millis(200));
    drain(iface, pipe.ep_in);
    let cid = plan.enable[10];
    log(&format!("enabling adb (cmd_id 0x{cid:02x}, iface {})…", pipe.iface_num));
    if send_and_wait(iface, pipe, &plan.enable, ENABLE_WAIT_MS, log) {
        log("adb enabling — goggles re-enumerate in ~5s. Then `adb devices`.");
        return Ok(());
    }
    if !scan_fallback {
        return Err(format!("cmd_id 0x{cid:02x} did not switch and --no-scan is set — no other commands were sent"));
    }
    log(&format!("cmd_id 0x{cid:02x} did not switch — scanning 0x60–0x6F…"));
    scan(iface, pipe, plan, log)
}

/// Send one packet and wait up to `wait_ms` for the device to re-enumerate.
/// Returns true on disconnect (enable took) and logs the winning cmd_id. An ACK
/// or silence just means "still here" — keep waiting until the deadline.
fn send_and_wait(iface: &nusb::Interface, pipe: &Pipe, pkt: &[u8], wait_ms: u64, log: &impl Fn(&str)) -> bool {
    // cmd_id is byte 10 of the framed packet (4-byte frame header + HOST dst seq
    // ack cmd_set). Log the winner so the real trigger is captured empirically.
    let cid = pkt[10];
    if bulk_out(iface, pipe.ep_out, pkt).is_err() {
        log(&format!("adb enabled by cmd_id 0x{cid:02x}"));
        return true;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(wait_ms);
    while std::time::Instant::now() < deadline {
        if let In::Closed(_) = bulk_in(iface, pipe.ep_in, 1024, SCAN_MS) {
            log(&format!("adb enabled by cmd_id 0x{cid:02x}"));
            return true;
        }
    }
    false
}

/// Fallback cmd_id scan (0x60–0x6F) for firmwares where the primary differs.
/// Ok on re-enumeration (logs the winner); Err when the range is exhausted.
fn scan(iface: &nusb::Interface, pipe: &Pipe, plan: &EnablePlan, log: &impl Fn(&str)) -> Result<(), String> {
    drain(iface, pipe.ep_in);
    for pkt in &plan.candidates {
        let cid = pkt[10];
        if bulk_out(iface, pipe.ep_out, pkt).is_err() {
            log(&format!("adb enabled by cmd_id 0x{cid:02x}"));
            log("adb enabling — goggles re-enumerate in ~5s. Then `adb devices`.");
            return Ok(());
        }
        for _ in 0..8 {
            match bulk_in(iface, pipe.ep_in, 1024, SCAN_MS) {
                In::Data(r) if r.len() >= 11 && r[8] == 0xC0 && r[10] == cid => break,
                In::Data(_) => continue,
                In::Timeout => break,
                In::Closed(_) => {
                    log(&format!("adb enabled by cmd_id 0x{cid:02x}"));
                    log("adb enabling — goggles re-enumerate in ~5s. Then `adb devices`.");
                    return Ok(());
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Err("no cmd_id triggered USB disconnect".into())
}

/// Clean device/version string from a DUML `</dji>` response, for diagnostics.
/// The XML is fragmented across DUML frames (each prefixed by a 0x55 header), so
/// the raw blob interleaves framing bytes into the text. Reassemble the frame
/// payloads first, then pull the `id` + firmware attributes.
fn parse_dji_version(blob: &[u8]) -> Option<String> {
    for src in [reassemble_duml(blob), blob.to_vec()] {
        let text = printable_ascii(&src);
        let dev = attr(&text, "id=\"");
        let fw = attr(&text, "formal=\"").or_else(|| attr(&text, "version=\""));
        if dev.is_some() || fw.is_some() {
            return Some(match (dev, fw) {
                (Some(d), Some(f)) => format!("{d} fw {f}"),
                (Some(d), None) => d.to_string(),
                (None, Some(f)) => format!("fw {f}"),
                _ => unreachable!(),
            });
        }
    }
    None
}

/// Concatenate the payloads of the DUML frames in `blob`, dropping each frame's
/// 0x55 header + CRC16. This strips the framing bytes that otherwise splice into
/// a fragmented `</dji>` XML response.
fn reassemble_duml(blob: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= blob.len() {
        if blob[i] != 0x55 { i += 1; continue; }
        let len = blob[i + 1] as usize | (((blob[i + 2] & 0x03) as usize) << 8);
        if len < 6 || i + len > blob.len() { i += 1; continue; }
        out.extend_from_slice(&blob[i + 4..i + len - 2]);
        i += len;
    }
    out
}

/// Map a byte slice to a string, replacing non-printable bytes with a space.
fn printable_ascii(b: &[u8]) -> String {
    b.iter().map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { ' ' }).collect()
}

/// Extract an XML attribute value: the text after `key` (e.g. `id="`) up to the
/// next quote. Returns None if absent or empty.
fn attr<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let i = s.find(key)? + key.len();
    let rest = &s[i..];
    let end = rest.find('"')?;
    let v = rest[..end].trim();
    if v.is_empty() { None } else { Some(v) }
}

/// Outcome of a bulk IN: data, a deadline with nothing received, or a real USB
/// error (the device dropped off the bus — during enable that means success).
enum In {
    Data(Vec<u8>),
    Timeout,
    Closed(String),
}

/// Drive `fut` to completion, or return `None` if `ms` elapses first. The loser
/// is dropped when this returns, which cancels a still-pending nusb transfer.
fn block_on_timeout<T>(fut: impl std::future::Future<Output = T>, ms: u64) -> Option<T> {
    pollster::block_on(async {
        let mut fut = std::pin::pin!(fut);
        let mut delay = std::pin::pin!(Delay::new(std::time::Duration::from_millis(ms)));
        std::future::poll_fn(|cx| {
            if let std::task::Poll::Ready(v) = fut.as_mut().poll(cx) {
                return std::task::Poll::Ready(Some(v));
            }
            match delay.as_mut().poll(cx) {
                std::task::Poll::Ready(()) => std::task::Poll::Ready(None),
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        })
        .await
    })
}

/// Minimal timer future: on first poll it arms a thread that sleeps, then flips
/// the flag and wakes the task. Avoids pulling in an async-timer dependency.
struct Delay {
    fired: std::sync::Arc<std::sync::atomic::AtomicBool>,
    armed: bool,
    dur: std::time::Duration,
}

impl Delay {
    fn new(dur: std::time::Duration) -> Self {
        Self {
            fired: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            armed: false,
            dur,
        }
    }
}

impl std::future::Future for Delay {
    type Output = ();
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        use std::sync::atomic::Ordering;
        if self.fired.load(Ordering::Acquire) {
            return std::task::Poll::Ready(());
        }
        if !self.armed {
            self.armed = true;
            let fired = self.fired.clone();
            let waker = cx.waker().clone();
            let dur = self.dur;
            std::thread::spawn(move || {
                std::thread::sleep(dur);
                fired.store(true, Ordering::Release);
                waker.wake();
            });
        }
        std::task::Poll::Pending
    }
}

const DRAIN_MS: u64 = 200;
const HS_MS: u64 = 300;
const SCAN_MS: u64 = 150;
/// How long to wait for the re-enumeration after the primary single enable
/// packet before falling back to the scan. The success hint says ~5s.
const ENABLE_WAIT_MS: u64 = 5000;

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}
