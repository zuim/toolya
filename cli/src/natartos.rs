//! natartos — the on-device firmware mod framework for the Goggles 3 (modloader,
//! mods/services, with brick-prevention as its first feature). This module is toolya's
//! installer for it; natartos itself lives in its own repo (grom-labs/natartos).
//!
//! toolya is a generic installer: it never hardcodes natartos's on-device layout.
//! Every natartos release ships a `manifest.json`
//! describing what to place where, with a sha256 per artifact. This module fetches the
//! manifest + artifacts (from a GitHub release, or a --local dir), verifies hashes, and
//! applies it over adb.
//!
//! Subcommands:
//!   versions                 list natartos releases
//!   install [--version <t>] [--local <dir>] [--mods <ids>] [--yes]
//!   uninstall [--yes]        restore stock from on-device backups
//!   start                    start mods (after reboot or install)
//!   noverity [--yes]         disable dm-verity for this session (research)
//!   status                   bootcount / kill-switch / log
//!   safe [on|off]            kill-switch toggle

use adb_client::server::ADBServer;
use adb_client::server_device::ADBServerDevice;
use adb_client::usb::ADBUSBDevice;
use adb_client::ADBDeviceExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::process::exit;

use toolya_core::contract::natartos as NC;

const GITHUB_REPO: &str = NC::REPO;
const SUPPORTED_SCHEMA: u32 = NC::MANIFEST_SCHEMA;
const STOCK_DIR: &str = NC::STOCK_DIR;
const MANIFEST_DEVICE_PATH: &str = NC::MANIFEST_DEVICE;
const DEFAULT_LOADER_PATH: &str = NC::DEFAULT_LOADER;
const BOOTCOUNT_PATH: &str = NC::BOOTCOUNT;
const STATUS_PATH: &str = NC::STATUS;
const DISABLED_PATH: &str = NC::DISABLED;
const LOG_PATH: &str = NC::LOG;

// --- manifest ---

#[derive(Deserialize)]
struct Manifest {
    schema: u32,
    #[serde(default)]
    natartos_version: String,
    #[serde(default)]
    device: String,
    #[serde(default)]
    min_toolya: String,
    #[serde(default)]
    init_rc: Option<InitRc>,
    loader: Artifact,
    #[serde(default)]
    files: Vec<Artifact>,
    #[serde(default)]
    mods: Vec<Artifact>,
    #[serde(default)]
    backup: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
}

#[derive(Deserialize)]
struct InitRc {
    device_path: String,
    content: String,
}

#[derive(Deserialize, Clone)]
struct Artifact {
    file: String,
    device_path: String,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    sha256: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    default: bool,
}

fn default_mode() -> String {
    "0644".into()
}

/// A resolved manifest plus the local directory that holds its artifact files.
struct Resolved {
    manifest: Manifest,
    dir: PathBuf,
    /// true when fetched from a release (hashes required); false for --local dev builds.
    from_release: bool,
}

// --- GitHub Releases API ---

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    published_at: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

fn api_get(url: &str) -> Result<String, String> {
    ureq::get(url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "toolya")
        .call()
        .map_err(|e| format!("GitHub API: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("read response: {e}"))
}

/// `None` → latest non-prerelease; `Some(tag)` → that tag.
fn fetch_release(tag: Option<&str>) -> Result<Release, String> {
    let url = match tag {
        Some(t) => format!("https://api.github.com/repos/{GITHUB_REPO}/releases/tags/{t}"),
        None => format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest"),
    };
    let body = api_get(&url)?;
    serde_json::from_str(&body).map_err(|e| format!("parse release: {e}"))
}

fn asset_url<'a>(release: &'a Release, name: &str) -> Result<&'a str, String> {
    release
        .assets
        .iter()
        .find(|a| a.name == name)
        .map(|a| a.browser_download_url.as_str())
        .ok_or_else(|| format!("release is missing asset `{name}`"))
}

fn download_to(url: &str, dest: &Path) -> Result<(), String> {
    let mut reader = ureq::get(url)
        .header("User-Agent", "toolya")
        .call()
        .map_err(|e| format!("download {url}: {e}"))?
        .into_body()
        .into_reader();
    let mut file = File::create(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    io::copy(&mut reader, &mut file).map_err(|e| format!("write {}: {e}", dest.display()))?;
    Ok(())
}

// --- hashing ---

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut f = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Verify a local artifact file against the manifest's sha256.
/// `from_release` builds require a hash; --local dev builds may omit it (warn only).
fn verify(dir: &Path, art: &Artifact, from_release: bool) -> Result<PathBuf, String> {
    let path = dir.join(&art.file);
    if !path.exists() {
        return Err(format!("artifact `{}` not found in {}", art.file, dir.display()));
    }
    if art.sha256.is_empty() {
        if from_release {
            return Err(format!("release manifest has no sha256 for `{}` — refusing", art.file));
        }
        eprintln!("  ⚠ no sha256 for `{}` (local build) — skipping integrity check", art.file);
        return Ok(path);
    }
    let got = sha256_file(&path)?;
    if got.eq_ignore_ascii_case(&art.sha256) {
        Ok(path)
    } else {
        Err(format!(
            "sha256 mismatch for `{}`\n  manifest: {}\n  actual:   {}",
            art.file, art.sha256, got
        ))
    }
}

// --- manifest resolution (download or --local) ---

fn read_manifest(path: &Path) -> Result<Manifest, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let m: Manifest = serde_json::from_str(&text).map_err(|e| format!("parse manifest: {e}"))?;
    if m.schema != SUPPORTED_SCHEMA {
        return Err(format!(
            "manifest schema {} unsupported (toolya understands {SUPPORTED_SCHEMA}) — update toolya",
            m.schema
        ));
    }
    Ok(m)
}

/// Local dir must contain manifest.json; artifact `file`s may sit in the dir itself
/// or in `src/` / `mods/` subdirs (the natartos build layout). We normalize all
/// artifact files into one flat dir so `verify` can find them by basename.
fn resolve_local(dir: &Path) -> Result<Resolved, String> {
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    let manifest = read_manifest(&dir.join("manifest.json"))?;

    let flat = std::env::temp_dir().join("toolya-natartos-local");
    let _ = fs::remove_dir_all(&flat);
    fs::create_dir_all(&flat).map_err(|e| format!("mkdir tmp: {e}"))?;

    let find = |name: &str| -> Option<PathBuf> {
        for cand in [dir.join(name), dir.join("src").join(name), dir.join("mods").join(name), dir.join("deploy").join(name)] {
            if cand.exists() {
                return Some(cand);
            }
        }
        None
    };
    let place = |art: &Artifact| -> Result<(), String> {
        let src = find(&art.file)
            .ok_or_else(|| format!("artifact `{}` not found under {}", art.file, dir.display()))?;
        let dest = flat.join(&art.file);
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p).map_err(|e| format!("mkdir {}: {e}", p.display()))?;
        }
        fs::copy(&src, &dest).map_err(|e| format!("copy {}: {e}", art.file))?;
        Ok(())
    };
    place(&manifest.loader)?;
    for f in &manifest.files {
        place(f)?;
    }
    for m in &manifest.mods {
        place(m)?;
    }
    fs::copy(dir.join("manifest.json"), flat.join("manifest.json"))
        .map_err(|e| format!("copy manifest.json: {e}"))?;
    Ok(Resolved { manifest, dir: flat, from_release: false })
}

