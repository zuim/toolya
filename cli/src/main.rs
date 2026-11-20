//! toolya CLI. `adb` enables root over USB, `info` parses device props (offline),
//! `natartos` installs/manages the on-device mod framework, `desk` prints the
//! desk-mode profile. Stub commands await hardware wiring per milestone (PLAN.md).

mod desk;
mod info;
mod input;
mod natartos;
mod reboot;
mod selinux;
mod telemetry;
mod usb;
mod usbmode;

use std::process::exit;

// Not yet implemented. snap/stream/push/pull DO work — they live under the
// natartos modloader and are aliased at the top level in main() below.
const STUBS: &[(&str, &str)] = &[
    ("shell", "RE needed · interactive shell is broken on the G3"),
    ("log", "RE needed · adb logcat wrapper"),
    ("power", "RE needed · power off/on (cmd_id unknown)"),
    ("awake", "RE needed · inhibit auto-sleep"),
    ("record", "RE needed · stream to file"),
    ("tx", "RE needed · transmitter off/on (cmd_id unknown)"),
    ("fan", "RE needed · fan quiet/auto with temp guard"),
    ("temp", "RE needed · temperature readout"),
    ("mute", "RE needed · audio mute (cf. natartos mute mod)"),
    ("brightness", "RE needed · display brightness"),
    ("backup", "RE needed · dump a partition/config"),
    ("emu", "RE needed · host-mode dev (research)"),
];