fn resolve_release(tag: Option<&str>) -> Result<Resolved, String> {
    println!("Fetching natartos release ({})...", tag.unwrap_or("latest"));
    let release = fetch_release(tag)?;
    println!("  version: {}", release.tag_name);

    let tmp = std::env::temp_dir().join("toolya-natartos");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).map_err(|e| format!("mkdir tmp: {e}"))?;

    // manifest first
    let manifest_url = asset_url(&release, "manifest.json")?;
    let manifest_path = tmp.join("manifest.json");
    download_to(manifest_url, &manifest_path)?;
    let manifest = read_manifest(&manifest_path)?;

    // then each file it names
    let mut need: Vec<&Artifact> = vec![&manifest.loader];
    need.extend(manifest.files.iter());
    need.extend(manifest.mods.iter());
    for art in need {
        println!("  downloading {}...", art.file);
        let url = asset_url(&release, &art.file)?;
        download_to(url, &tmp.join(&art.file))?;
    }
    Ok(Resolved { manifest, dir: tmp, from_release: true })
}

// --- version compare (for min_toolya) ---

/// true if `a` < `b` by dotted numeric components (leading 'v' ignored).
/// Unparseable → false (never blocks).
fn version_lt(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split(['.', '-', '+'])
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (pa, pb) = (parse(a), parse(b));
    for i in 0..pa.len().max(pb.len()) {
        let (x, y) = (pa.get(i).copied().unwrap_or(0), pb.get(i).copied().unwrap_or(0));
        if x != y {
            return x < y;
        }
    }
    false
}

// --- ADB helpers ---

pub enum AdbConn {
    Usb(ADBUSBDevice),
    Server(ADBServerDevice),
}

impl std::ops::Deref for AdbConn {
    type Target = dyn ADBDeviceExt;
    fn deref(&self) -> &(dyn ADBDeviceExt + 'static) {
        match self {
            AdbConn::Usb(d) => d,
            AdbConn::Server(d) => d,
        }
    }
}

impl std::ops::DerefMut for AdbConn {
    fn deref_mut(&mut self) -> &mut (dyn ADBDeviceExt + 'static) {
        match self {
            AdbConn::Usb(d) => d,
            AdbConn::Server(d) => d,
        }
    }
}

/// NATARTOS_ADB_HOST=host:port → TCP to ADB server; otherwise direct USB.
pub fn adb_device() -> Result<AdbConn, String> {
    if let Ok(addr) = std::env::var("NATARTOS_ADB_HOST") {
        let parts: Vec<&str> = addr.splitn(2, ':').collect();
        let ip: Ipv4Addr = parts[0].parse().map_err(|e| format!("bad ADB host IP: {e}"))?;
        let port: u16 = parts.get(1).unwrap_or(&"5037").parse().map_err(|e| format!("bad ADB port: {e}"))?;
        let mut server = ADBServer::new(SocketAddrV4::new(ip, port));
        let dev = server.get_device().map_err(|e| format!("ADB server {addr}: {e}"))?;
        return Ok(AdbConn::Server(dev));
    }
    let dev = ADBUSBDevice::autodetect().map_err(|e| format!("USB device not found: {e}"))?;
    Ok(AdbConn::Usb(dev))
}

fn shell(dev: &mut AdbConn, cmd: &str) -> Result<String, String> {
    let mut stdout = Vec::new();
    let none: Option<&mut dyn std::io::Write> = None;
    dev.shell_command(&cmd, Some(&mut stdout as &mut dyn std::io::Write), none)
        .map_err(|e| format!("shell: {e}"))?;
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

fn push_file(dev: &mut AdbConn, local: &Path, remote: &str) -> Result<(), String> {
    let mut f = File::open(local).map_err(|e| format!("open {}: {e}", local.display()))?;
    dev.push(&mut f, &remote).map_err(|e| format!("push → {remote}: {e}"))?;
    Ok(())
}

fn push_bytes(dev: &mut AdbConn, data: &[u8], remote: &str) -> Result<(), String> {
    let mut cursor = std::io::Cursor::new(data);
    dev.push(&mut cursor, &remote).map_err(|e| format!("push → {remote}: {e}"))?;
    Ok(())
}

fn pull_file(dev: &mut AdbConn, remote: &str) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    dev.pull(&remote, &mut buf).map_err(|e| format!("pull {remote}: {e}"))?;
    Ok(buf)
}

// --- commands ---

pub fn run(args: &[String]) {
    let sub = args.first().map(String::as_str).unwrap_or("");
    match sub {
        "versions" => cmd_versions(),
        "install" => {
            if args.get(1).map(String::as_str) == Some("doom") {
                let wad = flag_value(args, "--wad").map(PathBuf::from);
                cmd_doom(wad.as_deref());
            } else {
                let confirmed = args.iter().any(|a| a == "--yes");
                let local = flag_value(args, "--local").map(PathBuf::from);
                let version = flag_value(args, "--version");
                let mods = flag_value(args, "--mods");
                cmd_install(confirmed, local.as_deref(), version.as_deref(), mods.as_deref());
            }
        }
        "uninstall" => cmd_uninstall(args.iter().any(|a| a == "--yes")),
        "status" => cmd_status(),
        "safe" => cmd_safe(args.get(1).map(String::as_str)),
        "push" => {
            let dir = flag_value(args, "--local").map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("dist"));
            let no_restart = args.iter().any(|a| a == "--no-restart");
            let files: Vec<&str> = args.iter().skip(1)
                .filter(|a| !a.starts_with("--") && !is_flag_value(args, a))
                .map(String::as_str)
                .collect();
            cmd_push(&dir, &files, no_restart);
        }
        "mods" | "mod" => cmd_mods(&args[1..]),
        "logs" | "log" => cmd_log(args.iter().any(|a| a == "-f" || a == "--follow")),
        "snap" => {
            let output = flag_value(args, "-o")
                .or_else(|| flag_value(args, "--output"))
                .or_else(|| args.get(1).filter(|a| !a.starts_with('-')).cloned());
            cmd_snap(output.as_deref());
        }
        "start" | "inject" => cmd_start(),
        "dump" | "diag" | "report" => cmd_dump(),
        "noverity" | "dm-noverity" | "dm_noverity" => {
            let local = flag_value(args, "--local").map(PathBuf::from);
            let confirmed = args.iter().any(|a| a == "--yes");
            cmd_noverity(local.as_deref(), confirmed);
        }
        "stream" => {
            let dir = flag_value(args, "--local").map(PathBuf::from);
            let stop = args.iter().any(|a| a == "--stop");
            cmd_stream(dir.as_deref(), stop);
        }
        _ => {
            eprintln!("toolya natartos — mod framework\n");
            eprintln!("Dev:");
            eprintln!("  push [files...]         push .so to the goggles and restart the UI");
            eprintln!("  start                   start mods (after reboot or install)");
            eprintln!("  snap [-o file.png]      screenshot the goggles' screen (needs hook_overlay)");
            eprintln!("  stream [--local <dir>]  PC Stream: stream the PC screen to the goggles");
            eprintln!("  stream --stop           stop fb_receiver on the goggles");
            eprintln!("  mods                    what is installed, what loads");
            eprintln!("  mods load <name>        load a mod");
            eprintln!("  mods unload <name>      don't load a mod");
            eprintln!("  logs [-f|--follow]      unified log from the goggles");
            eprintln!("  status                  bootcount, kill-switch, version");
            eprintln!("  dump                    diagnostic dump (for bug reports)");
            eprintln!("  noverity [--yes]        disable dm-verity for this session (research tool)");
            eprintln!("");
            eprintln!("Install:");
            eprintln!("  install [--local <dir>] [--version <tag>] [--mods id,...] [--yes]");
            eprintln!("  install doom [--wad <file>]   download Freedoom WAD and push to goggles");
            eprintln!("  uninstall [--yes]");
            eprintln!("  versions                GitHub releases");
            eprintln!("  safe [on|off]           kill-switch");
            exit(1);
        }
    }
}

/// Value following `--flag` in args, if present.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone())
}

/// True when `val` is the value part of a --flag val pair.
fn is_flag_value(args: &[String], val: &str) -> bool {
    args.windows(2).any(|w| w[0].starts_with("--") && w[1] == val)
}

// --- push (fast dev cycle) ---

fn cmd_push(dir: &Path, files: &[&str], no_restart: bool) {
    if !dir.is_dir() {
        eprintln!("dist dir not found: {}", dir.display());
        eprintln!("Run from the natartos repo root, or pass --local <dir>");
        exit(1);
    }

    println!("Connecting...");
    let mut dev = match adb_device() {
        Ok(d) => d,
        Err(e) => { eprintln!("{e}"); exit(1); }
    };

    // Read the on-device manifest for path mapping
    let manifest_text = shell(&mut dev, &format!("cat {MANIFEST_DEVICE_PATH} 2>/dev/null"))
        .unwrap_or_default();
    let manifest: Option<Manifest> = serde_json::from_str(&manifest_text).ok();

    // Decide what to push: named files, or all .so in dir
    let to_push: Vec<(PathBuf, String)> = if files.is_empty() {
        let mut v = Vec::new();
        for entry in fs::read_dir(dir).unwrap_or_else(|e| { eprintln!("read {}: {e}", dir.display()); exit(1); }) {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".so") && name != "inject.sh" { continue; }
            let remote = resolve_device_path(&manifest, &name);
            v.push((entry.path(), remote));
        }
        v
    } else {
        files.iter().map(|f| {
            let local = dir.join(f);
            if !local.exists() {
                eprintln!("not found: {}", local.display());
                exit(1);
            }
            let remote = resolve_device_path(&manifest, f);
            (local, remote)
        }).collect()
    };

    if to_push.is_empty() {
        eprintln!("nothing to push in {}", dir.display());
        exit(1);
    }

    // Push each file, comparing sha256 to skip unchanged
    let mut pushed = 0;
    let mut skipped = 0;
    for (local, remote) in &to_push {
        let local_hash = sha256_file(local).unwrap_or_default();
        let device_hash = shell(&mut dev, &format!("sha256sum {} 2>/dev/null", remote))
            .unwrap_or_default();
        let device_hash = device_hash.split_whitespace().next().unwrap_or("");

        if local_hash == device_hash {
            skipped += 1;
            continue;
        }

        let name = local.file_name().unwrap().to_string_lossy();
        print!("  {} → {}", name, remote);
        if let Err(e) = push_file(&mut dev, local, remote) {
            println!(" FAIL: {e}");
            continue;
        }
        let mode = if name.ends_with(".sh") { "0755" } else { "0644" };
        let _ = shell(&mut dev, &format!("chmod {mode} {remote}"));
        println!(" ✓");
        pushed += 1;
    }

    if pushed == 0 && skipped > 0 {
        println!("all {skipped} files unchanged — nothing pushed");
        if !no_restart {
            println!("(skip restart; use --no-restart to suppress this)");
        }
        return;
    }
    println!("{pushed} pushed, {skipped} unchanged");

    let _ = shell(&mut dev, "sync");

    if no_restart || pushed == 0 {
        return;
    }

    // Restart dji_glasses
    println!("\nRestarting dji_glasses...");
    let _ = shell(&mut dev, "mkdir -p /data/local/tmp");
    let _ = shell(&mut dev, "rm -f /data/local/tmp/natartos_menu.log /data/local/tmp/natartos_inject.log /data/local/tmp/natartos.log");
    let _ = shell(&mut dev, "sync && echo 3 > /proc/sys/vm/drop_caches");
    let inject = manifest.as_ref()
        .and_then(|m| m.files.iter().find(|a| a.file == "inject.sh"))
        .map(|a| a.device_path.as_str())
        .unwrap_or("/vendor/natartos/inject.sh");
    let _ = shell(&mut dev, &format!("setsid sh {inject} </dev/null >/dev/null 2>&1 &"));

    println!("Waiting 8s for startup...");
    std::thread::sleep(std::time::Duration::from_secs(8));

    // Show logs
    print_device_logs(&mut dev);
}

// --- snap (screenshot via overlay mod) ---

const SCREENCAP_DEVICE_PATH: &str = "/data/local/tmp/screencap.bin";
const SCREENCAP_W: u32 = 1920;
const SCREENCAP_H: u32 = 1080;
const SCREENCAP_BPP: u32 = 4; // BGRA
const SCREENCAP_SIZE: usize = (SCREENCAP_W * SCREENCAP_H * SCREENCAP_BPP) as usize;

fn cmd_snap(output: Option<&str>) {
    let mut dev = match adb_device() {
        Ok(d) => d,
        Err(e) => { eprintln!("{e}"); exit(1); }
    };

    // Find dji_glasses PID — overlay mod hooks this process
    let pid_out = shell(&mut dev, "pidof dji_glasses").unwrap_or_default();
    let pid = pid_out.trim();
    if pid.is_empty() {
        eprintln!("dji_glasses not running — is the overlay mod loaded?");
        exit(1);
    }

    // SIGWINCH triggers the DMA-buf screencap in hook_overlay.so
    println!("Triggering capture (SIGWINCH → dji_glasses pid {pid})...");
    let res = shell(&mut dev, &format!("kill -WINCH {pid}")).unwrap_or_default();
    if res.contains("No such process") {
        eprintln!("dji_glasses died or pid stale");
        exit(1);
    }

    // Wait for the overlay mod to write the file
    println!("Waiting for screencap...");
    std::thread::sleep(std::time::Duration::from_secs(2));

    // Verify the file appeared and has the right size
    let stat = shell(&mut dev, &format!(
        "[ -f {SCREENCAP_DEVICE_PATH} ] && wc -c < {SCREENCAP_DEVICE_PATH} || echo missing"
    )).unwrap_or_default();
    let stat = stat.trim();
    if stat == "missing" {
        eprintln!("{SCREENCAP_DEVICE_PATH} not found — is hook_overlay.so loaded?");
        eprintln!("Check: toolya natartos mods");
        exit(1);
    }
    let file_size: usize = stat.parse().unwrap_or(0);
    if file_size != SCREENCAP_SIZE {
        eprintln!(
            "unexpected screencap size: {file_size} bytes (expected {SCREENCAP_SIZE})"
        );
        exit(1);
    }

    // Pull raw BGRA data
    print!("Pulling {SCREENCAP_DEVICE_PATH}...");
    let mut raw = match pull_file(&mut dev, SCREENCAP_DEVICE_PATH) {
        Ok(d) => d,
        Err(e) => { println!(" FAIL"); eprintln!("{e}"); exit(1); }
    };
    println!(" {} bytes", raw.len());

    if raw.len() != SCREENCAP_SIZE {
        eprintln!("pulled size mismatch: {} (expected {SCREENCAP_SIZE})", raw.len());
        exit(1);
    }

    // BGRA → RGBA: swap B and R channels
    for pixel in raw.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }

    // Encode as PNG
    let img = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(
        SCREENCAP_W, SCREENCAP_H, raw,
    ).expect("raw buffer size mismatch (impossible)");

    let out_path = match output {
        Some(p) => p.to_string(),
        None => {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            format!("screencap_{ts}.png")
        }
    };

    if let Err(e) = img.save(&out_path) {
        eprintln!("save {out_path}: {e}");
        exit(1);
    }

    println!("{out_path}");
}