fn usage() {
    eprintln!("toolya {} — https://toolya.dev\n", env!("CARGO_PKG_VERSION"));
    eprintln!("usage: toolya <command> [args]\n");
    eprintln!("  duml-show adb-enable    print raw DUML packet (no hardware)");
    eprintln!("  adb --yes [--no-telemetry] [--no-scan] [--require-fw <substr>]  enable ADB/root over USB");
    eprintln!("      --no-scan       send only the primary cmd_id (0x64), never scan 0x60-0x6F");
    eprintln!("      --require-fw s  abort before sending unless the handshake firmware string contains s");
    eprintln!("  info --from <f> [--usage <f>]  parse getprop/usage dumps (offline)");
    eprintln!("  usb <adb|rndis>         show USB-mode switch plan");
    eprintln!("  reboot [--hard]         show reboot plan (graceful / sysrq)");
    eprintln!("  selinux <status|permissive|enforcing> [--yes]  session SELinux toggle (devmem)");
    eprintln!("  input <key|down|up|raw>  send evdev events (BACK/OK/UP/DOWN/LEFT/RIGHT)");
    eprintln!("  natartos <sub>          mod framework (install/push/status/log/safe/...)");
    eprintln!("  snap|stream|push|pull   on-device capture / transfer (needs natartos)");
    eprintln!("  desk                    show desk-mode step sequence");
    eprintln!("\nnot implemented yet:");
    for (n, d) in STUBS { eprintln!("  {:<22}  {}", n, d); }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    match cmd {
        "" | "-h" | "--help" => { usage(); exit(1); }
        "--version" => println!("toolya {} — https://toolya.dev\nCopyright (c) 2026 grom-labs. AGPL-3.0 + attribution term, see NOTICE.", env!("CARGO_PKG_VERSION")),
        "adb" => {
            eprintln!(
                "⚠ toolya enables ADB/root on your DJI Goggles 3 — an unauthorized modification\n\
                 of your own device. It likely violates DJI's Terms of Service; DJI may suspend/ban\n\
                 your account and/or void your warranty. In practice unlikely (local, undetectable,\n\
                 reverts to stock on removal). Own hardware only; as-is, no warranty."
            );
            if !args.iter().any(|a| a == "--yes") {
                eprintln!("\nRe-run to proceed:  toolya adb --yes");
                exit(1);
            }
            let no_telemetry = args.iter().any(|a| a == "--no-telemetry");
            let scan = !args.iter().any(|a| a == "--no-scan");
            let require_fw = args.windows(2).find(|w| w[0] == "--require-fw").map(|w| w[1].clone());
            // Full enable: handshake + cmd_id scan — same plan the Web client uses.
            let plan = toolya_core::enable_plan();
            let goggles = match usb::Goggles::open(&plan.ports) {
                Ok(g) => g,
                Err(e) => { eprintln!("toolya adb: {e}"); exit(1); }
            };
            let serial = goggles.serial().unwrap_or_default();
            if goggles.adb_mode() {
                println!("ADB is already enabled — connect directly: `adb devices`");
                return;
            }
            match goggles.run_enable(&plan, scan, require_fw.as_deref(), |msg| eprintln!("  {msg}")) {
                Ok(()) => {
                    println!("adb enable sent — wait ~5s, then `adb devices`");
                    telemetry::after_root(&serial, no_telemetry);
                }
                Err(e) => { eprintln!("toolya adb: {e}"); exit(1); }
            }
        }
        "info" => {
            // Offline: `info --from <getprop> [--usage <glasses_statistics.json>]`.
            // Live read over adb is M1/hardware.
            let mut props_file = None;
            let mut usage_file = None;
            let mut it = args.iter().skip(1);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--from" => props_file = it.next(),
                    "--usage" => usage_file = it.next(),
                    _ => {}
                }
            }
            if props_file.is_none() && usage_file.is_none() {
                eprintln!("🔴 toolya info: live read needs adb (M1/hw). Offline: `info --from <getprop> [--usage /data/glasses_statistics.json]`");
                exit(2);
            }
            let read = |f: &str| std::fs::read_to_string(f).unwrap_or_else(|e| { eprintln!("info: read {f}: {e}"); exit(1); });
            let mut i = match props_file {
                Some(f) => info::Info::from_props(&info::parse_getprop(&read(f))),
                None => info::Info::default(),
            };
            if let Some(f) = usage_file {
                let (p, m) = info::parse_usage(&read(f));
                i.power_ons = p;
                i.motor_starts = m;
            }
            i.print();
        }
        "reboot" => {
            // Graceful by default; --hard uses sysrq (sync-first). Scaffold only.
            let hard = args.iter().any(|a| a == "--hard");
            let steps = if hard { reboot::sysrq() } else { reboot::graceful() };
            println!("reboot ({}) — device (adb root):", if hard { "sysrq --hard" } else { "graceful" });
            for c in &steps { println!("  {c}"); }
            if hard {
                println!("  ⚠ sysrq 'b' is an immediate reboot — the sync (echo s) runs first to protect /data.");
                println!("  ⚠ /proc/sysrq-trigger may be SELinux-denied (was, from su:s0).");
            }
            println!("  note: adb does not survive reboot — re-enable after (toolya adb).");
            eprintln!("🔴 not executed — mutates the device, drops the connection; run only on explicit go-ahead.");
            exit(2);
        }
        "usb" => {
            // Toggle USB mode: adb (acm,adb) ↔ rndis (keeps adb). Live switch needs
            // adb (root setprop) and re-enumerates USB — scaffold prints the plan.
            match args.get(1).map(String::as_str) {
                Some(m @ ("adb" | "rndis")) => {
                    let cfg = if m == "rndis" { usbmode::CFG_RNDIS } else { usbmode::CFG_ADB };
                    println!("usb {m} → sys.usb.config = {cfg}");
                    println!("device (adb root):");
                    for c in usbmode::setprop_sequence(cfg) { println!("  {c}"); }
                    if m == "rndis" {
                        println!("host (usb0):");
                        for c in usbmode::host_rndis_up() { println!("  {c}"); }
                        println!("  device RNDIS IP: {}", usbmode::RNDIS_DEVICE_IP);
                    }
                    eprintln!("🔴 not applied — live switch needs adb (M1/hw) + re-enumerates USB.");
                    exit(2);
                }
                _ => { eprintln!("usage: toolya usb <adb|rndis>  (uvc unsupported on ZV902)"); exit(1); }
            }
        }
        "selinux" => selinux::run(&args[1..]),
        "input" => input::run(&args[1..]),
        "natartos" => natartos::run(&args[1..]),
        // These are implemented inside the natartos modloader; expose them at the
        // top level too (they need natartos installed on the goggles).
        "snap" | "stream" | "push" | "pull" => natartos::run(&args),
        "desk" => desk::run(),
        "duml-show" => match args.get(1).map(String::as_str) {
            Some("adb-enable") => {
                let p = toolya_core::enable_plan();
                println!("handshake ({} packets, {} init + {} probe):", p.handshake.len(), p.init_count, p.handshake.len() - p.init_count);
                for (i, pkt) in p.handshake.iter().enumerate() {
                    let tag = if i < p.init_count { "init" } else { "probe" };
                    println!("  [{tag:>5} {i:>2}] {}", pkt.iter().map(|b| format!("{b:02x}")).collect::<String>());
                }
                println!("primary enable (cmd_id 0x{:02x}, single packet, ~5s wait):", p.enable[10]);
                println!("  [0x{:02x}] {}", p.enable[10], p.enable.iter().map(|b| format!("{b:02x}")).collect::<String>());
                println!("fallback candidates ({}, scan until USB disconnect):", p.candidates.len());
                for (i, pkt) in p.candidates.iter().enumerate() {
                    println!("  [0x{:02x}] {}", 0x64u8.wrapping_add(i as u8).min(0x6F), pkt.iter().map(|b| format!("{b:02x}")).collect::<String>());
                }
            }
            other => { eprintln!("unknown action: {}", other.unwrap_or("<none>")); exit(1); }
        },
        name => match STUBS.iter().find(|(n, _)| *n == name) {
            Some((_, d)) => { eprintln!("🔴 toolya {}: not implemented — {}", name, d); exit(2); }
            None => { eprintln!("unknown command: {}\n", name); usage(); exit(1); }
        },
    }
}