// --- stream (PC screen → goggles via fb_receiver) ---

const FB_RECEIVER_DEVICE_PATH: &str = "/data/local/tmp/fb_receiver";
const FB_PIPE_DEVICE_PATH: &str = "/data/local/tmp/fb_pipe";
const FB_SHARED_DEVICE_PATH: &str = "/data/local/tmp/fb_shared";

fn cmd_stream(local_dir: Option<&Path>, stop: bool) {
    println!("Connecting...");
    let mut dev = match adb_device() {
        Ok(d) => d,
        Err(e) => { eprintln!("{e}"); exit(1); }
    };

    if stop {
        println!("Stopping fb_receiver...");
        let _ = shell(&mut dev, "pkill -f fb_receiver 2>/dev/null");
        let _ = shell(&mut dev, "rm -f /data/local/tmp/fb_override /data/local/tmp/fb_pipe /data/local/tmp/fb_shared");
        println!("Stream stopped");
        return;
    }

    // Check dji_glasses is running (overlay mod hooks this process for inject)
    let pid_out = shell(&mut dev, "pidof dji_glasses").unwrap_or_default();
    if pid_out.trim().is_empty() {
        eprintln!("dji_glasses not running — overlay mod is required for stream inject");
        eprintln!("Install natartos first: toolya natartos install --local <dir> --yes");
        exit(1);
    }

    // Push fb_receiver if local copy available and differs from on-device
    let dir = local_dir.unwrap_or(Path::new("dist"));
    let local_bin = dir.join("fb_receiver");

    if local_bin.exists() {
        let local_hash = sha256_file(&local_bin).unwrap_or_default();
        let device_hash_out = shell(&mut dev, &format!(
            "sha256sum {FB_RECEIVER_DEVICE_PATH} 2>/dev/null"
        )).unwrap_or_default();
        let device_hash = device_hash_out.split_whitespace().next().unwrap_or("");

        if local_hash != device_hash {
            print!("  fb_receiver → {FB_RECEIVER_DEVICE_PATH}");
            if let Err(e) = push_file(&mut dev, &local_bin, FB_RECEIVER_DEVICE_PATH) {
                println!(" FAIL: {e}");
                exit(1);
            }
            let _ = shell(&mut dev, &format!("chmod 755 {FB_RECEIVER_DEVICE_PATH}"));
            println!(" ok");
        } else {
            println!("fb_receiver unchanged, skip push");
        }
    } else {
        // No local binary — check device already has it
        let exists = shell(&mut dev, &format!(
            "[ -x {FB_RECEIVER_DEVICE_PATH} ] && echo y || echo n"
        )).unwrap_or_default();
        if exists.trim() != "y" {
            eprintln!("fb_receiver not found locally ({}) or on device", local_bin.display());
            eprintln!("Build natartos first, or pass --local <dir> pointing to dist/");
            exit(1);
        }
        println!("Using fb_receiver already on device");
    }

    // Kill stale fb_receiver if running
    let old_pid = shell(&mut dev, "pidof fb_receiver").unwrap_or_default();
    if !old_pid.trim().is_empty() {
        println!("Stopping previous fb_receiver (pid {})...", old_pid.trim());
        let _ = shell(&mut dev, "pkill -f fb_receiver 2>/dev/null");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // Create named pipe and start fb_receiver reading from it.
    // fb_receiver blocks at pipe open until a writer (the web UI) connects.
    let _ = shell(&mut dev, &format!("rm -f {FB_PIPE_DEVICE_PATH}"));
    let _ = shell(&mut dev, &format!("mkfifo {FB_PIPE_DEVICE_PATH}"));
    let _ = shell(&mut dev, &format!(
        "nohup sh -c '{FB_RECEIVER_DEVICE_PATH} < {FB_PIPE_DEVICE_PATH}' > /dev/null 2>&1 &"
    ));

    println!("\nPC Stream ready");
    println!("  fb_receiver waiting for frames (pipe: {FB_PIPE_DEVICE_PATH})");
    println!("  Protocol: [u32le jpeg_size][jpeg_bytes] per frame");
    println!("\nOpen toolya.dev -> PC Stream to start streaming");
    println!("\nTo stop:  toolya natartos stream --stop");
}

/// Map a filename to its on-device path using the manifest, with sensible defaults.
fn resolve_device_path(manifest: &Option<Manifest>, name: &str) -> String {
    if let Some(m) = manifest {
        if name == m.loader.file { return m.loader.device_path.clone(); }
        for a in &m.files {
            if a.file == name { return a.device_path.clone(); }
        }
        for a in &m.mods {
            if a.file == name { return a.device_path.clone(); }
        }
    }
    // defaults
    match name {
        "natartos_loader.so" => "/vendor/natartos/natartos_loader.so".into(),
        "inject.sh" => "/vendor/natartos/inject.sh".into(),
        _ => format!("/vendor/natartos/mods/{name}"),
    }
}

fn print_device_logs(dev: &mut AdbConn) {
    let log = shell(dev, &format!("cat {LOG_PATH} 2>/dev/null"))
        .unwrap_or_default();
    if !log.trim().is_empty() {
        println!("{}", log.trim());
    } else {
        println!("(no logs)");
    }
}

// --- log ---

fn cmd_log(follow: bool) {
    let mut dev = match adb_device() {
        Ok(d) => d,
        Err(e) => { eprintln!("{e}"); exit(1); }
    };

    print_device_logs(&mut dev);

    if follow {
        println!("\n--- following (Ctrl-C to stop) ---");
        let mut last_len = 0usize;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let log = shell(&mut dev, &format!("cat {LOG_PATH} 2>/dev/null"))
                .unwrap_or_default();
            if log.len() > last_len {
                let new_part = &log[last_len..];
                for line in new_part.lines() {
                    if !line.is_empty() { println!("{line}"); }
                }
                last_len = log.len();
            }
        }
    }
}

// --- mods (enable/disable) ---

const ENABLE_PREFIX: &str = "/vendor/natartos/enabled/";

fn cmd_mods(args: &[String]) {
    let mut dev = match adb_device() {
        Ok(d) => d,
        Err(e) => { eprintln!("{e}"); exit(1); }
    };

    let action = args.first().map(String::as_str).unwrap_or("");
    match action {
        "load" | "unload" => {
            let name = match args.get(1) {
                Some(n) => n.as_str(),
                None => { eprintln!("usage: toolya natartos mods {action} <name>"); exit(1); }
            };
            let so_name = if name.ends_with(".so") { name.to_string() } else { format!("{name}.so") };
            let so_path = format!("/vendor/natartos/mods/{so_name}");
            let flag = format!("{ENABLE_PREFIX}{so_name}");

            let exists = shell(&mut dev, &format!("[ -f {so_path} ] && echo y || echo n"))
                .unwrap_or_default();
            if exists.trim() != "y" {
                eprintln!("{so_name} is not installed on the goggles");
                exit(1);
            }

            if action == "load" {
                let _ = shell(&mut dev, &format!("mkdir -p {ENABLE_PREFIX} && touch {flag} && sync"));
                let check = shell(&mut dev, &format!("[ -f {flag} ] && echo OK || echo FAIL")).unwrap_or_default();
                if check.trim() == "OK" {
                    println!("{so_name} — enabled");
                } else {
                    eprintln!("{so_name} — flag write FAILED");
                }
            } else {
                let _ = shell(&mut dev, &format!("rm -f {flag}"));
                println!("{so_name} — disabled");
            }
            println!("restart to apply: toolya natartos start");
        }
        _ => {
            let manifest_text = shell(&mut dev, &format!("cat {MANIFEST_DEVICE_PATH} 2>/dev/null"))
                .unwrap_or_default();
            let manifest: Option<Manifest> = serde_json::from_str(&manifest_text).ok();

            let installed = shell(&mut dev, "find /vendor/natartos/mods/ -name '*.so' 2>/dev/null")
                .unwrap_or_default();
            let flags = shell(&mut dev, &format!("find {ENABLE_PREFIX} -name '*.so' 2>/dev/null"))
                .unwrap_or_default();

            // Collect all known mods: from device + from manifest
            let mut seen: Vec<(String, bool, bool, String)> = Vec::new(); // (so, on_device, has_flag, label)

            for line in installed.lines() {
                let so = line.rsplit('/').next().unwrap_or(line).trim();
                if so.is_empty() { continue; }
                let flag = format!("{ENABLE_PREFIX}{so}");
                let has_flag = flags.lines().any(|f| f.trim() == flag);
                let label = manifest.as_ref()
                    .and_then(|m| m.mods.iter().find(|a| a.file == so))
                    .map(|a| if a.name.is_empty() { a.id.clone() } else { a.name.clone() })
                    .unwrap_or_default();
                seen.push((so.to_string(), true, has_flag, label));
            }

            // Mods in manifest but not on device
            if let Some(m) = &manifest {
                for a in &m.mods {
                    if seen.iter().any(|(s, ..)| *s == a.file) { continue; }
                    let label = if a.name.is_empty() { a.id.clone() } else { a.name.clone() };
                    seen.push((a.file.clone(), false, false, label));
                }
            }

            if seen.is_empty() {
                println!("no mods");
                return;
            }

            for (so, on_device, has_flag, label) in &seen {
                let state = match (on_device, has_flag) {
                    (true, true)   => "loading  ",
                    (true, false)  => "installed",
                    (false, _)     => "missing  ",
                };
                let name = if label.is_empty() { so.clone() } else { format!("{label} ({so})") };
                println!("  {state}  {name}");
            }
            println!("\ntoolya natartos mods load/unload <name>");
        }
    }
}

fn cmd_versions() {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases");
    let body = match api_get(&url) {
        Ok(b) => b,
        Err(e) if e.contains("404") => {
            println!("no natartos releases yet ({GITHUB_REPO} not published, or none cut).");
            println!("Build locally and install with:  toolya natartos install --local <dir>");
            return;
        }
        Err(e) => {
            eprintln!("{e}");
            exit(1);
        }
    };
    let releases: Vec<Release> = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("parse releases: {e}");
            exit(1);
        }
    };
    if releases.is_empty() {
        println!("no natartos releases yet (build locally and use `install --local <dir>`)");
        return;
    }
    println!("natartos releases ({GITHUB_REPO}):");
    for r in releases {
        let tag = if r.prerelease { format!("{} (prerelease)", r.tag_name) } else { r.tag_name };
        let date = r.published_at.split('T').next().unwrap_or("");
        println!("  {tag:<24} {date}");
    }
    println!("\ninstall latest:        toolya natartos install");
    println!("install a tag:         toolya natartos install --version <tag>");
}

fn cmd_install(confirmed: bool, local_dir: Option<&Path>, version: Option<&str>, mods_filter: Option<&str>) {
    // 1. resolve the manifest + artifact files (download or local)
    let resolved = match local_dir {
        Some(dir) => {
            println!("Using local build: {}", dir.display());
            resolve_local(dir)
        }
        None => resolve_release(version),
    };
    let resolved = match resolved {
        Ok(r) => r,
        Err(e) => {
            if local_dir.is_none() && e.contains("404") {
                eprintln!("No such natartos release ({GITHUB_REPO} not published, or tag not found).");
                eprintln!("Use --local <dir> to install from a local build.");
            } else {
                eprintln!("Could not prepare natartos: {e}");
                if local_dir.is_none() {
                    eprintln!("Use --local <dir> to install from a local build.");
                }
            }
            exit(1);
        }
    };
    let m = &resolved.manifest;

    // 2. filter mods: --mods id1,id2 or all if omitted
    let selected_mods: Vec<&Artifact> = match mods_filter {
        Some(filter) => {
            let wanted: Vec<&str> = filter.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
            let available_ids: Vec<&str> = m.mods.iter().map(|a| a.id.as_str()).collect();
            for w in &wanted {
                if !available_ids.contains(w) {
                    eprintln!("Unknown mod `{w}`. Available: {}", available_ids.join(", "));
                    exit(1);
                }
            }
            m.mods.iter().filter(|a| wanted.contains(&a.id.as_str())).collect()
        }
        None => m.mods.iter().collect(),
    };

    // 3. verify every artifact's sha256 BEFORE touching the device
    println!("Verifying artifacts...");
    let loader_local = match verify(&resolved.dir, &m.loader, resolved.from_release) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("FATAL: {e}");
            exit(1);
        }
    };
    let mut file_locals = Vec::new();
    for art in &m.files {
        match verify(&resolved.dir, art, resolved.from_release) {
            Ok(p) => file_locals.push((art.clone(), p)),
            Err(e) => {
                eprintln!("FATAL: {e}");
                exit(1);
            }
        }
    }
    let mut mod_locals = Vec::new();
    for art in &selected_mods {
        match verify(&resolved.dir, art, resolved.from_release) {
            Ok(p) => mod_locals.push(((*art).clone(), p)),
            Err(e) => {
                eprintln!("FATAL: {e}");
                exit(1);
            }
        }
    }
    let min_toolya_ok = m.min_toolya.is_empty()
        || !version_lt(env!("CARGO_PKG_VERSION"), &m.min_toolya);
    if !min_toolya_ok {
        eprintln!(
            "This natartos ({}) needs toolya >= {}; you have {}. Update toolya.",
            m.natartos_version,
            m.min_toolya,
            env!("CARGO_PKG_VERSION")
        );
        exit(1);
    }

    // 4. confirmation
    if !confirmed {
        eprintln!("⚠ natartos install writes to /vendor on your goggles.");
        eprintln!("  version: {}", m.natartos_version);
        eprintln!("  device:  {}", m.device);
        eprintln!("  loader:  {} → {}", m.loader.file, m.loader.device_path);
        for a in &m.files {
            eprintln!("  file:    {} → {}", a.file, a.device_path);
        }
        if mods_filter.is_some() {
            let skipped = m.mods.len() - selected_mods.len();
            if skipped > 0 {
                eprintln!("  ({skipped} mod(s) skipped by --mods filter)");
            }
        }
        for a in &selected_mods {
            let label = if a.name.is_empty() { &a.id } else { &a.name };
            let def = if a.default { " [default]" } else { "" };
            let desc = if a.description.is_empty() { String::new() } else { format!(" — {}", a.description) };
            eprintln!("  mod:     {} ({}{}{}) → {}", a.file, label, def, desc, a.device_path);
        }
        eprintln!("\nRe-run with --yes to proceed:  toolya natartos install --yes");
        exit(1);
    }

    // 4. connect + device check
    println!("Connecting to goggles...");
    let mut dev = match adb_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            exit(1);
        }
    };
    if !m.device.is_empty() {
        let model = shell(&mut dev, "getprop ro.product.device; getprop ro.product.model")
            .unwrap_or_default()
            .to_lowercase();
        let want = m.device.to_lowercase();
        let hit = want.split_whitespace().any(|tok| model.contains(tok));
        if !hit {
            eprintln!(
                "Device mismatch: manifest targets `{}`, goggles report `{}`. Aborting.",
                m.device,
                model.trim()
            );
            exit(1);
        }
    }

    // 5. unlock vendor: SELinux must be permissive for /vendor writes
    print!("Checking SELinux... ");
    match crate::selinux::ensure_permissive(&mut dev) {
        Ok(true) => println!("already Permissive"),
        Ok(false) => println!("toggled to Permissive (session-only, resets on reboot)"),
        Err(e) => {
            eprintln!("\nFATAL: {e}");
            eprintln!("Cannot write to /vendor without SELinux permissive.");
            eprintln!("If this is a new kernel build, its selinux_state address may be unknown.");
            exit(1);
        }
    }

    println!("Remounting /vendor rw...");
    let _ = shell(&mut dev, "mount -o remount,rw /vendor");
    let _ = shell(&mut dev, &format!("mkdir -p {STOCK_DIR}"));

    println!("Backing up originals...");
    for path in &m.backup {
        let base = path.rsplit('/').next().unwrap_or(path);
        let _ = shell(
            &mut dev,
            &format!("[ -f {STOCK_DIR}/{base}.orig ] || cp {path} {STOCK_DIR}/{base}.orig"),
        );
    }

    println!("Pushing loader...");
    let _ = shell(&mut dev, &format!("mkdir -p {}", parent_dir(&m.loader.device_path)));
    let _ = shell(&mut dev, &format!("rm -f {}", m.loader.device_path));
    if let Err(e) = push_file(&mut dev, &loader_local, &m.loader.device_path) {
        eprintln!("FATAL: {e}");
        exit(1);
    }
    let _ = shell(&mut dev, &format!("chmod {} {}", m.loader.mode, m.loader.device_path));

    for (art, local) in &file_locals {
        println!("Pushing {}...", art.file);
        let _ = shell(&mut dev, &format!("mkdir -p {}", parent_dir(&art.device_path)));
        if let Err(e) = push_file(&mut dev, local, &art.device_path) {
            eprintln!("FATAL: {e}");
            exit(1);
        }
        let _ = shell(&mut dev, &format!("chmod {} {}", art.mode, art.device_path));
    }

    for (art, local) in &mod_locals {
        println!("Pushing mod {}...", art.file);
        let _ = shell(&mut dev, &format!("mkdir -p {}", parent_dir(&art.device_path)));
        if let Err(e) = push_file(&mut dev, local, &art.device_path) {
            eprintln!("FATAL: {e}");
            exit(1);
        }
        let _ = shell(&mut dev, &format!("chmod {} {}", art.mode, art.device_path));
    }

    for path in &m.remove {
        let _ = shell(&mut dev, &format!("rm -f {path}"));
    }

    if let Some(ref rc) = m.init_rc {
        println!("Writing vendor init...");
        if let Err(e) = push_bytes(&mut dev, rc.content.as_bytes(), &rc.device_path) {
            eprintln!("FATAL: {e}");
            exit(1);
        }
        let _ = shell(&mut dev, &format!("chmod 644 {}", rc.device_path));
    }

    // record the applied manifest on-device so uninstall knows exact paths
    if let Ok(text) = fs::read_to_string(resolved.dir.join("manifest.json")) {
        let _ = push_bytes(&mut dev, text.as_bytes(), MANIFEST_DEVICE_PATH);
    }

    // auto-enable default mods
    let _ = shell(&mut dev, &format!("mkdir -p {ENABLE_PREFIX}"));
    for a in &selected_mods {
        if a.default {
            let flag = format!("{ENABLE_PREFIX}{}", a.file);
            let _ = shell(&mut dev, &format!("touch {flag}"));
            println!("  enabled (default): {}", a.file);
        }
    }
    let _ = shell(&mut dev, "sync");

    // clear stale anti-brick state
    let _ = shell(&mut dev, &format!("rm -f {BOOTCOUNT_PATH} {STATUS_PATH} {DISABLED_PATH}"));
    let _ = shell(&mut dev, "sync");

    // Activate immediately: init won't see the new .rc until reboot,
    // so stop stock dji_glasses and run inject.sh now.
    println!("Activating mods (no reboot needed)...");
    let _ = shell(&mut dev, "sync && echo 3 > /proc/sys/vm/drop_caches");
    let _ = shell(&mut dev, "setprop dji.glasses_service 0");
    let _ = shell(&mut dev, "sleep 2");
    let inject_path = m.files.iter()
        .find(|a| a.file == "inject.sh")
        .map(|a| a.device_path.as_str())
        .unwrap_or("/vendor/natartos/inject.sh");
    let _ = shell(&mut dev, &format!("setsid sh {inject_path} </dev/null >/dev/null 2>&1 &"));

    println!("\n=== natartos {} installed and activated ===", m.natartos_version);
    println!("Mods are running. After a reboot: toolya adb --yes && toolya natartos start");
    println!("Check with:  toolya natartos status");
}

fn parent_dir(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((dir, _)) if !dir.is_empty() => dir.to_string(),
        _ => "/".to_string(),
    }
}

fn cmd_uninstall(confirmed: bool) {
    if !confirmed {
        eprintln!("⚠ natartos uninstall restores stock vendor init on your goggles.");
        eprintln!("  Removes: loader, mods, state files");
        eprintln!("  Restores: backed-up originals from {STOCK_DIR}");
        eprintln!("\nRe-run with --yes to proceed:  toolya natartos uninstall --yes");
        exit(1);
    }

    println!("Connecting to goggles...");
    let mut dev = match adb_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            exit(1);
        }
    };

    print!("Checking SELinux... ");
    match crate::selinux::ensure_permissive(&mut dev) {
        Ok(true) => println!("already Permissive"),
        Ok(false) => println!("toggled to Permissive"),
        Err(e) => {
            eprintln!("\nFATAL: {e}");
            exit(1);
        }
    }

    let _ = shell(&mut dev, "mount -o remount,rw /vendor");

    // read the applied manifest to restore exact files; fall back to convention.
    let applied = shell(&mut dev, &format!("cat {MANIFEST_DEVICE_PATH} 2>/dev/null"))
        .ok()
        .and_then(|t| serde_json::from_str::<Manifest>(&t).ok());

    println!("Restoring originals...");
    let backups: Vec<String> = applied
        .as_ref()
        .map(|m| {
            let mut v = m.backup.clone();
            if let Some(ref rc) = m.init_rc {
                v.push(rc.device_path.clone());
            }
            v
        })
        .unwrap_or_else(|| {
            vec![
                "/vendor/etc/init/dji_glasses_custom.rc".into(),
                "/vendor/etc/selinux/precompiled_sepolicy".into(),
            ]
        });
    for path in &backups {
        let base = path.rsplit('/').next().unwrap_or(path);
        let has = shell(&mut dev, &format!("[ -f {STOCK_DIR}/{base}.orig ] && echo yes || echo no"))
            .unwrap_or_default();
        if has.trim() == "yes" {
            let _ = shell(&mut dev, &format!("cp {STOCK_DIR}/{base}.orig {path}"));
            println!("  restored {base}");
        }
    }

    println!("Removing natartos...");
    let loader_path = applied
        .as_ref()
        .map(|m| m.loader.device_path.clone())
        .unwrap_or_else(|| DEFAULT_LOADER_PATH.to_string());
    let _ = shell(&mut dev, &format!("rm -f {loader_path}"));
    if let Some(m) = &applied {
        for a in &m.mods {
            let _ = shell(&mut dev, &format!("rm -f {}", a.device_path));
        }
    }
    let _ = shell(&mut dev, "rm -rf /vendor/natartos");
    let _ = shell(&mut dev, "rm -f /vendor/etc/init/persist_adb.rc");
    let _ = shell(&mut dev, &format!("rm -f {BOOTCOUNT_PATH} {STATUS_PATH} {DISABLED_PATH}"));
    let _ = shell(&mut dev, &format!("rm -f {LOG_PATH}"));
    let _ = shell(&mut dev, "sync");

    println!("\n=== natartos removed, stock restored ===");
    println!("Reboot to return to fully stock goggles.");
}

fn cmd_status() {
    let mut dev = match adb_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            exit(1);
        }
    };

    let version = shell(&mut dev, &format!(
        "grep -o '\"natartos_version\"[^,]*' {MANIFEST_DEVICE_PATH} 2>/dev/null | cut -d'\"' -f4 || echo '(not installed)'"
    )).unwrap_or_else(|e| format!("error: {e}"));
    let status = shell(&mut dev, &format!("cat {STATUS_PATH} 2>/dev/null || echo '(none)'"))
        .unwrap_or_else(|e| format!("error: {e}"));
    let bootcount = shell(&mut dev, &format!(
        "if [ -f {BOOTCOUNT_PATH} ]; then od -A n -t u4 -N 4 {BOOTCOUNT_PATH} | tr -d ' '; else echo 0; fi"
    )).unwrap_or_else(|e| format!("error: {e}"));
    let disabled = shell(&mut dev, &format!("[ -f {DISABLED_PATH} ] && echo yes || echo no"))
        .unwrap_or_else(|e| format!("error: {e}"));
    let log = shell(&mut dev, &format!("cat {LOG_PATH} 2>/dev/null || echo '(no log)'"))
        .unwrap_or_else(|e| format!("error: {e}"));

    println!("natartos status:");
    println!("  version:   {}", version.trim());
    println!("  state:     {}", status.trim());
    println!("  bootcount: {}", bootcount.trim());
    println!("  disabled:  {}", disabled.trim());
    println!("  log:");
    for line in log.lines() {
        println!("    {line}");
    }
}

fn cmd_start() {
    println!("Connecting...");
    let mut dev = match adb_device() {
        Ok(d) => d,
        Err(e) => { eprintln!("{e}"); exit(1); }
    };

    // Check natartos is installed
    let has_inject = shell(&mut dev, &format!(
        "[ -f /vendor/natartos/inject.sh ] && echo y || echo n"
    )).unwrap_or_default();
    if has_inject.trim() != "y" {
        eprintln!("natartos is not installed (no inject.sh on device)");
        eprintln!("Install first: toolya natartos install");
        exit(1);
    }

    // SELinux must be permissive for the loader
    print!("Checking SELinux... ");
    match crate::selinux::ensure_permissive(&mut dev) {
        Ok(true) => println!("already Permissive"),
        Ok(false) => println!("toggled to Permissive"),
        Err(e) => {
            eprintln!("\nFATAL: {e}");
            exit(1);
        }
    }

    // Read on-device manifest for inject path
    let manifest_text = shell(&mut dev, &format!("cat {MANIFEST_DEVICE_PATH} 2>/dev/null"))
        .unwrap_or_default();
    let manifest: Option<Manifest> = serde_json::from_str(&manifest_text).ok();
    let inject_path = manifest.as_ref()
        .and_then(|m| m.files.iter().find(|a| a.file == "inject.sh"))
        .map(|a| a.device_path.as_str())
        .unwrap_or("/vendor/natartos/inject.sh");

    println!("Starting mods...");
    let _ = shell(&mut dev, "mkdir -p /data/local/tmp");
    let _ = shell(&mut dev, "rm -f /data/local/tmp/natartos_inject.log /data/local/tmp/natartos_inject.pid /data/local/tmp/natartos_menu.log /data/local/tmp/natartos.log");
    let _ = shell(&mut dev, "sync && echo 3 > /proc/sys/vm/drop_caches");

    println!("Stopping dji_glasses (USB may drop)...");
    let _ = shell(&mut dev, "stop dji_glasses; killall dji_glasses 2>/dev/null; sleep 1");
    std::thread::sleep(std::time::Duration::from_secs(3));

    println!("Reconnecting to launch inject.sh...");
    drop(dev);
    std::thread::sleep(std::time::Duration::from_secs(2));
    let mut dev = match adb_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Could not reconnect ({e}) — try again after USB re-plug");
            exit(1);
        }
    };

    println!("Launching inject.sh...");
    let _ = shell(&mut dev, &format!(
        // setsid, not nohup: on zv902 fw 01.00.1600 a nohup'ed child dies with the adb shell
        "setsid sh {inject_path} </dev/null >/dev/null 2>&1 &"
    ));

    println!("Waiting 12s for dji_glasses startup...");
    std::thread::sleep(std::time::Duration::from_secs(12));

    println!("Reconnecting...");
    drop(dev);
    std::thread::sleep(std::time::Duration::from_secs(2));
    let mut dev = match adb_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Could not reconnect ({e}) — check `toolya natartos status` after re-plugging USB");
            return;
        }
    };

    print_device_logs(&mut dev);
    println!("\nMods started. Check: toolya natartos status");
}

const NOVERITY_DEVICE_PATH: &str = "/data/local/tmp/dm_noverity";

fn cmd_noverity(local_dir: Option<&Path>, confirmed: bool) {
    if !confirmed {
        eprintln!("⚠  dm-verity bypass — research tool, USE WITH CAUTION");
        eprintln!();
        eprintln!("This replaces dm-verity with dm-linear on /system for THIS SESSION ONLY.");
        eprintln!("After reboot, dm-verity is restored automatically.");
        eprintln!();
        eprintln!("  ╔══════════════════════════════════════════════════════════════╗");
        eprintln!("  ║  DANGER: Do NOT modify /system and reboot.                  ║");
        eprintln!("  ║  dm-verity mode 2 (restart_on_corruption) will BRICK        ║");
        eprintln!("  ║  the device — no known recovery exists for DJI Goggles 3.   ║");
        eprintln!("  ╚══════════════════════════════════════════════════════════════╝");
        eprintln!();
        eprintln!("Re-run with --yes to proceed:  toolya natartos noverity --yes");
        exit(1);
    }

    // Find the dm_noverity binary: --local dir, or dist/, or natartos/tools/
    let binary = find_noverity_binary(local_dir);
    let binary = match binary {
        Some(p) => p,
        None => {
            eprintln!("dm_noverity binary not found.");
            eprintln!("Expected in: dist/dm_noverity, or pass --local <dir>");
            exit(1);
        }
    };
    println!("Using {}", binary.display());

    println!("Connecting...");
    let mut dev = match adb_device() {
        Ok(d) => d,
        Err(e) => { eprintln!("{e}"); exit(1); }
    };

    // Push binary
    print!("Pushing dm_noverity... ");
    if let Err(e) = push_file(&mut dev, &binary, NOVERITY_DEVICE_PATH) {
        println!("FAIL");
        eprintln!("{e}");
        exit(1);
    }
    println!("ok");
    let _ = shell(&mut dev, &format!("chmod 755 {NOVERITY_DEVICE_PATH}"));

    // Run it
    println!("Running dm_noverity...");
    let output = shell(&mut dev, NOVERITY_DEVICE_PATH).unwrap_or_default();
    for line in output.lines() {
        println!("  {line}");
    }

    if output.contains("type='linear'") {
        println!("\ndm-verity disabled for this session.");
        println!("  mount -o remount,rw /system  — now works (via adb shell)");
        println!();
        println!("  ⚠  Do NOT modify /system and reboot — this will brick the device.");
    } else {
        eprintln!("\ndm_noverity may have failed — check output above.");
        exit(1);
    }
}

fn find_noverity_binary(local_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = local_dir {
        let p = dir.join("dm_noverity");
        if p.exists() { return Some(p); }
    }
    for cand in ["dist/dm_noverity", "tools/dm_noverity"] {
        let p = PathBuf::from(cand);
        if p.exists() { return Some(p); }
    }
    None
}

const FREEDOOM_ZIP_URL: &str = "https://github.com/freedoom/freedoom/releases/download/v0.13.0/freedoom-0.13.0.zip";
const DOOM_WAD_DEVICE: &str = "/data/local/tmp/freedoom1.wad";

fn cmd_doom(wad_path: Option<&Path>) {
    let wad_bytes: Vec<u8>;

    if let Some(p) = wad_path {
        println!("Reading {}…", p.display());
        wad_bytes = fs::read(p).unwrap_or_else(|e| {
            eprintln!("cannot read {}: {e}", p.display());
            exit(1);
        });
    } else {
        println!("Downloading Freedoom Phase 1 (~24 MB)…");
        let tmp = std::env::temp_dir().join("freedoom-0.13.0.zip");
        if let Err(e) = download_to(FREEDOOM_ZIP_URL, &tmp) {
            eprintln!("{e}");
            exit(1);
        }
        println!("Extracting freedoom1.wad…");
        let output = std::process::Command::new("unzip")
            .args(["-p", tmp.to_str().unwrap_or(""), "*/freedoom1.wad"])
            .output();
        match output {
            Ok(o) if o.status.success() && !o.stdout.is_empty() => {
                wad_bytes = o.stdout;
            }
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                eprintln!("unzip failed: {err}");
                eprintln!("Install unzip or pass --wad <path> with a .wad file");
                exit(1);
            }
            Err(e) => {
                eprintln!("cannot run unzip: {e}");
                eprintln!("Install unzip or pass --wad <path> with a .wad file");
                exit(1);
            }
        }
        let _ = fs::remove_file(&tmp);
    }

    println!("WAD: {:.1} MB", wad_bytes.len() as f64 / 1048576.0);

    println!("Connecting…");
    let mut dev = match adb_device() {
        Ok(d) => d,
        Err(e) => { eprintln!("{e}"); exit(1); }
    };

    print!("Pushing freedoom1.wad… ");
    let tmp_local = std::env::temp_dir().join("freedoom1.wad");
    fs::write(&tmp_local, &wad_bytes).unwrap_or_else(|e| {
        eprintln!("cannot write temp file: {e}");
        exit(1);
    });
    if let Err(e) = push_file(&mut dev, &tmp_local, DOOM_WAD_DEVICE) {
        println!("FAIL");
        eprintln!("{e}");
        exit(1);
    }
    let _ = fs::remove_file(&tmp_local);
    println!("ok");

    println!("\nDoom ready. Install the doom mod and run: toolya natartos start");
}

fn cmd_safe(arg: Option<&str>) {
    let mut dev = match adb_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            exit(1);
        }
    };

    match arg {
        Some("on") => {
            let _ = shell(&mut dev, &format!("touch {DISABLED_PATH}"));
            println!("kill-switch ON — mods disabled on next boot");
        }
        Some("off") => {
            let _ = shell(&mut dev, &format!("rm -f {DISABLED_PATH}"));
            let _ = shell(&mut dev, &format!("printf '\\x00\\x00\\x00\\x00' > {BOOTCOUNT_PATH}"));
            println!("kill-switch OFF + bootcount reset — mods enabled on next boot");
        }
        _ => {
            let disabled = shell(&mut dev, &format!("[ -f {DISABLED_PATH} ] && echo yes || echo no"))
                .unwrap_or_default();
            println!("kill-switch: {}", disabled.trim());
            eprintln!("usage: toolya natartos safe <on|off>");
        }
    }
}

fn cmd_dump() {
    let mut dev = match adb_device() {
        Ok(d) => d,
        Err(e) => { eprintln!("{e}"); exit(1); }
    };
    let mut q = |cmd: &str| -> String {
        shell(&mut dev, cmd).unwrap_or_else(|_| "(error)".into()).trim().to_string()
    };

    let sections: Vec<(&str, String)> = vec![
        ("model",    q("getprop ro.product.model")),
        ("device",   q("getprop ro.product.device")),
        ("build",    q("getprop ro.build.display.id")),
        ("kernel",   q("uname -r")),
        ("id",       q("id")),
        ("selinux",  q("getenforce 2>&1")),
        ("usb",      q("getprop sys.usb.config")),
        ("busybox",  q("busybox 2>&1 | head -1")),
        ("df",       q("df /vendor /data 2>&1")),
        ("version",  q(&format!("grep -o '\"natartos_version\"[^,}}]*' {MANIFEST_DEVICE_PATH} 2>/dev/null | cut -d'\"' -f4 || echo '(not installed)'"))),
        ("bootcount", q(&format!(
            "if [ -f {BOOTCOUNT_PATH} ]; then od -A n -t u4 -N 4 {BOOTCOUNT_PATH} | tr -d ' '; else echo 0; fi"
        ))),
        ("disabled", q(&format!("[ -f {DISABLED_PATH} ] && echo yes || echo no"))),
        ("mods",     q("ls -la /vendor/natartos/mods/ 2>/dev/null || echo '(none)'")),
        ("files",    q("ls -la /vendor/natartos/ 2>/dev/null || echo '(not installed)'")),
        ("loaded",   q("cat /proc/$(pidof dji_glasses)/maps 2>/dev/null | grep natartos || echo '(none)'")),
        ("inject",   q("cat /data/local/tmp/natartos_inject.pid 2>/dev/null && echo '' || echo '(no pid)'")),
        ("log",      q("tail -30 /data/local/tmp/natartos_inject.log 2>/dev/null || echo '(no log)'")),
        ("ps",       q("ps -A 2>/dev/null | grep -E 'dji_glasses|inject' || echo '(not running)'")),
    ];

    println!("```");
    println!("toolya natartos dump");
    for (key, val) in &sections {
        if val.contains('\n') {
            println!("[{key}]");
            for line in val.lines() { println!("  {line}"); }
        } else {
            println!("{key}: {val}");
        }
    }
    println!("```");
    eprintln!("\nPaste this into a GitHub issue or Discord for support.");
}
