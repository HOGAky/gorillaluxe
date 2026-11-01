//! Real Android runtime bootstrapper + headless Google emulator
//! engine bridge (spec PHASE 4 task 1).
//!
//! - Resolves official Google packages (emulator, platform-tools,
//!   Android 13 x86_64 system image) from `dl.google.com/android/repository`
//!   manifests — system images under `sys-img/<flavor>/sys-img2-1.xml`.
//! - Downloads natively via `downloader` (ureq + zip, `.part` resume)
//!   into `%LOCALAPPDATA%\GorillaLuxe\runtime\` — no PowerShell.
//! - Boots Google's open-source headless emulator with
//!   `-accel whpx -no-window` and `ANDROID_SDK_ROOT`/`ANDROID_AVD_HOME`
//!   pointing at the runtime dir — Google's Qt window never appears.
//! - Streams the guest framebuffer via raw `exec-out screencap`
//!   (12-byte header + uncompressed RGBA — no PNG/GDI+ cost) into the
//!   DX12/CAS compositor; falls back to PNG decode if the guest only
//!   sends `-p` frames.
//! - Persistent `adb shell` stdin pipe for <5 ms input forwarding
//!   (`input tap/swipe/keyevent`, `setprop`), plus `adb install` for
//!   dropped APKs.
//! - Everything noteworthy is appended to `runtime\gorillaluxe.log`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::Instant;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
// Explicit imports only — a `GdiPlus::*` glob would shadow
// `Result::Ok` with `Status::Ok`.
use windows::Win32::Graphics::GdiPlus::{
    BitmapData, GdipBitmapLockBits, GdipBitmapUnlockBits, GdipCreateBitmapFromStream,
    GdipDisposeImage, GdipGetImageHeight, GdipGetImageWidth, GdiplusShutdown, GdiplusStartup,
    GdiplusStartupInput, GpBitmap, GpImage, ImageLockModeRead, Rect,
};

use super::device_profile::DeviceProfile;
use super::downloader::{self, ArtifactTarget, DlEvent};
use super::manifest;
use crate::config::data_dir;

const EMULATOR_REL: &str = r"emulator\emulator.exe";
const QEMU_REL: &str = r"emulator\qemu\windows-x86_64\qemu-system-x86_64-headless.exe";
const ADB_REL: &str = r"platform-tools\adb.exe";

// ---------------------------------------------------------------------------
// Logging — delegates to `system::logger` (leveled + rotated).
// ---------------------------------------------------------------------------

pub fn log_path() -> PathBuf {
    super::logger::log_path()
}

/// Append one timestamped INFO line (kept for existing call sites).
pub fn log_line(msg: &str) {
    super::logger::info(msg);
}

// ---------------------------------------------------------------------------
// Install state + validation
// ---------------------------------------------------------------------------

/// Root of the on-disk runtime.
pub fn runtime_dir() -> PathBuf {
    data_dir().join("runtime")
}

pub fn emulator_exe() -> PathBuf {
    runtime_dir().join(EMULATOR_REL)
}

pub fn adb_exe() -> PathBuf {
    runtime_dir().join(ADB_REL)
}

/// First extracted system-image directory (contains `system.img`).
/// Layout: `runtime\system-images\android-33\<flavor>\x86_64\`.
pub fn find_sysimg(root: &Path) -> Option<PathBuf> {
    let si = root.join("system-images");
    for lvl in std::fs::read_dir(&si).ok()? {
        let lvl = lvl.ok()?.path();
        for flavor in std::fs::read_dir(&lvl).ok()? {
            for abi in std::fs::read_dir(flavor.ok()?.path()).ok()? {
                let dir = abi.ok()?.path();
                if dir.join("system.img").is_file() && dir.join("ramdisk.img").is_file() {
                    return Some(dir);
                }
            }
        }
    }
    None
}

/// Runtime files that MUST exist for the emulator to boot.
/// (`userdata.img` is excluded — absent from some sys-img zips; the
/// emulator creates it on first boot.)
pub fn missing_required(root: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    for rel in [EMULATOR_REL, QEMU_REL, ADB_REL] {
        if !root.join(rel).is_file() {
            missing.push(rel.replace('/', "\\"));
        }
    }
    match find_sysimg(root) {
        Some(si) => {
            // `userdata.img` is intentionally absent from this list —
            // some Google sys-img zips omit it and emulator.exe will
            // create it on first boot.
            for f in ["system.img", "vendor.img", "ramdisk.img"] {
                if !si.join(f).is_file() {
                    missing.push(format!(r"system-images\...\x86_64\{f}"));
                }
            }
            // Kernel is required — either ranchu naming.
            if !si.join("kernel-ranchu").is_file() && !si.join("kernel-ranchu-64").is_file() {
                missing.push(r"system-images\...\x86_64\kernel-ranchu*".into());
            }
        }
        None => missing.push(r"system-images\android-33\*\x86_64\system.img".into()),
    }
    missing
}

/// What's installed under `runtime\`.
#[derive(Debug)]
pub enum InstallState {
    Missing,
    Partial(String),
    Ready {
        emulator: PathBuf,
        adb: PathBuf,
        sysimg: PathBuf,
    },
}

pub fn install_state() -> InstallState {
    let root = runtime_dir();
    let missing = missing_required(&root);
    if !missing.is_empty() {
        return if !root.exists() {
            InstallState::Missing
        } else {
            InstallState::Partial(missing.join(", "))
        };
    }
    InstallState::Ready {
        emulator: emulator_exe(),
        adb: adb_exe(),
        sysimg: find_sysimg(&root).unwrap(),
    }
}

// ---------------------------------------------------------------------------
// Manifest resolution
// ---------------------------------------------------------------------------

/// Base URL artifacts of manifest `rel` resolve against — the
/// manifest's own directory. sys-img `<url>` values are bare filenames
/// (`x86_64-33_r09.zip`), NOT repository-root-relative — joining them
/// with `REPO_BASE` produces 404s.
fn manifest_base(rel: &str) -> String {
    match rel.rfind('/') {
        Some(i) => format!("{}{}", manifest::REPO_BASE, &rel[..i + 1]),
        None => manifest::REPO_BASE.to_string(),
    }
}

/// Fetch + parse a repository manifest. ureq is tried first, then
/// `curl.exe` (built into Windows 10/11) — errors always carry the
/// exact URL and transport detail.
pub fn fetch_manifest(rel: &str) -> Result<Vec<manifest::Artifact>, String> {
    let url = format!("{}{rel}", manifest::REPO_BASE);
    let bytes = downloader::http_get(&url)?;
    let body = String::from_utf8_lossy(&bytes);
    if !body.contains("<remotePackage") {
        return Err(format!(
            "{url}: not an SDK manifest ({} bytes)",
            bytes.len()
        ));
    }
    let arts = manifest::parse_manifest(&body, &manifest_base(rel));
    if arts.is_empty() {
        return Err(format!("{url}: manifest parsed but had no archives"));
    }
    Ok(arts)
}

/// Resolve the Android 13 x86_64 system image: walks the flavor
/// manifests in preference order (playstore → google_apis → aosp),
/// collecting per-flavor errors so nothing is silently swallowed.
pub fn fetch_system_image() -> Result<manifest::Artifact, String> {
    let mut errs = Vec::new();
    for (_flavor, rel) in manifest::SYSIMG_MANIFESTS {
        match fetch_manifest(rel) {
            Ok(arts) => match manifest::choose_system_image(&arts) {
                Some(img) => return Ok(img),
                None => errs.push(format!(
                    "{rel}: no android-{TARGET}x86_64 image",
                    TARGET = manifest::TARGET_API
                )),
            },
            Err(e) => errs.push(e),
        }
    }
    Err(errs.join(" | "))
}

fn resolve_live() -> Result<[manifest::Artifact; 3], String> {
    let repo = fetch_manifest(manifest::REPO_MANIFEST)?;
    let tools = manifest::choose_platform_tools(&repo)
        .ok_or_else(|| "repository2-1.xml: no windows platform-tools archive".to_string())?;
    let emu = manifest::choose_emulator(&repo)
        .ok_or_else(|| "repository2-1.xml: no windows emulator archive".to_string())?;
    let img = fetch_system_image()?;
    Ok([tools, emu, img])
}

/// Resolve the three download URLs needed for the runtime.
/// `Ok` = `(platform-tools, emulator, system-image)` in download order.
/// Live resolution is preferred; on ANY failure the compile-time
/// `KNOWN_GOOD_PACKAGES` (CI-verified URLs) take over so the download
/// always has concrete targets.
pub fn resolve_runtime_urls() -> Result<[manifest::Artifact; 3], String> {
    match resolve_live() {
        Ok(t) => Ok(t),
        Err(e) => {
            log_line(&format!(
                "live manifest resolve failed ({e}); using known-good package URLs"
            ));
            Ok(manifest::known_good_packages())
        }
    }
}

// ---------------------------------------------------------------------------
// Native download job — channel events consumed by the Zone D UI
// ---------------------------------------------------------------------------

pub struct DownloadJob {
    rx: mpsc::Receiver<DlEvent>,
}

fn target_dest(root: &Path, a: &manifest::Artifact) -> PathBuf {
    // system-images;android-33;google_apis_playstore;x86_64
    //   → system-images\android-33\google_apis_playstore\
    // (the zip itself contributes the final `x86_64` component)
    if let Some(rest) = a.package_path.strip_prefix("system-images;") {
        let mut seg = rest.split(';');
        let api = seg.next().unwrap_or("android-33");
        let flavor = seg.next().unwrap_or("default");
        return root.join("system-images").join(api).join(flavor);
    }
    root.to_path_buf()
}

impl DownloadJob {
    /// Resolve dest routing, then spawn `downloader::start` on a
    /// background thread.
    pub fn start(urls: &[manifest::Artifact; 3]) -> Result<Self, String> {
        let root = runtime_dir();
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        log_line(&format!(
            "download start: {} + {} + {}",
            urls[0].url, urls[1].url, urls[2].url
        ));
        let labels = [
            "Android Platform-Tools (adb.exe)",
            "Google Emulator Engine (emulator.exe, gfxstream/ANGLE)",
            "Android 13 x86_64 System Image (system.img)",
        ];
        let mut targets = Vec::new();
        for (i, a) in urls.iter().enumerate() {
            let leaf = a.url.rsplit('/').next().unwrap_or(&a.url);
            let dest = target_dest(&root, a);
            // Per-component "installed" marker — retries skip a step
            // whose payload is already extracted and valid on disk.
            let marker = if a.package_path == "platform-tools" {
                Some(root.join(ADB_REL))
            } else if a.package_path == "emulator" {
                Some(root.join(EMULATOR_REL))
            } else {
                // system-images;android-33;<flavor>;x86_64 → zip's top
                // folder is `x86_64/` inside `dest`.
                Some(dest.join("x86_64").join("system.img"))
            };
            targets.push(ArtifactTarget {
                step: i + 1,
                label: labels[i].to_string(),
                url: a.absolute_url.clone(),
                zip: root.join(leaf),
                dest,
                sha256: a.sha256.clone(),
                sha1: a.sha1.clone(),
                marker,
                size: a.size,
            });
        }
        Ok(DownloadJob {
            rx: downloader::start(targets),
        })
    }

    /// Drain pending events (call every frame).
    pub fn poll(&self) -> Vec<DlEvent> {
        let mut v = Vec::new();
        while let Ok(e) = self.rx.try_recv() {
            v.push(e);
        }
        v
    }
}

// ---------------------------------------------------------------------------
// AVD + emulator launch
// ---------------------------------------------------------------------------

/// Running headless emulator process.
pub struct EmulatorProc {
    pub child: Child,
    pub gpu: String,
    pub launched_at: Instant,
    /// Whether a fallback relaunch has already been attempted.
    pub retried: bool,
    /// Windows Job Object handle (raw) — `KILL_ON_JOB_CLOSE` guarantees
    /// kernel-level cleanup of `emulator.exe` + `qemu` grandchildren
    /// even if `gorillaluxe.exe` dies abruptly. 0 when unsupported.
    job: isize,
}

impl EmulatorProc {
    /// Wait for the child to exit on its own, up to `ms`.
    fn wait_exit(&mut self, ms: u64) -> bool {
        let deadline = Instant::now() + std::time::Duration::from_millis(ms);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Graceful-first teardown: a hard `kill()` here would drop qemu
    /// mid-write and the QuickBoot snapshot is LOST — every launch
    /// then cold-boots and replays the multi-minute GMS first-boot
    /// churn (the SystemUI-ANR storm this build mitigates). Order:
    ///  1) `avd snapshot save default_boot` (console :5554) — writes
    ///     the full QuickBoot state (framebuffer, gfxstream textures,
    ///     and qcow2 device overlays);
    ///  2) `kill` via console :5554 (NOT `adb shell reboot -p` — guest
    ///     poweroff triggers `Snapshotter` to log `Deleting snapshot
    ///     default_boot` and forces a cold boot on the next launch!);
    ///  3) monitor `quit` — clean qemu exit fallback;
    ///  4) kill + Job close — last resort, never left orphaned.
    pub fn shutdown(&mut self) {
        log_line("emulator shutdown: saving QuickBoot snapshot");
        match emu_console_cmd("avd snapshot save default_boot", 60_000) {
            Ok(r) => {
                if r.contains("KO") {
                    log_line(&format!("snapshot save failed: {}", r.trim()));
                } else {
                    log_line("QuickBoot snapshot saved");
                }
            }
            Err(e) => log_line(&format!("snapshot save: {e}")),
        }
        log_line("emulator shutdown: requesting clean console exit");
        let _ = emu_console_cmd("kill", 10_000);
        if self.wait_exit(35_000) {
            log_line("emulator exited cleanly (console kill)");
        } else {
            log_line("emulator still up after console kill — monitor quit");
            if let Ok(mut m) = QemuMonitor::connect() {
                let _ = m.quit();
            }
            if self.wait_exit(25_000) {
                log_line("emulator exited cleanly (monitor quit)");
            } else {
                log_line("emulator did not exit — hard kill");
            }
        }
        let _ = self.child.kill();
        // Only clear when the stored pid is THIS child's — a GPU-chain
        // retry may already have published its successor's pid.
        if OWNED_EMU_PID.load(Ordering::Acquire) == self.child.id() {
            OWNED_EMU_PID.store(0, Ordering::Release);
        }
        if self.job != 0 {
            unsafe {
                let _ = CloseHandle(HANDLE(self.job as _));
            }
            self.job = 0;
        }
    }
}

impl Drop for EmulatorProc {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Bind `child` to a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
/// — kernel-guaranteed orphan-free teardown.
fn attach_kill_job(child: &Child) -> isize {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    unsafe {
        let Ok(job) = CreateJobObjectW(None, PCWSTR::null()) else {
            log_line("warning: CreateJobObject failed — emulator not job-bound");
            return 0;
        };
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .is_err()
        {
            let _ = CloseHandle(job);
            return 0;
        }
        if AssignProcessToJobObject(job, HANDLE(child.as_raw_handle() as _)).is_err() {
            let _ = CloseHandle(job);
            return 0;
        }
        job.0 as isize
    }
}

fn avd_home() -> PathBuf {
    runtime_dir().join("avd")
}

/// Create/refresh the `GorillaLuxe` AVD inside the runtime dir.
/// Paths with spaces are safe — config values, not CLI args.
/// `gpu_mode` lands in `hw.gpu.mode` so config and CLI agree; `spec`
/// scales RAM/cores/LCD to the host (weak laptops → 1 GiB/2c/540p).
pub fn write_avd(
    root: &Path,
    sysimg: &Path,
    _profile: &DeviceProfile,
    gpu_mode: &str,
    spec: &crate::config::SpecProfile,
) -> Result<PathBuf, String> {
    let home = root.join("avd");
    let avd = home.join("GorillaLuxe.avd");
    std::fs::create_dir_all(&avd).map_err(|e| e.to_string())?;
    // AVD migration: a `GorillaLuxe.avd` left by an older build can
    // carry a portrait `1440x3088` config + stale `userdata-qemu.img` —
    // qemu would boot that mismatch (wrong LCD, ghost apps, possible
    // bootloop) instead of our landscape panel. The `.glx_avd_v4`
    // marker marks a config written by THIS generation; when absent,
    // kill the emulator still holding the AVD and purge every mutable
    // image/lock/snapshot so the fresh config.ini below is the only
    // source of truth.
    let marker = avd.join(".glx_avd_v4");
    if !marker.exists() && avd.join("config.ini").exists() {
        for (pid, _) in our_avd_live_pids() {
            let _ = super::silent_cmd("taskkill.exe")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .output();
        }
        if !our_avd_live_pids().is_empty() {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        for e in std::fs::read_dir(&avd).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let is_mutable = name.ends_with(".img")
                || name.ends_with(".qcow2")
                || name.ends_with(".lock")
                || name == "snapshots";
            if !is_mutable {
                continue;
            }
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let _ = std::fs::remove_dir_all(e.path());
            } else {
                let _ = std::fs::remove_file(e.path());
            }
        }
        log_line("AVD migrated to .glx_avd_v4 (stale images/locks purged)");
    }
    // emulator.exe aborts with "PANIC: Cannot find AVD system path.
    // Please define ANDROID_SDK_ROOT" unless a `platforms\android-33\`
    // dir exists inside the SDK root — ship a minimal build.prop.
    let platform = root.join("platforms").join("android-33");
    std::fs::create_dir_all(&platform).map_err(|e| e.to_string())?;
    std::fs::write(
        platform.join("build.prop"),
        "# GorillaLuxe minimal platform marker\nro.build.version.sdk=33\nro.build.version.release=13\n",
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(
        home.join("GorillaLuxe.ini"),
        format!(
            "avd.ini.encoding=UTF-8\npath={}\npath.rel=avd/GorillaLuxe.avd\ntarget=android-{}\n",
            avd.display(),
            manifest::TARGET_API
        ),
    )
    .map_err(|e| e.to_string())?;
    let config = format!(
        concat!(
            "avd.ini.encoding=UTF-8\n",
            "hw.cpu.arch=x86_64\n",
            "hw.cpu.ncore={cores}\n",
            "hw.ramSize={ram}\n",
            // Landscape gaming panel — resolution/density scale to the
            // host spec profile (960x540 on ≤4 GiB laptops).
            "hw.lcd.width={lcd_w}\n",
            "hw.lcd.height={lcd_h}\n",
            "hw.lcd.density={lcd_dpi}\n",
            "hw.initialOrientation=landscape\n",
            "hw.keyboard=yes\n",
            "hw.mainKeys=no\n",
            "hw.sensors.proximity=yes\n",
            "hw.camera.back=virtualscene\n",
            "hw.gpu.enabled=yes\n",
            "hw.gpu.mode={gpu_mode}\n",
            "vm.heapSize=512\n",
            "image.sysdir.1={sysdir}\n",
            "tag.display=Google APIs PlayStore\n",
            "tag.id=google_apis_playstore\n",
            "abi.type=x86_64\n",
            "test.delayAdbTillBootComplete=0\n",
            "PlayStore.enabled=no\n",
            "showDeviceFrame=no\n",
            "hw.device.manufacturer=Generic\n",
            "hw.device.name=GorillaLuxe\n",
            "disk.dataPartition.size=8G\n",
            // QuickBoot: after the first cold boot the engine snapshots
            // guest RAM — subsequent launches restore in ~2-3 s.
            "fastboot.forceColdBoot=no\n",
            "fastboot.forceFastBoot=yes\n"
        ),
        cores = spec.guest_cores,
        ram = spec.guest_mem_mb,
        lcd_w = spec.lcd_w,
        lcd_h = spec.lcd_h,
        lcd_dpi = spec.lcd_dpi,
        gpu_mode = gpu_mode,
        sysdir = sysimg.to_string_lossy().replace('\\', "/"),
    );
    std::fs::write(avd.join("config.ini"), config).map_err(|e| e.to_string())?;
    let _ = std::fs::write(avd.join("quickBootChoice.ini"), b"saveOnExit = true\n");
    let _ = std::fs::write(avd.join(".glx_avd_v4"), b"landscape spec-profile v4\n");
    // One-time snapshot reset when switching to `-feature -Vulkan`:
    // a snapshot saved while `goldfish_vulkan` was enabled carries a
    // Vulkan device state that crashes Chrome/WebView on Intel iGPUs.
    // Preserves `userdata-qemu.img.qcow2` (installed apps/accounts).
    let vk_marker = avd.join(".glx_no_vulkan_v1");
    if !vk_marker.exists() {
        let _ = std::fs::remove_dir_all(avd.join("snapshots"));
        let _ = std::fs::write(
            &vk_marker,
            b"vulkan disabled for chrome/webview stability\n",
        );
    }
    Ok(avd)
}

/// Keys that MUST appear in a valid AVD `config.ini` (selftest).
pub fn validate_avd(avd_dir: &Path) -> Result<(), String> {
    let ini = avd_dir
        .parent()
        .map(|p| p.join("GorillaLuxe.ini"))
        .filter(|p| p.is_file())
        .ok_or("GorillaLuxe.ini missing")?;
    let cfg = std::fs::read_to_string(avd_dir.join("config.ini"))
        .map_err(|_| "config.ini missing".to_string())?;
    let _ = ini;
    let required = [
        "image.sysdir.1=",
        "hw.cpu.arch=x86_64",
        "hw.cpu.ncore=",
        "hw.ramSize=",
        "hw.gpu.enabled=yes",
        "hw.gpu.mode=",
        "hw.lcd.density=",
    ];
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|k| !cfg.contains(k))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("config.ini missing keys: {}", missing.join(", ")))
    }
}

/// Launch arguments — `-no-window` keeps Google's Qt window from ever
/// appearing while gfxstream/ANGLE renders offscreen. `-no-metrics`
/// silences the usage-stats banner; the boot animation is intentionally
/// kept (screencap streams it live so boot never looks frozen).
/// NEVER pass `-prop ro.product.*` here — overriding
/// hardware/board/device props makes Android 13 `init` fail to locate
/// `init.ranchu.rc` and the guest bootloops (sys.boot_completed never 1).
fn emulator_args(gpu: &str, accel: &str, spec: &crate::config::SpecProfile) -> Vec<String> {
    // emulator >=34 only accepts `-accel on|off|auto` — the legacy
    // "whpx" name is rejected outright ("Invalid '-accel whpx'
    // parameter, valid values are: on off auto") and the engine exits
    // in <1 s. `on` = require the host hypervisor (WHPX) and fail fast
    // when it is absent — exactly the semantics our "whpx" pass needs.
    let accel_mode = if accel == "whpx" { "on" } else { accel };
    // NOTE: emulator 37.3.6 has NO hardware-ANGLE GLES mode — the
    // ForceANGLE feature flag hijacks mode selection entirely and
    // resolves to `swangle` (ANGLE→SwiftShader, still software).
    // Verified live: `-gpu host -feature ForceANGLE` logged
    // `gpu_mode_requested: swangle`. Hardware accel = `-gpu host`
    // (vendor WGL) only; everything else is CPU rendering.
    vec![
        "-avd".into(),
        "GorillaLuxe".into(),
        // `-qt-hide-window` (Qt starts but the window is hidden) —
        // NOT `-no-window`: with -no-window gfxstream never posts
        // frames to the QEMU scanout (verified: `screendump` dumps an
        // all-black PPM and gRPC `getScreenshot` long-polls forever
        // with FAILED_PRECONDITION "guest has not posted new frame").
        // The hidden window keeps the display pipeline alive so the
        // host-side gRPC capture works; our DX12 window is still the
        // only thing the user sees.
        "-qt-hide-window".into(),
        // QuickBoot snapshots stay ON — never `-no-snapshot`: the
        // engine restores guest RAM in ~2-3 s on every launch after
        // the first cold boot.
        "-no-metrics".into(),
        // Local gRPC control port (screenshot streaming / touch API).
        "-grpc".into(),
        "8554".into(),
        "-accel".into(),
        accel_mode.to_string(),
        "-gpu".into(),
        gpu.to_string(),
        "-memory".into(),
        spec.guest_mem_mb.to_string(),
        "-cores".into(),
        spec.guest_cores.to_string(),
        "-netdelay".into(),
        "none".into(),
        "-netspeed".into(),
        "full".into(),
        // Disable guest Vulkan passthrough (`-feature -Vulkan`): on
        // Intel iGPUs (`host has ext semaphore? win32 0`),
        // `goldfish_vulkan` crashes Chrome/WebView `CompositorGpuTh`
        // with a null function-pointer dereference (`rip = 0x0`) and
        // aborts QuickBoot saves with `UNSUPPORTED_VK_APP`. Hardware
        // OpenGL ES 3.0 (`-gpu host`) remains fully active.
        "-feature".into(),
        "-Vulkan".into(),
        "-verbose".into(),
        // Everything after `-qemu` passes through to qemu-system.
        // The human monitor on 127.0.0.1:4444 is the injection path
        // for `mouse_move`/`mouse_button`/`sendkey` (works in
        // -no-window where console `event send` is dropped) and the
        // graceful `quit` that saves the QuickBoot snapshot.
        "-qemu".into(),
        "-monitor".into(),
        format!("tcp:127.0.0.1:{},server,nowait", QemuMonitor::PORT),
    ]
}

/// Boot Android headlessly. `gpu` = `"host"` (gfxstream→ANGLE→D3D) or
/// `"swiftshader_indirect"`; `accel` = `"whpx"` or `"auto"`; `spec`
/// scales guest RAM/cores/LCD to host resources.
pub fn launch(
    profile: &DeviceProfile,
    gpu: &str,
    accel: &str,
    spec: &crate::config::SpecProfile,
) -> Result<EmulatorProc, String> {
    let InstallState::Ready {
        emulator, sysimg, ..
    } = install_state()
    else {
        return Err("Android runtime not installed".into());
    };
    let root = runtime_dir();
    // Software renderers (swangle/swiftshader/software/lavapipe) run
    // ALL guest composition on CPU — at 1280x720 the composer HAL eats
    // ~60% of a core even on an idle screen (measured live), starving
    // system_server into ANRs. Halving the pixels to 960x540 cuts that
    // almost in half; the host compositor upscales via CAS anyway.
    let soft_spec;
    let spec =
        if matches!(gpu, "swangle" | "swiftshader" | "software" | "lavapipe") && spec.lcd_w > 960 {
            soft_spec = crate::config::SpecProfile {
                lcd_w: 960,
                lcd_h: 540,
                lcd_dpi: 160,
                ..*spec
            };
            &soft_spec
        } else {
            spec
        };
    write_avd(&root, &sysimg, profile, gpu, spec)?;
    // Engine stdout+stderr → logs\emulator.log so an early exit (e.g.
    // "PANIC: Cannot find AVD system path") is diagnosable. Rotated
    // once per PROCESS run — the log reached 6.6 MB across sessions
    // and every `emulator_log_tail` call had to slurp it. Per-attempt
    // rotation would erase the previous attempt's FATAL line, so the
    // once flag lives at process scope, not per launch().
    let logs = data_dir().join("logs");
    let _ = std::fs::create_dir_all(&logs);
    static EMU_LOG_ROTATED: AtomicBool = AtomicBool::new(false);
    let emu_log = logs.join("emulator.log");
    if !EMU_LOG_ROTATED.swap(true, Ordering::AcqRel) && emu_log.exists() {
        let _ = std::fs::rename(&emu_log, logs.join("emulator.prev.log"));
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&emu_log)
        .map_err(|e| format!("emulator.log: {e}"))?;
    let log_err = log_file.try_clone().map_err(|e| e.to_string())?;
    let child = super::silent_cmd(&emulator)
        .args(emulator_args(gpu, accel, spec))
        .current_dir(emulator.parent().unwrap())
        // Point the engine entirely inside our runtime dir — AVD and
        // SDK resolution never touch the user's global ~/.android.
        .env("ANDROID_SDK_ROOT", &root)
        .env("ANDROID_HOME", &root)
        .env("ANDROID_EMULATOR_HOME", &root)
        .env("ANDROID_AVD_HOME", avd_home())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_err))
        .spawn()
        .map_err(|e| format!("spawn emulator: {e}"))?;
    // Published BEFORE the pid-file record exists — the poller's
    // liveness check counts a live owned child as "still booting".
    OWNED_EMU_PID.store(child.id(), Ordering::Release);
    let job = attach_kill_job(&child);
    log_line(&format!(
        "emulator launched (gpu={gpu} accel={accel} job={job})"
    ));
    Ok(EmulatorProc {
        child,
        gpu: gpu.to_string(),
        launched_at: Instant::now(),
        retried: false,
        job,
    })
}

// ---------------------------------------------------------------------------
// GPU auto-selection + resilient fallback
// ---------------------------------------------------------------------------

/// VendorId of the primary DXGI adapter (0x10DE NVIDIA, 0x1002 AMD,
/// 0x8086 Intel). `None` when enumeration fails — treat as discrete.
pub fn primary_gpu_vendor() -> Option<u32> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        for i in 0u32.. {
            let Ok(adapter) = factory.EnumAdapters1(i) else {
                break;
            };
            let Ok(desc) = adapter.GetDesc1() else {
                continue;
            };
            // Skip Microsoft's software (WARP/BasicRender) adapter.
            if desc.VendorId != 0x1414 {
                return Some(desc.VendorId);
            }
        }
        None
    }
}

/// Ordered `-gpu` candidates for the fallback chain — emulator >=35
/// names only (`-help-gpu`: auto | host | swiftshader | swangle |
/// software | lavapipe). The legacy `*_indirect` modes were removed:
/// `angle_indirect`/`swiftshader_indirect` are rejected and remapped
/// to `auto` with an ERROR line. `host` leads: on the i3/Intel-UHD
/// test host `-gpu auto` resolved to swiftshader/lavapipe (SOFTWARE
/// rendering) while `-gpu host` passed the engine's own compat check
/// (`hasSufficientHwGpu: passed`) — software capture is what wedged
/// `screencap` for 15-60 s. If `host` dies within the 12 s watch
/// window the chain falls through to `auto`; `swiftshader` is the
/// always-valid software last resort.
///
/// Modes that crashed MID-SESSION are demoted behind everything
/// crash-free: `-gpu host` runs gfxstream over the native vendor GL
/// driver and Intel iGPU hosts (igxelpicd64.dll, observed APPCRASH
/// c0000005 killing qemu 20-30 min into a session — this is the
/// "browser opened then the app died" report) die outside the 12 s
/// launch watch. `record_gpu_crash` persists one line per crash;
/// a mode with any crash record goes last, so the NEXT launch skips
/// the flaky driver. `swangle`/`swiftshader` (SwiftShader CPU) can
/// never die in a vendor driver — they anchor the tail.
pub fn gpu_chain(force_swiftshader: bool) -> Vec<&'static str> {
    if force_swiftshader {
        return vec!["swiftshader"];
    }
    let base = ["host", "auto", "swangle", "swiftshader"];
    // `auto` resolves to the SAME native-GL path whenever the host
    // driver is healthy enough to pass the compat check — so a `host`
    // crash record implicates `auto` too (verified live: gpu=auto on
    // Intel UHD selected `gles_mode_selected:host` → same igxelpicd64
    // crash class). Software modes have no such alias.
    let eff_crashes = |m: &str| {
        gpu_crash_count(m)
            + if m == "auto" {
                gpu_crash_count("host")
            } else {
                0
            }
    };
    let mut chain: Vec<&'static str> = base
        .iter()
        .copied()
        .filter(|m| eff_crashes(m) == 0)
        .collect();
    // Crashed modes stay reachable as a deeper fallback (sorted by
    // fewest crashes) — better a flaky host GPU than a dead emulator.
    let mut crashed: Vec<&'static str> = base
        .iter()
        .copied()
        .filter(|m| eff_crashes(m) > 0)
        .collect();
    crashed.sort_by_key(|m| eff_crashes(m));
    chain.extend(crashed);
    chain
}

/// One line per mid-session GPU-mode crash in
/// `GorillaLuxe/gpu_crashes.txt` — the persistence that lets
/// [`gpu_chain`] demote a flaky mode forever after it dies once
/// (launch-time deaths inside the 12 s watch never reach this file:
/// the chain already advances on its own).
pub fn record_gpu_crash(mode: &str) {
    let path = data_dir().join("gpu_crashes.txt");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{mode}");
    }
}

fn gpu_crash_count(mode: &str) -> u32 {
    let path = data_dir().join("gpu_crashes.txt");
    std::fs::read_to_string(&path)
        .map(|s| s.lines().filter(|l| l.trim() == mode).count() as u32)
        .unwrap_or(0)
}

/// Block until no live pid holds our AVD (≤20 s). A killed qemu can
/// take several seconds to release `multiinstance.lock` and delete
/// its `pid_*.ini` record — spawning while it still lives trips the
/// engine's `FATAL: Running multiple emulators with the same AVD`.
fn wait_avd_drained() {
    let deadline = Instant::now() + std::time::Duration::from_secs(20);
    while Instant::now() < deadline {
        if our_avd_live_pids().is_empty() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    log_line("WARN AVD still held after 20 s drain wait");
}

/// Delete stale qemu lock files that block a cold relaunch after an
/// unclean exit (`multiinstance.lock`, `hardware-qemu.ini.lock`, etc).
fn purge_stale_locks(avd: &Path) {
    if let Ok(rd) = std::fs::read_dir(avd) {
        for e in rd.flatten() {
            if e.path().extension().is_some_and(|x| x == "lock") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Last lines of `logs\emulator.log` — surfaced in errors/UI on early exit.
fn emulator_log_tail() -> String {
    let p = data_dir().join("logs").join("emulator.log");
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Emulator attachment: `Owned` = spawned by us (Job Object bound,
/// killed with the app); `Attached` = an emulator that was already
/// running our AVD — we adopt it without respawning (and never kill
/// it on shutdown, since we don't own it).
pub enum EmulatorHandle {
    Owned(EmulatorProc),
    Attached { since: Instant },
}

impl EmulatorHandle {
    /// When the engine was spawned (or adopted) — drives the
    /// boot-elapsed HUD counter.
    pub fn launched_at(&self) -> Instant {
        match self {
            EmulatorHandle::Owned(e) => e.launched_at,
            EmulatorHandle::Attached { since, .. } => *since,
        }
    }

    /// `-gpu` mode of an owned proc (`None` for attached engines —
    /// we never spawned them so their mode is unknown).
    pub fn gpu_mode(&self) -> Option<String> {
        match self {
            EmulatorHandle::Owned(e) => Some(e.gpu.clone()),
            EmulatorHandle::Attached { .. } => None,
        }
    }
}

/// `true` while the emulator is up. Never spawns a process — the
/// `Attached` branch reads [`EMU_ONLINE`] (refreshed ~2 s by the
/// background liveness poller); `Owned` is a `try_wait` syscall.
/// Calling this on the UI thread is safe.
pub fn alive(h: &mut EmulatorHandle) -> bool {
    match h {
        EmulatorHandle::Owned(e) => matches!(e.child.try_wait(), Ok(None)),
        EmulatorHandle::Attached { since } => {
            EMU_ONLINE.load(Ordering::Acquire)
                // Boot grace — adbd only registers `emulator-5554`
                // ~10-60 s into boot and the poller needs a couple of
                // ticks; a live pid owning our AVD counts as alive.
                || (since.elapsed() < std::time::Duration::from_secs(BOOT_GRACE_S)
                    && !our_avd_live_pids().is_empty())
        }
    }
}

impl EmulatorHandle {
    /// Terminate the emulator we spawned. `Attached` is left running —
    /// it predates us.
    pub fn shutdown(&mut self) {
        if let EmulatorHandle::Owned(e) = self {
            e.shutdown();
        }
    }
}

/// `true` when adb already sees a live emulator (e.g. a previous
/// `emulator.exe` still booted under `Temp\avd\running\pid_*.ini`).
fn adb_device_present() -> bool {
    adb_run(&["devices"])
        .map(|d| {
            d.lines()
                .any(|l| l.starts_with("emulator-") && l.ends_with("device"))
        })
        .unwrap_or(false)
}

/// `adb reconnect` + settle + recheck — returns true when a device
/// that was reported `offline` came back as `device`.
fn adb_reconnect() -> bool {
    let _ = adb_run(&["reconnect"]);
    std::thread::sleep(std::time::Duration::from_secs(2));
    adb_device_present()
}

/// `true` while `pid` refers to a live process.
fn pid_alive(pid: u32) -> bool {
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => {
                let _ = CloseHandle(h);
                true
            }
            Err(_) => false,
        }
    }
}

/// Live pids from `%TEMP%\avd\running\pid_*.ini` whose record names OUR
/// `GorillaLuxe` AVD (`avd.id=`/`avd.dir=` lines) — `(pid, file age
/// secs)`. Every emulator.exe of this user writes these records, so
/// the avd match is what scopes them to us: foreign emulators
/// (Android Studio, other AVDs) are never returned. Records whose pid
/// is dead are deleted on sight — that IS the stale-file cleanup.
fn our_avd_live_pids() -> Vec<(u32, u64)> {
    let run_dir = std::env::temp_dir().join(r"avd\running");
    let our_dir = avd_home().join("GorillaLuxe.avd");
    let our_dir_s = our_dir.to_string_lossy().to_string();
    let mut out = Vec::new();
    for e in std::fs::read_dir(&run_dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(stem) = name
            .strip_prefix("pid_")
            .and_then(|s| s.strip_suffix(".ini"))
        else {
            continue;
        };
        let Ok(pid) = stem.parse::<u32>() else {
            continue;
        };
        if !pid_alive(pid) {
            let _ = std::fs::remove_file(e.path());
            continue;
        }
        let body = std::fs::read_to_string(e.path()).unwrap_or_default();
        let ours =
            body.lines().any(|l| l.trim() == "avd.id=GorillaLuxe") || body.contains(&our_dir_s);
        if !ours {
            continue;
        }
        let age = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX);
        out.push((pid, age));
    }
    out
}

/// Seconds a live pid may hold our AVD without adb seeing it before
/// it's declared a zombie rather than a booting guest.
const BOOT_GRACE_S: u64 = 120;

/// A guest `screencap` on a loaded host can legitimately take a few
/// seconds; a pipe silent for this long is a wedge, not a slow frame
/// (observed: >10 min of silence while adbd stayed alive).
const STREAM_STALL_MS: u64 = 30_000;

/// One-shot `adb exec-out screencap` budget — a wedged guest capture
/// is killed at this deadline instead of blocking the frame worker
/// forever (same failure class as the pipe stall).
/// Deadline per screencap attempt. During boot SurfaceFlinger is
/// contending with zygote/package-scan/QuickBoot-restore — live
/// captures there took 15-30 s on the i3 test host, so a short fuse
/// murders every first frame and the guest-side orphan retries
/// starve each other (retry storm). Post-boot a >15 s capture IS a
/// wedge; pre-`boot_completed` we allow a full minute.
const CAPTURE_TIMEOUT_MS: u64 = 15_000;
const CAPTURE_TIMEOUT_BOOT_MS: u64 = 60_000;

fn capture_deadline() -> std::time::Duration {
    std::time::Duration::from_millis(if guest_booted() {
        CAPTURE_TIMEOUT_MS
    } else {
        CAPTURE_TIMEOUT_BOOT_MS
    })
}

// ---------------------------------------------------------------------------
// UI-visible guest state — written ONLY by background threads, read by
// the render/UI thread. The UI thread must never spawn `adb.exe`:
// a single 50–300 ms process spawn on the message pump made the HUD
// buttons feel dead (bug report: clicks on F1/F2/RU dropped while the
// Attached-liveness probe shelled out every 2 s).
// ---------------------------------------------------------------------------

/// `adb get-state == device` for our emulator — refreshed ~every 2 s
/// by [`start_liveness_poller`].
static EMU_ONLINE: AtomicBool = AtomicBool::new(false);
/// `sys.boot_completed == 1` — latched once by the same poller.
static BOOT_COMPLETED: AtomicBool = AtomicBool::new(false);
/// Non-black frames delivered to the UI — drives the "waiting for
/// display" chip and the HUD frame counter.
pub static FRAME_COUNT: AtomicU64 = AtomicU64::new(0);
/// Guards against two poller threads.
static POLLER_RUNNING: AtomicBool = AtomicBool::new(false);
/// Pid of the `emulator.exe` WE spawned (0 = none). The pid-file
/// record under `%TEMP%\avd\running\` only appears after the launcher
/// finishes its init — on a fresh AVD (marker migration, first
/// install) that can take 30-60 s, longer than the poller's miss
/// window. This pid is the liveness signal that covers the gap.
static OWNED_EMU_PID: AtomicU32 = AtomicU32::new(0);

/// Launch/boot stage the HUD chip reports — replaces the static
/// "Загрузка Android 13..." with the REAL current step so a stuck
/// launch shows exactly where it is stuck.
pub mod stage {
    pub const IDLE: u8 = 0;
    pub const CHECK_RUNTIME: u8 = 1;
    pub const SPAWN_EMULATOR: u8 = 2;
    pub const WAIT_ADBD: u8 = 3;
    pub const WAIT_FRAMES: u8 = 4;
    pub const BOOTED: u8 = 5;
    pub const FAILED: u8 = 6;
}
static BOOT_STAGE: AtomicU64 = AtomicU64::new(0);

/// Exact failure text for the HUD — set once and latched until the
/// next launch attempt resets it.
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

pub fn set_stage(s: u8) {
    BOOT_STAGE.store(s as u64, Ordering::Release);
}
pub fn boot_stage() -> u8 {
    BOOT_STAGE.load(Ordering::Acquire) as u8
}
pub fn set_error(msg: &str) {
    *LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg.to_string());
    log_line(&format!("ERROR {msg}"));
    set_stage(stage::FAILED);
}
pub fn last_error() -> Option<String> {
    LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// One-line stage summary for the boot card/HUD — `[n/6] <stage>`
/// plus the latched error when the launch failed. Bilingual via i18n.
pub fn stage_line(lang: crate::i18n::Lang) -> String {
    use crate::i18n::tr;
    let s = boot_stage();
    let key = match s {
        stage::CHECK_RUNTIME => "ui.stage.check",
        stage::SPAWN_EMULATOR => "ui.stage.spawn",
        stage::WAIT_ADBD => "ui.stage.adb",
        stage::WAIT_FRAMES => "ui.stage.frames",
        stage::BOOTED => "ui.stage.booted",
        stage::FAILED => "ui.stage.failed",
        _ => "ui.stage.check",
    };
    let mut out = format!("[{s}/6] {}", tr(lang, key));
    if let Some(e) = last_error() {
        out.push_str(&format!(" — {e}"));
    }
    out
}

/// Guest reached `sys.boot_completed=1` (poller-latched).
pub fn guest_booted() -> bool {
    BOOT_COMPLETED.load(Ordering::Acquire)
}

/// adb sees our emulator in `device` state (poller-fresh, ≤2 s stale).
pub fn emulator_online() -> bool {
    EMU_ONLINE.load(Ordering::Acquire)
}

/// Reset the latches for a fresh launch — called by the launch path.
pub fn reset_guest_state() {
    EMU_ONLINE.store(false, Ordering::Release);
    BOOT_COMPLETED.store(false, Ordering::Release);
    FRAME_COUNT.store(0, Ordering::Release);
    GUEST_LCD.store(0, Ordering::Release);
}

/// Background thread: every 2 s refresh [`EMU_ONLINE`], latch
/// [`BOOT_COMPLETED`], and — on the first `device` sighting — wake
/// the guest display (sleeping SurfaceFlinger = all-black captures).
/// Exits when the emulator vanishes for good (10 misses with no live
/// GorillaLuxe-AVD pid).
pub fn start_liveness_poller() {
    if POLLER_RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(|| {
        let mut misses = 0u32;
        let mut woke = false;
        let mut calmed_store = false;
        let mut anr_tick = 0u32;
        let mut anr_ticks = 6u32;
        let mut anr_streak = 0u32;
        loop {
            let online = adb_run(&["get-state"])
                .map(|s| s.trim() == "device")
                .unwrap_or(false);
            EMU_ONLINE.store(online, Ordering::Release);
            if online {
                misses = 0;
                if !woke {
                    // keyevent 82 = MENU (wakes + unlocks a slept
                    // lockscreen); wm dismiss-keyguard clears the
                    // keyguard window — without these an early-boot
                    // display can sit black with boot_completed=1.
                    wake_guest();
                    // Single shell invocation for all early-boot tuning:
                    //  1) nativebridge for ARM64 games;
                    //  2) cap dex2oat to 1 thread + disable bg-dexopt so
                    //     background compilation never starves SystemUI;
                    //  3) zero window/transition animations + suppress
                    //     background ANR popups;
                    //  4) disable preinstalled non-gaming bloatware whose
                    //     Play-Store `rapid_auto_update` (YouTube Music
                    //     83 MB, Maps, Photos, Videos, SearchBox, etc.)
                    //     pegs system_server at 70% CPU on 2-core hosts.
                    //     Play Store (`com.android.vending`) & GMS stay
                    //     enabled for user-initiated installs.
                    let init_script = concat!(
                        "setprop persist.sys.nativebridge 1;",
                        "setprop debug.hwui.renderer opengl;",
                        "setprop dalvik.vm.dex2oat-threads 1;",
                        "setprop dalvik.vm.boot-dex2oat-threads 1;",
                        "cmd package bg-dexopt-job --disable >/dev/null 2>&1;",
                        "settings put global anr_show_background 0;",
                        // Skip SetupWizard provisioning entirely — SUW
                        // + GMS first-signin is the biggest single
                        // cold-boot CPU consumer (~60% for 5+ min,
                        // measured live) and a headless box has no
                        // wizard to show anyway. Marking provisioned
                        // before disabling keeps GMS services happy.
                        "settings put global device_provisioned 1;",
                        "settings put secure user_setup_complete 1;",
                        "pm disable-user --user 0 com.google.android.setupwizard >/dev/null 2>&1;",
                        "settings put global window_animation_scale 0.0;",
                        "settings put global transition_animation_scale 0.0;",
                        "settings put global animator_duration_scale 0.0;",
                        "pm enable com.google.android.googlequicksearchbox >/dev/null 2>&1;",
                        "for p in ",
                        "com.google.android.apps.youtube.music ",
                        "com.google.android.youtube ",
                        "com.google.android.apps.maps ",
                        "com.google.android.apps.photos ",
                        "com.google.android.videos ",
                        "com.google.android.apps.tachyon ",
                        "com.google.android.apps.docs ",
                        "com.google.android.apps.wellbeing; ",
                        "do pm disable-user --user 0 $p >/dev/null 2>&1; done"
                    );
                    let _ = adb_run(&["-e", "shell", init_script]);
                    log_line("guest wake + keyguard dismiss + ANR/bloatware tuning sent");
                    woke = true;
                }
                if !BOOT_COMPLETED.load(Ordering::Acquire)
                    && adb_run(&["-e", "shell", "getprop", "sys.boot_completed"])
                        .map(|s| s.trim() == "1")
                        .unwrap_or(false)
                {
                    BOOT_COMPLETED.store(true, Ordering::Release);
                    set_stage(stage::BOOTED);
                    log_line("sys.boot_completed=1");
                    if !calmed_store {
                        let _ = adb_run(&[
                            "-e",
                            "shell",
                            "cmd package bg-dexopt-job --cancel >/dev/null 2>&1",
                        ]);
                        calmed_store = true;
                    }
                }
                anr_tick += 1;
                if BOOT_COMPLETED.load(Ordering::Acquire) && anr_tick >= anr_ticks {
                    let count = dismiss_anr_dialogs(anr_streak);
                    anr_tick = 0;
                    if count > 0 {
                        anr_streak = anr_streak.saturating_add(1);
                        anr_ticks = 2; // ~4 s re-check while clearing
                    } else {
                        anr_streak = 0;
                        anr_ticks = 6; // ~12 s quiet cadence
                    }
                }
            } else {
                let owned = OWNED_EMU_PID.load(Ordering::Acquire);
                let child_live = owned != 0 && pid_alive(owned);
                if child_live || !our_avd_live_pids().is_empty() {
                    misses = 0;
                } else {
                    misses += 1;
                }
                if misses >= 10 {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        EMU_ONLINE.store(false, Ordering::Release);
        POLLER_RUNNING.store(false, Ordering::Release);
        log_line("liveness poller: emulator gone — exiting");
    });
}

/// Wake the guest display + drop the keyguard — safe to call on any
/// background thread once adb is attached.
pub fn wake_guest() {
    let _ = adb_run(&["-e", "shell", "input", "keyevent", "82"]);
    let _ = adb_run(&["-e", "shell", "wm", "dismiss-keyguard"]);
}

/// Dismiss guest "Application Not Responding" dialogs via the sub-ms
/// emulator console mouse channel (`console_tap` → `event mouse` on
/// :5554, zero guest JVM spawn).
///
/// Button coordinates measured against the Android 13 AOSP
/// `AppNotRespondingDialog` (`mFrame=[224,212][1055,507]` on 1280x720):
/// * `"Close app"` (`aerr_close`) sits at **52 %** down `mFrame` (y≈365);
/// * `"Wait"` (`aerr_wait`) sits at **78 %** down `mFrame` (y≈442);
///   (70 % = y≈418 landed in the dead gap between the two rows).
///
/// On the first two sightings (`streak < 2`) we tap `"Wait"`. If the
/// dialog is still present on `streak >= 2`, the process (e.g. a
/// wedged `com.android.systemui`) is deadlocked and clicking "Wait"
/// only re-schedules the same ANR every 5 s — we tap `"Close app"` so
/// `ActivityManager` kills and immediately respawns a clean SystemUI.
fn dismiss_anr_dialogs(_streak: u32) -> usize {
    let Ok(dump) = adb_run(&["-e", "shell", "dumpsys", "window", "windows"]) else {
        return 0;
    };
    let found = dump
        .lines()
        .filter(|l| l.contains("Window #") && l.contains("Application Not Responding:"))
        .count();
    if found == 0 {
        return 0;
    }
    // Precise "Wait" hit: the ANR mFrame only loosely tracks the
    // visible AlertDialog (verified live — 78%-of-frame taps landed
    // between the title and the buttons). `uiautomator dump` hands us
    // the real button bounds; tap each `Wait`/`Подождать` node center.
    // "Close app"/"Закрыть" is NEVER tapped (killing SystemUI blanks
    // the wallpaper surface to all-black).
    let _ = adb_run(&["-e", "shell", "uiautomator", "dump", "/sdcard/glx_ui.xml"]);
    if let Ok(ui) = adb_run(&["-e", "shell", "cat", "/sdcard/glx_ui.xml"]) {
        let mut rest = ui.as_str();
        while let Some(p) = rest.find("bounds=\"[") {
            let seg = &rest[..p];
            let node_start = seg.rfind("<node").unwrap_or(0);
            let node = &seg[node_start..];
            let is_wait = ["\"Wait\"", "\"Подождать\""]
                .iter()
                .any(|t| node.contains(&format!("text={t}")));
            rest = &rest[p + 8..];
            if !is_wait {
                continue;
            }
            let coords: Vec<i32> = p_split_bounds(&rest[..rest.find('"').unwrap_or(40)]);
            if coords.len() >= 4 {
                let tx = ((coords[0] + coords[2]) / 2).max(0) as u32;
                let ty = ((coords[1] + coords[3]) / 2).max(0) as u32;
                if console_tap(tx, ty).is_err() {
                    let _ = adb_run(&[
                        "-e",
                        "shell",
                        "input",
                        "tap",
                        &tx.to_string(),
                        &ty.to_string(),
                    ]);
                }
                log_line(&format!("ANR dialog dismissed via Wait tap ({tx},{ty})"));
            }
        }
    }
    found
}

/// Parse `[x1,y1][x2,y2]` from a uiautomator `bounds` attribute body.
fn p_split_bounds(s: &str) -> Vec<i32> {
    s.split(['[', ']', ','])
        .filter_map(|t| t.trim().parse::<i32>().ok())
        .collect()
}

/// Attach to a running emulator, else clear stale state, then spawn →
/// watch 12 s → if the engine died, retry with the next `-gpu` mode in
/// the vendor-aware chain. `accel` = `"whpx"` (mapped to `-accel on`,
/// required-hypervisor) or `"auto"`.
///
/// Two `emulator.exe` instances can never share an AVD — a second
/// spawn dies instantly with `FATAL: Running multiple emulators with
/// the same AVD`. Detection has three cases, in order:
/// 1. `adb devices` already lists `emulator-*` → adopt it.
/// 2. A live `pid_*.ini` holds OUR AVD but adb doesn't see it — fresh
///    (< BOOT_GRACE_S) means mid-boot → attach; stale means a zombie
///    with dead adbd → kill THAT pid only (never an image-name
///    `taskkill /IM`, which would nuke foreign emulators too).
/// 3. Dead pids → their ini files are deleted inside the scan, then
///    `*.lock` leftovers are purged before the spawn.
pub fn launch_resilient(
    profile: &DeviceProfile,
    accel: &str,
    force_swiftshader: bool,
    spec: &crate::config::SpecProfile,
) -> Result<EmulatorHandle, String> {
    reset_guest_state();
    *LAST_ERROR.lock().unwrap_or_else(|e| e.into_inner()) = None;
    set_stage(stage::CHECK_RUNTIME);
    start_liveness_poller();
    set_stage(stage::SPAWN_EMULATOR);
    if adb_device_present() {
        log_line("live emulator detected — attaching (no respawn)");
        set_stage(stage::WAIT_ADBD);
        return Ok(EmulatorHandle::Attached {
            since: Instant::now(),
        });
    }
    let ours = our_avd_live_pids();
    if ours.iter().any(|(_, age)| *age < BOOT_GRACE_S) {
        log_line("our AVD held by a booting emulator (no adb yet) — attaching");
        set_stage(stage::WAIT_ADBD);
        return Ok(EmulatorHandle::Attached {
            since: Instant::now(),
        });
    }
    if !ours.is_empty() && adb_reconnect() {
        // `adb devices` reports `offline` whenever the console link
        // exists but the adbd handshake dropped — often transient
        // (memory pressure, TCG stall). A reconnect that flips the
        // device back to `device` saves us from killing a healthy
        // guest; `our_avd_live_pids` confirmed it IS ours, so a
        // reconnect is safe — no foreign AVD can be adopted.
        log_line("adb reconnect revived the emulator — attaching");
        set_stage(stage::WAIT_ADBD);
        return Ok(EmulatorHandle::Attached {
            since: Instant::now(),
        });
    }
    for (pid, _) in &ours {
        log_line(&format!(
            "stale emulator pid {pid} holds GorillaLuxe.avd — killing"
        ));
        let _ = super::silent_cmd("taskkill.exe")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output();
    }
    wait_avd_drained();
    purge_stale_locks(&avd_home().join("GorillaLuxe.avd"));
    let chain = gpu_chain(force_swiftshader);
    let mut last = String::new();
    for (i, gpu) in chain.iter().enumerate() {
        // A still-dying previous attempt holds the AVD lock — the next
        // spawn would die instantly with FATAL "multiple emulators",
        // burning a GPU mode for a lock race (observed live: the whole
        // chain failed while the first qemu was still flushing).
        wait_avd_drained();
        // Remove stale locks BEFORE qemu grabs the AVD — leftover
        // `*.lock` files from an unclean exit block cold relaunch.
        purge_stale_locks(&avd_home().join("GorillaLuxe.avd"));
        let mut emu = match launch(profile, gpu, accel, spec) {
            Ok(e) => e,
            Err(e) => {
                last = format!("gpu={gpu}: {e}");
                log_line(&format!("launch attempt {i} failed: {last}"));
                continue;
            }
        };
        emu.retried = i > 0;
        // Watch: a GPU-mode failure kills emulator.exe within ~2-10 s.
        let deadline = Instant::now() + std::time::Duration::from_secs(12);
        let mut alive_past = true;
        while Instant::now() < deadline {
            if matches!(emu.child.try_wait(), Ok(Some(_)) | Err(_)) {
                alive_past = false;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        if alive_past {
            log_line(&format!(
                "emulator stable on -gpu {gpu} (attempt {})",
                i + 1
            ));
            set_stage(stage::WAIT_ADBD);
            return Ok(EmulatorHandle::Owned(emu));
        }
        last = format!("gpu={gpu}: exited within 12s — {}", emulator_log_tail());
        log_line(&format!("launch attempt {} failed: {last}", i + 1));
        record_gpu_crash(gpu);
        emu.shutdown();
    }
    let msg = format!("all GPU modes failed ({accel}): {last}");
    set_error(&msg);
    Err(msg)
}

// ---------------------------------------------------------------------------
// ADB bridge — persistent shell + one-shot commands + frame capture
// ---------------------------------------------------------------------------

fn adb() -> std::ffi::OsString {
    adb_exe().into_os_string()
}

fn adb_run(args: &[&str]) -> Result<String, String> {
    let out = super::silent_cmd(adb())
        .args(args)
        .output()
        .map_err(|e| format!("adb: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Persistent `adb shell` session — commands are written to its stdin
/// and flushed immediately (<5 ms vs ~150 ms per-spawn).
pub struct AdbShell {
    child: Child,
    stdin: ChildStdin,
}

impl AdbShell {
    pub fn open() -> Result<Self, String> {
        let mut child = super::silent_cmd(adb())
            .args(["-e", "shell"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("adb shell: {e}"))?;
        let stdin = child.stdin.take().ok_or("shell stdin")?;
        Ok(AdbShell { child, stdin })
    }

    /// Write one shell command line + flush. Returns Err if the pipe
    /// died (caller should reopen).
    pub fn cmd(&mut self, line: &str) -> Result<(), String> {
        if !self.alive() {
            return Err("adb shell exited".into());
        }
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|e| e.to_string())
    }

    /// `true` while the shell process is alive.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn tap(&mut self, x: u32, y: u32) -> Result<(), String> {
        self.cmd(&format!("input tap {x} {y}"))
    }
    pub fn swipe(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, ms: u32) -> Result<(), String> {
        self.cmd(&format!("input swipe {x1} {y1} {x2} {y2} {ms}"))
    }
    pub fn keyevent(&mut self, code: u32) -> Result<(), String> {
        self.cmd(&format!("input keyevent {code}"))
    }
    /// `input text` — POSIX-shell-quoted (`%s` for space, `%25` for
    /// percent). Non-control chars pass through as UTF-8.
    pub fn text(&mut self, s: &str) -> Result<(), String> {
        let mut esc = String::with_capacity(s.len() + 2);
        esc.push('\'');
        for c in s.chars() {
            match c {
                '\'' => esc.push_str("'\"'\"'"),
                ' ' => esc.push_str("%s"),
                '%' => esc.push_str("%25"),
                c if c.is_control() => {}
                c => esc.push(c),
            }
        }
        esc.push('\'');
        self.cmd(&format!("input text {esc}"))
    }
    /// Guest locale sync (spec §3.5): `ru-RU` / `en-US`.
    pub fn set_locale(&mut self, locale: &str) -> Result<(), String> {
        let lang = locale.split('-').next().unwrap_or("en");
        self.cmd(&format!("setprop persist.sys.locale {locale}"))?;
        self.cmd(&format!("setprop persist.sys.language {lang}"))
    }
}

impl Drop for AdbShell {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// One-shot `input tap` (fallback when no persistent shell attached).
pub fn send_tap(x: u32, y: u32) -> Result<(), String> {
    adb_run(&["shell", "input", "tap", &x.to_string(), &y.to_string()]).map(|_| ())
}

/// One-shot `input keyevent` fallback.
pub fn send_keyevent(code: u32) -> Result<(), String> {
    adb_run(&["shell", "input", "keyevent", &code.to_string()]).map(|_| ())
}

/// One-shot `input swipe` fallback.
pub fn send_swipe(x1: i32, y1: i32, x2: i32, y2: i32, ms: u32) -> Result<(), String> {
    adb_run(&[
        "shell",
        "input",
        "swipe",
        &x1.to_string(),
        &y1.to_string(),
        &x2.to_string(),
        &y2.to_string(),
        &ms.to_string(),
    ])
    .map(|_| ())
}

/// One-shot guest locale sync fallback.
pub fn set_locale(locale: &str) -> Result<(), String> {
    let lang = locale.split('-').next().unwrap_or("en");
    adb_run(&["shell", "setprop", "persist.sys.locale", locale])?;
    adb_run(&["shell", "setprop", "persist.sys.language", lang]).map(|_| ())
}

/// Block (caller runs this on a thread) until `adb` reports a device.
pub fn wait_for_device() -> bool {
    super::silent_cmd(adb())
        .arg("wait-for-device")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `true` once the guest reports `sys.boot_completed=1` — polled every
/// 2 s, up to `timeout_s`. Frame streaming must NOT wait on this: the
/// boot animation is live-captured the moment adbd answers.
pub fn boot_completed(timeout_s: u64) -> bool {
    let deadline = Instant::now() + std::time::Duration::from_secs(timeout_s);
    while Instant::now() < deadline {
        if let Ok(out) = adb_run(&["shell", "getprop", "sys.boot_completed"]) {
            if out.trim() == "1" {
                return true;
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    false
}

/// Install an `.apk`/`.xapk` dropped onto the window. `.xapk`/`.apks`
/// bundles are unzipped and installed via `install-multiple` so split
/// APKs (base + config.arm64_v8a + config.xxhdpi) land atomically.
pub fn install_package(path: &Path) -> Result<String, String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if ext == "apk" {
        return adb_run(&["install", "-r", &path.to_string_lossy()]);
    }
    if ext != "xapk" && ext != "apks" {
        return Err(format!("unsupported package type: {ext}"));
    }
    // Extract the split APKs to a temp dir (path-traversal safe).
    let tmp = std::env::temp_dir().join(format!("gorillaluxe-xapk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let result = (|| -> Result<Vec<PathBuf>, String> {
        let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut zip = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
        let mut apks = Vec::new();
        for i in 0..zip.len() {
            let mut e = zip.by_index(i).map_err(|e| e.to_string())?;
            let Some(rel) = e.enclosed_name() else {
                continue;
            };
            let name = rel.to_string_lossy().to_string();
            if !name.to_ascii_lowercase().ends_with(".apk") || name.contains('/') {
                continue;
            }
            let out = tmp.join(&name);
            std::io::copy(
                &mut e,
                &mut std::fs::File::create(&out).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            apks.push(out);
        }
        Ok(apks)
    })();
    let mut apks = match result {
        Ok(a) if !a.is_empty() => a,
        Ok(_) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err("xapk contains no .apk files".into());
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!("xapk extract: {e}"));
        }
    };
    // base.apk first, then splits.
    apks.sort_by_key(|p| !p.file_name().map(|n| n == "base.apk").unwrap_or(false));
    let mut args: Vec<String> = vec!["install-multiple".into(), "-r".into()];
    args.extend(apks.iter().map(|p| p.to_string_lossy().to_string()));
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let r = adb_run(&arg_refs);
    let _ = std::fs::remove_dir_all(&tmp);
    r
}

/// Kept for older call sites — forwards to `install_package`.
pub fn install_apk(path: &Path) -> Result<String, String> {
    install_package(path)
}

/// Parse the raw `screencap` (no `-p`) header.
/// Android ≤11 writes 12 bytes `(w: u32, h: u32, fmt: u32)`; Android
/// 12/13 (API 31+) appends a `dataspace: u32` — a 16-byte header.
/// `fmt == 1` means RGBA_8888. Returns `(w, h, pixel_offset)` — the
/// offset is derived from the total length so a 16-byte capture is
/// never misparsed as 12 (the `dataspace` u32 would otherwise be read
/// as pixel 0 and shift the whole image by one pixel).
/// Pure, unit-testable.
pub fn parse_screencap_header(b: &[u8]) -> Option<(u32, u32, usize)> {
    if b.len() < 12 {
        return None;
    }
    let w = u32::from_le_bytes(b[0..4].try_into().unwrap());
    let h = u32::from_le_bytes(b[4..8].try_into().unwrap());
    let fmt = u32::from_le_bytes(b[8..12].try_into().unwrap());
    if fmt != 1 || w == 0 || w > 8192 || h == 0 || h > 8192 {
        return None;
    }
    let px = (w as usize) * (h as usize) * 4;
    if b.len() == 16 + px {
        Some((w, h, 16))
    } else if b.len() >= 12 + px {
        Some((w, h, 12))
    } else {
        None
    }
}

/// `adb -e exec-out screencap` with a hard deadline — the child is
/// killed at [`CAPTURE_TIMEOUT_MS`] instead of blocking the frame
/// worker forever (a wedged guest capture never returns).
/// `stdout` is drained on a reader thread: the pipe buffer (~64 KiB)
/// would deadlock a ~4 MB frame if we only polled `try_wait`.
fn exec_out_capture(png: bool) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut cmd = super::silent_cmd(adb());
    cmd.args(["-e", "exec-out", "screencap"]);
    if png {
        cmd.arg("-p");
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("adb screencap: {e}"))?;
    let mut out = child.stdout.take().ok_or("screencap stdout")?;
    let reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out.read_to_end(&mut v);
        v
    });
    let budget = capture_deadline();
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let b = reader.join().unwrap_or_default();
                return Ok(b);
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return Err(format!(
                        "screencap timed out after {} ms",
                        budget.as_millis()
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = reader.join();
                return Err(format!("screencap wait: {e}"));
            }
        }
    }
}

/// `true` when the frame actually carries an image — early-boot and
/// slept-display captures are all-black; counting pixels brighter
/// than 16/255 and demanding ≥64 lit pixels keeps a black frame from
/// ever being presented as "the guest is up".
pub fn frame_is_visible(px: &[u8]) -> bool {
    px.as_chunks::<4>()
        .0
        .iter()
        .filter(|c| c[0] as u16 + c[1] as u16 + c[2] as u16 > 16)
        .take(64)
        .count()
        >= 64
}

/// Decode one raw `screencap` record → `(rgba, w, h)` or `None`.
/// Handles BOTH header layouts (12 B legacy, 16 B with `dataspace`),
/// both formats (`fmt=1` RGBA_8888, `fmt=5` BGRA_8888 → channel-swap),
/// forces alpha=255 on every pixel, and rejects all-black frames.
pub fn parse_frame(b: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    if b.len() < 12 {
        return None;
    }
    let w = u32::from_le_bytes(b[0..4].try_into().unwrap());
    let h = u32::from_le_bytes(b[4..8].try_into().unwrap());
    let fmt = u32::from_le_bytes(b[8..12].try_into().unwrap());
    if w == 0 || h == 0 || w > 8192 || h > 8192 || (fmt != 1 && fmt != 5) {
        return None;
    }
    let px = (w as usize) * (h as usize) * 4;
    let off = if b.len() == 16 + px {
        16
    } else if b.len() >= 12 + px {
        12
    } else {
        return None;
    };
    let mut rgba = b[off..off + px].to_vec();
    if fmt == 5 {
        // BGRA_8888 → RGBA_8888.
        for c in rgba.as_chunks_mut::<4>().0 {
            c.swap(0, 2);
        }
    }
    for c in rgba.as_chunks_mut::<4>().0 {
        c[3] = 255;
    }
    if !frame_is_visible(&rgba) {
        return None;
    }
    Some((rgba, w, h))
}

/// Grab one live frame via `adb -e exec-out screencap` (raw RGBA —
/// zero PNG cost). Falls back to GDI+ decode when the output already
/// IS a PNG. All-black frames surface as `Err` so the caller keeps
/// showing "waiting for display" instead of a black screen.
pub fn grab_frame() -> Result<(Vec<u8>, u32, u32), String> {
    let b = exec_out_capture(false)?;
    if let Some(f) = parse_frame(&b) {
        return Ok(f);
    }
    if b.starts_with(b"\x89PNG") {
        return png_to_rgba(&b);
    }
    // Valid screencap header with all-black pixels after boot means
    // the guest display dozed or SystemUI restarted on keyguard — wake
    // it automatically so the stream recovers on the next frame.
    if guest_booted() && parse_screencap_header(&b).is_some() {
        wake_guest();
    }
    Err(format!(
        "screencap failed: len={} head={:02x?}",
        b.len(),
        &b[..b.len().min(16)]
    ))
}

/// Explicit PNG path — `adb -e exec-out screencap -p` decoded via
/// GDI+. Used after repeated raw-capture failures so SOMETHING always
/// reaches the screen.
pub fn grab_frame_png() -> Result<(Vec<u8>, u32, u32), String> {
    let b = exec_out_capture(true)?;
    png_to_rgba(&b)
}

/// Save an RGBA8888 frame to `%USERPROFILE%\Pictures\GorillaLuxe` (or
/// `%LOCALAPPDATA%\GorillaLuxe\screenshots`) as a 32-bpp BMP file.
/// Returns the saved path.
pub fn save_screenshot_bmp(rgba: &[u8], w: u32, h: u32) -> Result<PathBuf, String> {
    if w == 0 || h == 0 || rgba.len() < (w as usize) * (h as usize) * 4 {
        return Err("no guest frame available".into());
    }
    let dir = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .map(|p| p.join("Pictures").join("GorillaLuxe"))
        .unwrap_or_else(|| data_dir().join("screenshots"));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("GorillaLuxe_{ts}.bmp"));
    let px_bytes = w * h * 4;
    let file_size = 54 + px_bytes;
    let mut bmp = Vec::with_capacity(file_size as usize);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&file_size.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
    bmp.extend_from_slice(&(w as i32).to_le_bytes());
    bmp.extend_from_slice(&(-(h as i32)).to_le_bytes()); // top-down
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&32u16.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&px_bytes.to_le_bytes());
    bmp.extend_from_slice(&2835i32.to_le_bytes());
    bmp.extend_from_slice(&2835i32.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    for c in rgba[..(w as usize) * (h as usize) * 4].as_chunks::<4>().0 {
        bmp.extend_from_slice(&[c[2], c[1], c[0], 255]); // BGRA
    }
    std::fs::write(&path, &bmp).map_err(|e| e.to_string())?;
    Ok(path)
}

/// HTTP/2 `h2c` client-streaming connection to
/// `/android.emulation.control.EmulatorController/streamInputEvent`
/// on `127.0.0.1:8554` — the SAME input path the emulator's own Qt
/// window uses. Each `InputEvent` is a small DATA frame written
/// directly onto the open stream: sub-millisecond dispatch into the
/// guest's virtio-input devices, no console `event send` quirks, no
/// `sendevent` permission issues on `ro.build.type=user` images, and
/// pixel coordinates (no 0..32767 scaling).
pub struct GrpcInputClient {
    stream: std::net::TcpStream,
    /// Last pointer position — a lift reuses it (MouseEvent lifts
    /// "pointer 0", the coords are informational on release).
    last: (u32, u32),
}

fn pb_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(b);
            return;
        }
        buf.push(b | 0x80);
    }
}

fn pb_int(buf: &mut Vec<u8>, field: u32, v: i64) {
    pb_varint(buf, (field as u64) << 3);
    pb_varint(buf, v as u64); // int32/int64 varint (sign-extended)
}

fn pb_msg(buf: &mut Vec<u8>, field: u32, inner: &[u8]) {
    pb_varint(buf, ((field as u64) << 3) | 2);
    pb_varint(buf, inner.len() as u64);
    buf.extend_from_slice(inner);
}

fn pb_str(buf: &mut Vec<u8>, field: u32, s: &str) {
    pb_msg(buf, field, s.as_bytes());
}

impl GrpcInputClient {
    const STREAM_SID: u32 = 1;

    pub fn connect() -> Result<Self, String> {
        use std::io::Write;
        let mut stream = std::net::TcpStream::connect("127.0.0.1:8554")
            .map_err(|e| format!("grpc input 8554: {e}"))?;
        stream.set_nodelay(true).ok();
        stream
            .set_write_timeout(Some(std::time::Duration::from_millis(1500)))
            .ok();
        stream
            .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
            .and_then(|_| stream.write_all(&[0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]))
            .map_err(|e| format!("grpc input preface: {e}"))?;

        const PATH: &[u8] = b"/android.emulation.control.EmulatorController/streamInputEvent";
        const AUTH: &[u8] = b"127.0.0.1:8554";
        const CTYPE: &[u8] = b"application/grpc";
        let mut hpack = Vec::with_capacity(140);
        hpack.push(0x83);
        hpack.push(0x86);
        hpack.push(0x04);
        hpack.push(PATH.len() as u8);
        hpack.extend_from_slice(PATH);
        hpack.push(0x01);
        hpack.push(AUTH.len() as u8);
        hpack.extend_from_slice(AUTH);
        hpack.extend_from_slice(&[0x0f, 0x10]);
        hpack.push(CTYPE.len() as u8);
        hpack.extend_from_slice(CTYPE);
        hpack.extend_from_slice(b"\x00\x02te\x08trailers");
        let hlen = hpack.len() as u32;
        let sid = Self::STREAM_SID;
        let mut req = Vec::with_capacity(9 + hpack.len());
        req.extend_from_slice(&[(hlen >> 16) as u8, (hlen >> 8) as u8, hlen as u8]);
        req.push(0x01); // HEADERS
        req.push(0x04); // END_HEADERS — NO END_STREAM: client-streaming
        req.extend_from_slice(&sid.to_be_bytes());
        req.extend_from_slice(&hpack);
        stream
            .write_all(&req)
            .and_then(|_| stream.flush())
            .map_err(|e| format!("grpc input headers: {e}"))?;

        // The server replies only when the stream closes, but it does
        // emit SETTINGS / PING / WINDOW_UPDATE that must be drained —
        // a dedicated thread ACKs control frames and discards the rest
        // so the recv buffer never fills during long sessions.
        if let Ok(mut drain) = stream.try_clone() {
            drain.set_read_timeout(None).ok();
            std::thread::spawn(move || {
                use std::io::Read;
                let mut fh = [0u8; 9];
                loop {
                    if drain.read_exact(&mut fh).is_err() {
                        return;
                    }
                    let flen =
                        ((fh[0] as usize) << 16) | ((fh[1] as usize) << 8) | (fh[2] as usize);
                    let ftype = fh[3];
                    let flags = fh[4];
                    // HEADERS/RST on the input stream carry grpc-status
                    // trailers — dump them so rejected events are
                    // diagnosable (INVALID_ARGUMENT reasons etc.).
                    if ftype == 0x01 || ftype == 0x09 || ftype == 0x03 {
                        let mut payload = vec![0u8; flen];
                        if drain.read_exact(&mut payload).is_err() {
                            return;
                        }
                        let mut dump = String::new();
                        for b in &payload {
                            if b.is_ascii_graphic() || *b == b' ' {
                                dump.push(*b as char);
                            } else {
                                dump.push_str(&format!("<{b:02x}>"));
                            }
                        }
                        log_line(&format!("grpc input stream ctrl t={ftype} [{dump}]"));
                        continue;
                    }
                    let mut payload = vec![0u8; flen.min(16384)];
                    let mut left = flen;
                    while left > 0 {
                        let chunk = left.min(payload.len());
                        let n = match drain.read(&mut payload[..chunk]) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        left -= n;
                    }
                    match (ftype, flags & 0x01 == 0) {
                        (0x04, true) => {
                            let _ = drain
                                .write_all(&[0, 0, 0, 4, 1, 0, 0, 0, 0])
                                .and_then(|_| drain.flush());
                        }
                        (0x06, true) => {
                            // PING -> ACK echoes the 8-byte opaque
                            // payload verbatim (RFC 7540 §6.7).
                            let mut pong = [0u8; 17];
                            pong[0..5].copy_from_slice(&[0, 0, 8, 6, 1]);
                            if flen == 8 {
                                pong[9..17].copy_from_slice(&payload[..8]);
                            }
                            let _ = drain.write_all(&pong).and_then(|_| drain.flush());
                        }
                        _ => {}
                    }
                }
            });
        }
        Ok(GrpcInputClient {
            stream,
            last: (0, 0),
        })
    }

    /// Serialize `msg` into a gRPC DATA frame on the open input stream.
    fn send_event(&mut self, msg: &[u8]) -> Result<(), String> {
        use std::io::Write;
        let sid = Self::STREAM_SID;
        let glen = (5 + msg.len()) as u32;
        let mut f = Vec::with_capacity(9 + 5 + msg.len());
        f.extend_from_slice(&[(glen >> 16) as u8, (glen >> 8) as u8, glen as u8]);
        f.push(0x00); // DATA
        f.push(0x00); // no END_STREAM — the stream stays open
        f.extend_from_slice(&sid.to_be_bytes());
        f.push(0x00); // gRPC: uncompressed
        f.extend_from_slice(&(msg.len() as u32).to_be_bytes());
        f.extend_from_slice(msg);
        self.stream
            .write_all(&f)
            .and_then(|_| self.stream.flush())
            .map_err(|e| format!("grpc input write: {e}"))
    }

    /// MouseEvent → InputEvent.mouse_event (field 3): emulates a
    /// single-finger capacitive touch. `buttons` bit0 = primary button.
    /// Scale guest display pixels to the sender's domain. The
    /// emulator maps incoming Touch.x/y onto the MT ABS axes using
    /// the DISPLAY size — i.e. it expects plain pixel coordinates
    /// (verified: 0..32767 input arrives unmapped; pixel input does).
    fn scale(&self, x: u32, y: u32) -> (u32, u32) {
        let lcd = guest_lcd();
        (x.min(lcd.0), y.min(lcd.1))
    }

    /// TouchEvent → InputEvent.touch_event (field 2): a Protocol-B
    /// MT contact. `pressure > 0` = down/move, `pressure = 0` = lift.
    /// (MouseEvent was verified live to emit PRESSURE/TRACKING_ID
    /// without ABS_MT_POSITION on headless builds — TouchEvent is
    /// the documented touch path and carries explicit coordinates.)
    fn touch(&mut self, x: u32, y: u32, pressure: i32) -> Result<(), String> {
        let (sx, sy) = self.scale(x, y);
        let mut t = Vec::with_capacity(24);
        pb_int(&mut t, 1, sx as i64); // x
        pb_int(&mut t, 2, sy as i64); // y
        pb_int(&mut t, 3, 1); // identifier
        pb_int(&mut t, 4, pressure as i64); // pressure
        pb_int(&mut t, 5, 60); // touch_major
        let mut te = Vec::with_capacity(t.len() + 4);
        pb_msg(&mut te, 1, &t); // touches
        let mut ie = Vec::with_capacity(te.len() + 4);
        pb_msg(&mut ie, 2, &te); // touch_event
        self.send_event(&ie)?;
        self.last = (x, y);
        Ok(())
    }

    pub fn mouse(&mut self, x: u32, y: u32, buttons: u32) -> Result<(), String> {
        self.touch(x, y, if buttons & 1 != 0 { 1024 } else { 0 })
    }

    pub fn down(&mut self, x: u32, y: u32) -> Result<(), String> {
        self.touch(x, y, 1024)
    }
    pub fn mv(&mut self, x: u32, y: u32) -> Result<(), String> {
        self.touch(x, y, 1024)
    }
    pub fn up(&mut self) -> Result<(), String> {
        let (x, y) = self.last;
        self.touch(x, y, 0)
    }
    pub fn tap(&mut self, x: u32, y: u32) -> Result<(), String> {
        self.touch(x, y, 1024)?;
        self.touch(x, y, 0)
    }

    /// WheelEvent → InputEvent.wheel_event (field 6). `dy` uses the
    /// 120-per-click convention (same units as WM_MOUSEWHEEL delta).
    pub fn wheel(&mut self, dx: i32, dy: i32) -> Result<(), String> {
        let mut we = Vec::with_capacity(12);
        pb_int(&mut we, 1, dx as i64);
        pb_int(&mut we, 2, dy as i64);
        let mut ie = Vec::with_capacity(we.len() + 4);
        pb_msg(&mut ie, 6, &we);
        self.send_event(&ie)
    }

    /// KeyboardEvent → InputEvent.key_event (field 1). `key` is a DOM
    /// key name ("Enter", "GoBack", "ArrowLeft", …); the emulator
    /// translates it to evdev via the chromium tables.
    pub fn key(&mut self, dom_key: &str) -> Result<(), String> {
        let mut ke = Vec::with_capacity(dom_key.len() + 8);
        pb_int(&mut ke, 2, 2); // eventType = keypress (down+up)
        pb_str(&mut ke, 4, dom_key);
        let mut ie = Vec::with_capacity(ke.len() + 4);
        pb_msg(&mut ie, 1, &ke);
        self.send_event(&ie)
    }

    /// KeyboardEvent.text — each printable ASCII char is translated to
    /// an evdev keypress. Non-ASCII characters are ignored server-side,
    /// so callers must keep an adb `input text`/clipboard fallback.
    pub fn text(&mut self, s: &str) -> Result<(), String> {
        if !s.is_ascii() {
            return Err("grpc text: non-ascii".into());
        }
        let mut ke = Vec::with_capacity(s.len() + 8);
        pb_str(&mut ke, 5, s);
        let mut ie = Vec::with_capacity(ke.len() + 4);
        pb_msg(&mut ie, 1, &ke);
        self.send_event(&ie)
    }
}

/// Direct HTTP/2 `h2c` gRPC client for `127.0.0.1:8554`
/// `/android.emulation.control.EmulatorController/streamScreenshot`.
///
/// Server-push `Image` stream straight from `gfxstream`'s host GPU
/// `ColorBuffer`: zero `adb.exe` spawns, 0% Android guest CPU. This is
/// the same RPC Android Studio uses for the embedded device view —
/// unlike `getScreenshot` (a "wait for next posted frame" long-poll
/// that pends forever once the Qt window is hidden), opening the
/// stream registers the client as a display consumer so the engine
/// keeps posting frames under `-qt-hide-window`.
pub struct GrpcScreenshotClient {
    stream: std::net::TcpStream,
    /// Leftover bytes of the in-flight gRPC message sequence on
    /// stream 1 — messages may split across DATA frames or share one.
    msg_buf: Vec<u8>,
    /// DATA bytes consumed since the last WINDOW_UPDATE on stream 1.
    credit: u32,
}

impl GrpcScreenshotClient {
    const STREAM_SID: u32 = 1;

    pub fn connect() -> Result<Self, String> {
        use std::io::Write;
        let mut stream = std::net::TcpStream::connect("127.0.0.1:8554")
            .map_err(|e| format!("grpc 8554: {e}"))?;
        stream.set_nodelay(true).ok();
        // The stream pushes frames only when the guest produces them;
        // an idle guest means silence — NOT a dead connection. A 5 s
        // read timeout turns that silence into `Ok(None)` so the pump
        // can check its own shutdown channel and loop back.
        stream
            .set_read_timeout(Some(std::time::Duration::from_millis(5000)))
            .ok();
        stream
            .set_write_timeout(Some(std::time::Duration::from_millis(2500)))
            .ok();
        // HTTP/2 client preface + SETTINGS(HEADER_TABLE_SIZE=0). A zero
        // header table forces the server to emit `grpc-status` as a
        // literal name instead of a dynamic-table index.
        stream
            .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
            .and_then(|_| stream.write_all(&[0, 0, 6, 4, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0]))
            .and_then(|_| {
                // Connection-level +1 GiB flow-control credit.
                stream.write_all(&[0, 0, 4, 8, 0, 0, 0, 0, 0, 0x40, 0, 0, 0])
            })
            .map_err(|e| format!("grpc preface: {e}"))?;

        // HPACK headers for POST …/streamScreenshot on stream 1.
        const PATH: &[u8] = b"/android.emulation.control.EmulatorController/streamScreenshot";
        const AUTH: &[u8] = b"127.0.0.1:8554";
        const CTYPE: &[u8] = b"application/grpc";
        let mut hpack = Vec::with_capacity(140);
        hpack.push(0x83); // :method: POST
        hpack.push(0x86); // :scheme: http
        hpack.push(0x04); // :path
        hpack.push(PATH.len() as u8);
        hpack.extend_from_slice(PATH);
        hpack.push(0x01); // :authority
        hpack.push(AUTH.len() as u8);
        hpack.extend_from_slice(AUTH);
        hpack.extend_from_slice(&[0x0f, 0x10]); // content-type (idx 31)
        hpack.push(CTYPE.len() as u8);
        hpack.extend_from_slice(CTYPE);
        hpack.extend_from_slice(b"\x00\x02te\x08trailers");

        let hlen = hpack.len() as u32;
        let sid = Self::STREAM_SID;
        let mut req = Vec::with_capacity(9 + hpack.len() + 9 + 7 + 13);
        req.extend_from_slice(&[(hlen >> 16) as u8, (hlen >> 8) as u8, hlen as u8]);
        req.push(0x01); // HEADERS
        req.push(0x04); // END_HEADERS
        req.extend_from_slice(&sid.to_be_bytes());
        req.extend_from_slice(&hpack);
        // DATA: 5-byte gRPC header + protobuf `ImageFormat { format:
        // RGBA8888 (1) }`, END_STREAM — request is complete, the
        // server then streams Images until we disconnect.
        req.extend_from_slice(&[0, 0, 7, 0x00, 0x01]);
        req.extend_from_slice(&sid.to_be_bytes());
        req.extend_from_slice(&[0x00, 0, 0, 0, 2, 0x08, 0x01]);
        // Stream-level +1 GiB credit — RGBA 1280x720 is ~3.6 MiB per
        // frame, far above the 64 KiB default stream window.
        req.extend_from_slice(&[0, 0, 4, 8, 0]);
        req.extend_from_slice(&sid.to_be_bytes());
        req.extend_from_slice(&[0x40, 0, 0, 0]);
        stream
            .write_all(&req)
            .and_then(|_| stream.flush())
            .map_err(|e| format!("grpc streamScreenshot: {e}"))?;

        Ok(GrpcScreenshotClient {
            stream,
            msg_buf: Vec::with_capacity(4 * 1024 * 1024),
            credit: 0,
        })
    }

    /// Pop one complete gRPC message (5-byte header + protobuf body)
    /// from the accumulated stream buffer.
    fn take_msg(&mut self) -> Option<Vec<u8>> {
        if self.msg_buf.len() < 5 {
            return None;
        }
        let mlen = u32::from_be_bytes(self.msg_buf[1..5].try_into().unwrap()) as usize;
        if self.msg_buf.len() < 5 + mlen {
            return None;
        }
        Some(self.msg_buf.drain(..5 + mlen).skip(5).collect())
    }

    /// Blocks until the server pushes the next `Image` frame.
    /// `Ok(None)` = read timeout while the guest screen is static —
    /// the stream stays open and the caller should just call again.
    pub fn grab_frame(&mut self) -> Result<Option<(Vec<u8>, u32, u32)>, String> {
        use std::io::{Read, Write};
        if let Some(msg) = self.take_msg() {
            if let Some(f) = parse_grpc_image(&msg) {
                return Ok(Some(f));
            }
        }
        let sid = Self::STREAM_SID;
        loop {
            let mut fh = [0u8; 9];
            match self.stream.read_exact(&mut fh) {
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut
                        || e.raw_os_error() == Some(10060) =>
                {
                    return Ok(None)
                }
                r => r.map_err(|e| format!("grpc read hdr: {e}"))?,
            }
            let flen = ((fh[0] as usize) << 16) | ((fh[1] as usize) << 8) | (fh[2] as usize);
            let ftype = fh[3];
            let flags = fh[4];
            let rsid = u32::from_be_bytes([fh[5] & 0x7f, fh[6], fh[7], fh[8]]);
            let mut payload = vec![0u8; flen];
            if flen > 0 {
                match self.stream.read_exact(&mut payload) {
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut
                            || e.raw_os_error() == Some(10060) =>
                    {
                        return Ok(None)
                    }
                    r => r.map_err(|e| format!("grpc read body: {e}"))?,
                }
            }
            match ftype {
                0x00 => {
                    // DATA
                    self.credit = self.credit.saturating_add(flen as u32);
                    if self.credit >= 8 * 1024 * 1024 {
                        let mut wu = Vec::with_capacity(26);
                        wu.extend_from_slice(&[0, 0, 4, 8, 0, 0, 0, 0, 0]);
                        wu.extend_from_slice(&self.credit.to_be_bytes());
                        wu.extend_from_slice(&[0, 0, 4, 8, 0]);
                        wu.extend_from_slice(&sid.to_be_bytes());
                        wu.extend_from_slice(&self.credit.to_be_bytes());
                        let _ = self.stream.write_all(&wu);
                        self.credit = 0;
                    }
                    if rsid == sid {
                        if flags & 0x08 != 0 {
                            let pad = payload[0] as usize;
                            if pad < payload.len() {
                                self.msg_buf
                                    .extend_from_slice(&payload[1..payload.len() - pad]);
                            }
                        } else {
                            self.msg_buf.extend_from_slice(&payload);
                        }
                        if flags & 0x01 != 0 {
                            return Err("grpc stream END_STREAM".into());
                        }
                        while let Some(msg) = self.take_msg() {
                            if let Some(f) = parse_grpc_image(&msg) {
                                return Ok(Some(f));
                            }
                        }
                    }
                }
                0x01 | 0x09 if rsid == sid => {
                    // HEADERS / CONTINUATION on our stream. END_STREAM
                    // here means the server closed the stream — report
                    // the trailers so the caller can reconnect.
                    if flags & 0x01 != 0 {
                        let mut dump = String::new();
                        for b in &payload {
                            if b.is_ascii_graphic() || *b == b' ' {
                                dump.push(*b as char);
                            } else {
                                dump.push_str(&format!("<{b:02x}>"));
                            }
                        }
                        return Err(format!("grpc stream closed [{dump}]"));
                    }
                }
                0x04 if flags & 0x01 == 0 => {
                    // Server SETTINGS -> ACK
                    let _ = self.stream.write_all(&[0, 0, 0, 4, 1, 0, 0, 0, 0]);
                }
                0x06 if flags & 0x01 == 0 && payload.len() == 8 => {
                    // PING -> ACK
                    let mut pong = [0u8; 17];
                    pong[0..5].copy_from_slice(&[0, 0, 8, 6, 1]);
                    pong[9..17].copy_from_slice(&payload);
                    let _ = self.stream.write_all(&pong);
                }
                0x03 if rsid == sid => return Err("grpc RST_STREAM".into()),
                0x07 => return Err("grpc GOAWAY".into()),
                _ => {}
            }
        }
    }
}

fn read_varint(b: &[u8], pos: &mut usize) -> Option<u64> {
    let mut val = 0u64;
    let mut shift = 0u32;
    while *pos < b.len() && shift < 64 {
        let byte = b[*pos];
        *pos += 1;
        val |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(val);
        }
        shift += 7;
    }
    None
}

/// Parse one protobuf `Image` message (gRPC prefix already stripped).
/// Returns `None` for the empty "display inactive" marker frames the
/// stream emits when the guest screen is off (w=0/h=0, no pixels).
/// Pixel order is RGBA8888 **bottom-up** — rows are flipped to the
/// top-down layout the DX12 uploader expects.
fn parse_grpc_image(msg: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut pos = 0usize;
    let mut w = 0u32;
    let mut h = 0u32;
    let mut img_slice: Option<&[u8]> = None;
    while pos < msg.len() {
        let Some(tag) = read_varint(msg, &mut pos) else {
            break;
        };
        let field = tag >> 3;
        let wire = tag & 7;
        match (field, wire) {
            (1, 2) => {
                // format: ImageFormat submessage
                let Some(len) = read_varint(msg, &mut pos) else {
                    break;
                };
                let end = (pos + len as usize).min(msg.len());
                let mut sp = pos;
                while sp < end {
                    let Some(stag) = read_varint(msg, &mut sp) else {
                        break;
                    };
                    match (stag >> 3, stag & 7) {
                        (3, 0) => {
                            if let Some(v) = read_varint(msg, &mut sp) {
                                if w == 0 {
                                    w = v as u32;
                                }
                            }
                        }
                        (4, 0) => {
                            if let Some(v) = read_varint(msg, &mut sp) {
                                if h == 0 {
                                    h = v as u32;
                                }
                            }
                        }
                        (_, 0) => {
                            let _ = read_varint(msg, &mut sp);
                        }
                        (_, 2) => {
                            if let Some(l) = read_varint(msg, &mut sp) {
                                sp = (sp + l as usize).min(end);
                            }
                        }
                        _ => break,
                    }
                }
                pos = end;
            }
            (2, 0) => {
                if let Some(v) = read_varint(msg, &mut pos) {
                    if v > 0 {
                        w = v as u32;
                    }
                }
            }
            (3, 0) => {
                if let Some(v) = read_varint(msg, &mut pos) {
                    if v > 0 {
                        h = v as u32;
                    }
                }
            }
            (4, 2) => {
                let Some(len) = read_varint(msg, &mut pos) else {
                    break;
                };
                let end = (pos + len as usize).min(msg.len());
                img_slice = Some(&msg[pos..end]);
                pos = end;
            }
            (_, 0) => {
                let _ = read_varint(msg, &mut pos);
            }
            (_, 1) => pos = (pos + 8).min(msg.len()),
            (_, 2) => {
                if let Some(l) = read_varint(msg, &mut pos) {
                    pos = (pos + l as usize).min(msg.len());
                }
            }
            (_, 5) => pos = (pos + 4).min(msg.len()),
            _ => break,
        }
    }
    // Empty Image = "display inactive" marker — not a real frame.
    let raw = img_slice?;
    if w == 0 || h == 0 || raw.is_empty() {
        return None;
    }
    let row = w as usize * 4;
    let expected = row * h as usize;
    if raw.len() != expected {
        log_line(&format!(
            "grpc Image size {} != {expected} ({w}x{h}) — skipping",
            raw.len()
        ));
        return None;
    }
    // Bottom-up RGBA → top-down with alpha forced opaque.
    let mut rgba = vec![0u8; expected];
    for y in 0..h as usize {
        let src = &raw[(h as usize - 1 - y) * row..(h as usize - y) * row];
        rgba[y * row..(y + 1) * row].copy_from_slice(src);
    }
    for c in rgba.as_chunks_mut::<4>().0 {
        c[3] = 255;
    }
    if !frame_is_visible(&rgba) {
        return None;
    }
    Some((rgba, w, h))
}

/// PNG bytes → RGBA via GDI+ (fallback path only).
fn png_to_rgba(png: &[u8]) -> Result<(Vec<u8>, u32, u32), String> {
    unsafe {
        let mut token: usize = 0;
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            DebugEventCallback: 0,
            SuppressBackgroundThread: false.into(),
            SuppressExternalCodecs: false.into(),
        };
        if GdiplusStartup(&mut token, &input, std::ptr::null_mut()).0 != 0 {
            return Err("GdiplusStartup failed".into());
        }
        let result: Result<(Vec<u8>, u32, u32), String> = (|| {
            let stream = windows::Win32::UI::Shell::SHCreateMemStream(Some(png))
                .ok_or_else(|| "SHCreateMemStream".to_string())?;
            let mut bmp: *mut GpBitmap = std::ptr::null_mut();
            if GdipCreateBitmapFromStream(&stream, &mut bmp).0 != 0 || bmp.is_null() {
                return Err("GdipCreateBitmapFromStream".to_string());
            }
            let mut w = 0u32;
            let mut h = 0u32;
            GdipGetImageWidth(bmp as *mut GpImage, &mut w);
            GdipGetImageHeight(bmp as *mut GpImage, &mut h);
            const PF_ARGB32: i32 = 0x0026_200A; // PixelFormat32bppARGB
            let rect = Rect {
                X: 0,
                Y: 0,
                Width: w as i32,
                Height: h as i32,
            };
            let mut data = BitmapData::default();
            if GdipBitmapLockBits(bmp, &rect, ImageLockModeRead.0 as u32, PF_ARGB32, &mut data).0
                != 0
            {
                GdipDisposeImage(bmp as *mut GpImage);
                return Err("GdipBitmapLockBits".to_string());
            }
            let mut rgba = vec![0u8; (w * h * 4) as usize];
            for y in 0..h as usize {
                let src = (data.Scan0 as *const u8).add(y * data.Stride as usize);
                let dst = rgba.as_mut_ptr().add(y * w as usize * 4);
                for x in 0..w as usize {
                    let s = src.add(x * 4);
                    let d = dst.add(x * 4);
                    *d = *s.add(2);
                    *d.add(1) = *s.add(1);
                    *d.add(2) = *s;
                    *d.add(3) = *s.add(3);
                }
            }
            GdipBitmapUnlockBits(bmp, &mut data);
            GdipDisposeImage(bmp as *mut GpImage);
            Ok((rgba, w, h))
        })();
        GdiplusShutdown(token);
        result
    }
}

/// Quote a path for a command line — production helper for the
/// `D:\GLXE test\` space-containing install dirs (selftest).
pub fn cmdline_quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.contains(' ') {
        format!("\"{s}\"")
    } else {
        s.into_owned()
    }
}

// ---------------------------------------------------------------------------
// High-FPS frame stream — ONE adb process total, zero per-frame spawns
// ---------------------------------------------------------------------------

/// Persistent `adb exec-out "while true; do screencap; done"` pipe.
/// The guest pushes back-to-back `[u32 w][u32 h][u32 fmt][pixels]`
/// records over a single stdout channel — no `adb.exe` process is
/// ever created per frame (~10 FPS spawn-limited → display-limited).
/// Android 12+ appends `dataspace: u32` (16-byte header) — detected
/// once by peeking at the NEXT record's header.
pub struct FrameStream {
    child: Child,
    /// Byte chunks produced by a reader thread that owns
    /// `ChildStdout` — the blocking `read` lives over THERE so this
    /// side can enforce a stall deadline.
    rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    buf: Vec<u8>,
    /// Record header size: 0 = undetected, else 12 (Android ≤11) or
    /// 16 (Android 12+, with trailing `dataspace`).
    hdr_size: usize,
    /// Bytes of the NEXT record already consumed by an over-read.
    pending: Vec<u8>,
}

impl FrameStream {
    /// `interval_ms` throttles the guest-side capture loop (a toybox
    /// `sleep` between screencaps) so the pipe never floods a weak
    /// host CPU — e.g. 33 ms for a 30 FPS cap.
    pub fn open(interval_ms: u64) -> Result<Self, String> {
        let script = format!(
            "while true; do screencap; sleep {}; done",
            interval_ms as f64 / 1000.0
        );
        let mut child = super::silent_cmd(adb())
            .args(["exec-out", &script])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("frame stream: {e}"))?;
        let mut stdout = child.stdout.take().ok_or("frame stream stdout")?;
        let (tx, rx) = std::sync::mpsc::channel();
        // The reader thread is the ONLY one allowed to block on the
        // anonymous pipe; it forwards every chunk (or the terminal
        // EOF/error) so the consumer side can use `recv_timeout`.
        // When a stall is detected the stream is dropped → `child
        // .kill()` breaks the pipe → this thread exits on its own.
        std::thread::spawn(move || {
            use std::io::Read;
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => {
                        let _ = tx.send(Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "stream EOF",
                        )));
                        break;
                    }
                    Ok(n) => {
                        if tx.send(Ok(chunk[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        });
        Ok(FrameStream {
            child,
            rx,
            buf: Vec::new(),
            hdr_size: 0,
            pending: Vec::new(),
        })
    }

    fn drain_pending(&mut self, dst: &mut [u8]) -> usize {
        let take = self.pending.len().min(dst.len());
        dst[..take].copy_from_slice(&self.pending[..take]);
        self.pending.drain(..take);
        take
    }

    /// `read_exact` on an anonymous pipe blocks FOREVER when the
    /// guest's `screencap` wedges without dying (observed live: the
    /// stream went silent for >10 min while `adb shell` and one-shot
    /// captures kept working). A reader thread owns the blocking
    /// `read`; this side applies a [`STREAM_STALL_MS`] deadline and a
    /// producer-liveness check — Err → the caller drops the stream
    /// (Drop kills adb) and reopens a fresh pipe via the 2 s retry.
    fn fill_exact(&mut self, dst: &mut [u8]) -> Result<(), String> {
        use std::sync::mpsc::RecvTimeoutError;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(STREAM_STALL_MS);
        let mut filled = self.drain_pending(dst);
        while filled < dst.len() {
            // A dead producer never fills the pipe — bail early.
            if let Ok(Some(_)) = self.child.try_wait() {
                return Err("stream producer (adb) exited".into());
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(format!("stream stalled >{STREAM_STALL_MS} ms — reopening"));
            }
            match self.rx.recv_timeout(left) {
                Ok(Ok(chunk)) => {
                    self.pending.extend_from_slice(&chunk);
                    filled += self.drain_pending(&mut dst[filled..]);
                }
                Ok(Err(e)) => return Err(format!("stream read: {e}")),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    return Err(format!("stream stalled >{STREAM_STALL_MS} ms — reopening"));
                }
            }
        }
        Ok(())
    }

    /// First record: read header(12) + payload + 16 look-ahead bytes
    /// covering the NEXT record's header under both layouts; the one
    /// whose trailing header re-validates fixes `hdr_size` for the
    /// stream's lifetime.
    fn sniff_first_frame(&mut self) -> Result<(Vec<u8>, u32, u32), String> {
        let mut rec = vec![0u8; 12];
        self.fill_exact(&mut rec)
            .map_err(|e| format!("stream hdr: {e}"))?;
        let w = u32::from_le_bytes(rec[0..4].try_into().unwrap());
        let h = u32::from_le_bytes(rec[4..8].try_into().unwrap());
        let fmt = u32::from_le_bytes(rec[8..12].try_into().unwrap());
        if fmt != 1 || w == 0 || h == 0 || w > 8192 || h > 8192 {
            return Err(format!("stream bad hdr {w}x{h} fmt={fmt}"));
        }
        let n = (w as usize) * (h as usize) * 4;
        let mut extra = vec![0u8; n + 16];
        self.fill_exact(&mut extra)
            .map_err(|e| format!("stream px: {e}"))?;
        let hdr_ok = |b: &[u8]| -> bool {
            if b.len() < 12 {
                return false;
            }
            let w2 = u32::from_le_bytes(b[0..4].try_into().unwrap());
            let h2 = u32::from_le_bytes(b[4..8].try_into().unwrap());
            let f2 = u32::from_le_bytes(b[8..12].try_into().unwrap());
            w2 == w && h2 == h && f2 == 1
        };
        let (hdr_size, px_off) = if hdr_ok(&extra[n..n + 12]) {
            (12usize, 0usize) // next hdr right after 12+n -> 12B layout
        } else if hdr_ok(&extra[n + 4..n + 16]) {
            (16usize, 4usize) // 4-byte dataspace shifted it -> 16B
        } else {
            (12usize, 0usize) // undetermined — assume legacy layout
        };
        self.hdr_size = hdr_size;
        // Everything past this record is the start of the next one.
        let record_len = hdr_size + n;
        let consumed = 12 + extra.len();
        self.pending = extra[record_len - 12..consumed - 12].to_vec();
        Ok((extra[px_off..px_off + n].to_vec(), w, h))
    }

    /// Block for the next `[w,h,fmt,(dataspace),rgba]` record.
    /// Err = pipe dead (caller reopens or falls back to per-frame
    /// `grab_frame`). The pixel buffer is moved out — zero per-frame
    /// memcpy; the allocator reuses the freed block on the next
    /// `resize`.
    pub fn next_frame(&mut self) -> Result<(Vec<u8>, u32, u32), String> {
        if self.hdr_size == 0 {
            return self.sniff_first_frame();
        }
        let mut hdr = [0u8; 16];
        self.fill_exact(&mut hdr[..self.hdr_size])?;
        let w = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let h = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
        let fmt = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
        if fmt != 1 || w == 0 || h == 0 || w > 8192 || h > 8192 {
            return Err(format!("stream bad hdr {w}x{h} fmt={fmt}"));
        }
        let n = (w as usize) * (h as usize) * 4;
        let mut buf = std::mem::take(&mut self.buf);
        buf.resize(n, 0);
        self.fill_exact(&mut buf)?;
        Ok((buf, w, h))
    }
}

impl Drop for FrameStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

// ---------------------------------------------------------------------------
// Low-latency input — sendevent (eng builds) > persistent `input`
// ---------------------------------------------------------------------------

/// QEMU human monitor on 127.0.0.1:4444 — wired by
/// `-qemu -monitor tcp:…,server,nowait`. Used ONLY for `quit`
/// (graceful shutdown → QuickBoot snapshot save); the HMP
/// `mouse_*`/`sendkey` family was verified not to reach the ranchu
/// guest (no PS/2-class device behind the handlers in this build).
pub struct QemuMonitor {
    stream: std::net::TcpStream,
}

fn console_auth_token() -> Result<String, String> {
    let token = [
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .map(|p| p.join(".emulator_console_auth_token")),
        Some(runtime_dir().join(".emulator_console_auth_token")),
    ]
    .into_iter()
    .flatten()
    .find_map(|p| std::fs::read_to_string(p).ok())
    .map(|s| s.trim().to_string())
    .unwrap_or_default();
    if token.is_empty() {
        Err("no .emulator_console_auth_token".into())
    } else {
        Ok(token)
    }
}

/// One-shot emulator-console command (TCP :5554, token auth).
/// Drains both the initial banner `OK` AND the `auth` `OK` before
/// sending `line` — otherwise the caller would match `auth`'s `OK` in
/// <1 ms and close the socket while `avd snapshot save` is still
/// mid-write.
fn emu_console_cmd(line: &str, reply_budget_ms: u64) -> Result<String, String> {
    use std::io::{BufRead, BufReader, Write};
    let mut stream =
        std::net::TcpStream::connect("127.0.0.1:5554").map_err(|e| format!("console 5554: {e}"))?;
    stream.set_nodelay(true).ok();
    stream
        .set_read_timeout(Some(std::time::Duration::from_millis(
            reply_budget_ms.max(3000),
        )))
        .ok();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    let mut br = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut l = String::new();
    for _ in 0..12 {
        l.clear();
        match br.read_line(&mut l) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if l.trim_end() == "OK" {
                    break;
                }
            }
        }
    }
    let token = console_auth_token()?;
    writeln!(stream, "auth {token}").map_err(|e| e.to_string())?;
    stream.flush().ok();
    for _ in 0..12 {
        l.clear();
        match br.read_line(&mut l) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let t = l.trim_end();
                if t == "OK" {
                    break;
                }
                if t.starts_with("KO") {
                    return Err(format!("console auth: {t}"));
                }
            }
        }
    }
    writeln!(stream, "{line}").map_err(|e| e.to_string())?;
    stream.flush().ok();
    let deadline = Instant::now() + std::time::Duration::from_millis(reply_budget_ms);
    let mut out = String::new();
    loop {
        l.clear();
        match br.read_line(&mut l) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let t = l.trim_end();
                out.push_str(t);
                out.push('\n');
                if t == "OK" || t.starts_with("KO") {
                    break;
                }
                if Instant::now() > deadline {
                    break;
                }
            }
        }
    }
    Ok(out)
}

/// Persistent emulator console connection (`127.0.0.1:5554`) for
/// sub-millisecond mouse / touchpad / touch injection via
/// `event send EV_ABS:ABS_MT_... EV_SYN:0:0`.
///
/// Live verification against `getevent /dev/input/event2` and binary
/// inspection of `qemu-system-x86_64-headless.exe` revealed three
/// requirements for `-no-window` x86_64 ranchu:
/// 1. `EV_SYN` must use numeric code `0` (`EV_SYN:0:0`) — the string
///    alias `SYN_REPORT` is rejected with
///    `KO: invalid event code in 'EV_SYN:SYN_REPORT:0'`, which left
///    all touch events uncommitted in the kernel input buffer!
/// 2. `ABS_MT_POSITION_X` and `ABS_MT_POSITION_Y` must be scaled from
///    guest display pixels (`1280x720`) to `0..32767` (`event mouse`
///    calls `getMonitorRect` which returns `0x0` in `-no-window` mode
///    and collapses coordinates to `(0,0)`).
/// 3. `virtio_input_multi_touch_1` (`/dev/input/event2`) is a pure
///    MT-B device (`ABS_MT_SLOT:0`, `ABS_MT_TRACKING_ID`,
///    `ABS_MT_PRESSURE:1024`, `ABS_MT_TOUCH_MAJOR:4`, no `BTN_TOUCH`).
pub struct EmuConsole {
    stream: std::net::TcpStream,
    tracking_id: i32,
}

impl EmuConsole {
    pub fn connect() -> Result<Self, String> {
        use std::io::{BufRead, BufReader, Write};
        let mut stream = std::net::TcpStream::connect("127.0.0.1:5554")
            .map_err(|e| format!("console 5554: {e}"))?;
        stream.set_nodelay(true).ok();
        stream
            .set_read_timeout(Some(std::time::Duration::from_millis(1500)))
            .ok();
        stream
            .set_write_timeout(Some(std::time::Duration::from_millis(1500)))
            .ok();
        let mut br = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut l = String::new();
        for _ in 0..12 {
            l.clear();
            match br.read_line(&mut l) {
                Ok(0) | Err(_) => break,
                Ok(_) if l.trim_end() == "OK" => break,
                Ok(_) => {}
            }
        }
        let token = console_auth_token()?;
        writeln!(stream, "auth {token}").map_err(|e| e.to_string())?;
        stream.flush().ok();
        let mut authed = false;
        for _ in 0..12 {
            l.clear();
            match br.read_line(&mut l) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let t = l.trim_end();
                    if t == "OK" {
                        authed = true;
                        break;
                    }
                    if t.starts_with("KO") {
                        return Err(format!("console auth: {t}"));
                    }
                }
            }
        }
        if !authed {
            return Err("console 5554: no OK after auth".into());
        }
        // Probe `event send EV_SYN:0:0` to confirm the handler is live.
        writeln!(stream, "event send EV_SYN:0:0").map_err(|e| e.to_string())?;
        stream.flush().ok();
        l.clear();
        let _ = br.read_line(&mut l);
        if l.trim_end().starts_with("KO") {
            return Err(format!("event send probe: {}", l.trim_end()));
        }
        Ok(EmuConsole {
            stream,
            tracking_id: 1,
        })
    }

    fn scale(&self, x: u32, y: u32) -> (u32, u32) {
        // Re-read the atomic-cached LCD on every event: the console
        // often connects while adbd is still OFFLINE — the constructor
        // then latched the (1280,720) fallback and EVERY later tap
        // landed ~33% off on a 960x540 guest (verified via getevent:
        // tap (900,349) arrived as scaled 0x59FF = px 675). Once the
        // first `wm size` probe succeeds, all events re-scale right.
        let lcd = guest_lcd();
        (
            (x.saturating_mul(32767) / lcd.0.max(1)).min(32767),
            (y.saturating_mul(32767) / lcd.1.max(1)).min(32767),
        )
    }

    /// Non-blocking drain of accumulated `OK\r\n` replies so the
    /// socket recv buffer never fills during 60+ Hz drag streams.
    fn drain_replies(&mut self) {
        use std::io::Read;
        self.stream.set_nonblocking(true).ok();
        let mut buf = [0u8; 1024];
        while let Ok(n) = self.stream.read(&mut buf) {
            if n == 0 {
                break;
            }
        }
        self.stream.set_nonblocking(false).ok();
    }

    fn send_cmd(&mut self, cmd: &str) -> Result<(), String> {
        use std::io::Write;
        self.drain_replies();
        write!(self.stream, "{cmd}\r\n")
            .and_then(|_| self.stream.flush())
            .map_err(|e| e.to_string())?;
        self.drain_replies();
        Ok(())
    }

    /// Like `send_cmd` but reads one reply line and reports `KO`
    /// replies as errors — used by `event text`, which is optional in
    /// older engines and must fall back to `input text`.
    fn send_cmd_check(&mut self, cmd: &str) -> Result<(), String> {
        use std::io::{BufRead, BufReader, Write};
        self.drain_replies();
        write!(self.stream, "{cmd}\r\n")
            .and_then(|_| self.stream.flush())
            .map_err(|e| e.to_string())?;
        let mut br = BufReader::new(self.stream.try_clone().map_err(|e| e.to_string())?);
        let mut l = String::new();
        let _ = br.read_line(&mut l);
        let t = l.trim_end();
        if t.starts_with("KO") {
            return Err(format!("console: {t}"));
        }
        Ok(())
    }

    pub fn down(&mut self, x: u32, y: u32) -> Result<(), String> {
        self.tracking_id = self.tracking_id.wrapping_add(1).clamp(1, 60000);
        let id = self.tracking_id;
        let (sx, sy) = self.scale(x, y);
        self.send_cmd(&format!(
            "event send EV_ABS:ABS_MT_SLOT:0 EV_ABS:ABS_MT_TRACKING_ID:{id} EV_ABS:ABS_MT_POSITION_X:{sx} EV_ABS:ABS_MT_POSITION_Y:{sy} EV_ABS:ABS_MT_PRESSURE:1024 EV_ABS:ABS_MT_TOUCH_MAJOR:4 EV_SYN:0:0"
        ))
    }

    pub fn mv(&mut self, x: u32, y: u32) -> Result<(), String> {
        let (sx, sy) = self.scale(x, y);
        self.send_cmd(&format!(
            "event send EV_ABS:ABS_MT_SLOT:0 EV_ABS:ABS_MT_POSITION_X:{sx} EV_ABS:ABS_MT_POSITION_Y:{sy} EV_SYN:0:0"
        ))
    }

    pub fn up(&mut self) -> Result<(), String> {
        self.send_cmd(
            "event send EV_ABS:ABS_MT_SLOT:0 EV_ABS:ABS_MT_PRESSURE:0 EV_ABS:ABS_MT_TRACKING_ID:-1 EV_SYN:0:0",
        )
    }

    pub fn up_at(&mut self, _x: u32, _y: u32) -> Result<(), String> {
        self.up()
    }

    pub fn tap(&mut self, x: u32, y: u32) -> Result<(), String> {
        self.down(x, y)?;
        self.up()
    }

    pub fn key_tap(&mut self, key_name: &str) -> Result<(), String> {
        self.send_cmd(&format!(
            "event send EV_KEY:{key_name}:1 EV_SYN:0:0 EV_KEY:{key_name}:0 EV_SYN:0:0"
        ))
    }

    pub fn text(&mut self, s: &str) -> Result<(), String> {
        // `event text` takes the rest of the line verbatim — newlines
        // or CR would inject extra console commands, so strip them.
        let clean: String = s.chars().filter(|c| *c != '\r' && *c != '\n').collect();
        if clean.is_empty() {
            return Ok(());
        }
        self.send_cmd_check(&format!("event text {clean}"))
    }

    pub fn swipe(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, ms: u32) -> Result<(), String> {
        let steps = (ms / 16).clamp(4, 24) as i32;
        let step_sleep = std::time::Duration::from_millis((ms / steps as u32).max(8) as u64);
        self.down(x1.max(0) as u32, y1.max(0) as u32)?;
        for i in 1..=steps {
            std::thread::sleep(step_sleep);
            let x = (x1 + (x2 - x1) * i / steps).max(0) as u32;
            let y = (y1 + (y2 - y1) * i / steps).max(0) as u32;
            self.mv(x, y)?;
        }
        self.up()
    }
}

/// Android keycode → Linux `input-event-codes` name for the qwerty2
/// keyboard reachable through console `event send EV_KEY:<name>`.
/// Only keys that exist on a real keyboard are mapped — Android
/// system keys (BACK/HOME/APP_SWITCH) go through `input keyevent`.
pub fn android_to_linux_key(code: u32) -> Option<&'static str> {
    Some(match code {
        19 => "KEY_UP",
        20 => "KEY_DOWN",
        21 => "KEY_LEFT",
        22 => "KEY_RIGHT",
        61 => "KEY_TAB",
        62 => "KEY_SPACE",
        66 => "KEY_ENTER",
        67 => "KEY_BACKSPACE",
        92 => "KEY_PAGEUP",
        93 => "KEY_PAGEDOWN",
        111 => "KEY_ESC",
        112 => "KEY_DELETE",
        122 => "KEY_HOME",
        123 => "KEY_END",
        124 => "KEY_INSERT",
        _ => return None,
    })
}

/// One-shot sub-ms tap over emulator console `:5554` (safe from
/// background threads such as `dismiss_anr_dialogs`).
pub fn console_tap(x: u32, y: u32) -> Result<(), String> {
    let mut c = EmuConsole::connect()?;
    c.down(x, y)?;
    std::thread::sleep(std::time::Duration::from_millis(25));
    c.up()
}

impl QemuMonitor {
    pub const PORT: u16 = 4444;

    pub fn connect() -> Result<Self, String> {
        let stream = std::net::TcpStream::connect(("127.0.0.1", Self::PORT))
            .map_err(|e| format!("qemu monitor :{}: {e}", Self::PORT))?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_millis(300)))
            .ok();
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(2)))
            .ok();
        let mut m = QemuMonitor { stream };
        m.drain(); // "(qemu) " banner
        Ok(m)
    }

    /// Read-and-discard whatever the monitor emitted (banner, echoes)
    /// without blocking.
    fn drain(&mut self) {
        use std::io::Read;
        self.stream.set_nonblocking(true).ok();
        let mut buf = [0u8; 4096];
        for _ in 0..8 {
            match self.stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        self.stream.set_nonblocking(false).ok();
    }

    /// Clean qemu exit — saves the QuickBoot snapshot, unlike kill().
    pub fn quit(&mut self) -> Result<(), String> {
        use std::io::Write;
        writeln!(self.stream, "quit")
            .and_then(|_| self.stream.flush())
            .map_err(|e| e.to_string())
    }
}

/// Locate the guest touchscreen input node (`/dev/input/eventN`)
/// via `/proc/bus/input/devices` — the goldfish/ranchu "qwerty2"
/// device carries both keys and ABS_MT touch.
pub fn touch_input_node() -> Result<String, String> {
    let out = adb_run(&["shell", "cat", "/proc/bus/input/devices"])?;
    let mut name = String::new();
    for line in out.lines() {
        let l = line.trim();
        if let Some(n) = l.strip_prefix("N: Name=") {
            name = n.trim_matches('"').to_string();
        }
        if l.starts_with("H: Handlers=")
            && (name.contains("qwerty") || name.to_lowercase().contains("touch"))
        {
            if let Some(ev) = l.split_whitespace().find(|w| w.starts_with("event")) {
                return Ok(format!("/dev/input/{ev}"));
            }
        }
        if l.is_empty() {
            name.clear();
        }
    }
    Ok("/dev/input/event2".into()) // ranchu default
}

/// Unified injector ordered by latency and capability:
///
/// * `Grpc` — `streamInputEvent` mouse/wheel/key events over gRPC
///   `:8554` (<0.5 ms, the same path the Qt window uses — works on
///   `user` builds under `-qt-hide-window`).
/// * `Console` — `event send` batches over persistent emulator
///   console `127.0.0.1:5554`.
/// * `Events` — `sendevent` batches (eng/userdebug images only).
/// * `Shell` — `input tap`/`input swipe` via persistent `adb shell`.
pub enum FastInput {
    Grpc(GrpcInputClient),
    Console(EmuConsole),
    Events {
        node: String,
        tracking_id: i32,
        lcd: (u32, u32),
    },
    Shell,
}

/// `wm size` → (w,h); falls back to the spec profile LCD when the
/// guest is too early in boot to answer. A successful probe is
/// cached process-wide — the panel geometry never changes mid-boot,
/// and `EmuConsole::connect` calls this on EVERY `console_tap` (the
/// ANR dismisser would otherwise spawn `wm size` every ~4 s).
/// Packed w<<16|h in an atomic so there is no lock on the hot path.
/// CLEARED by [`reset_guest_state`] — a relaunch may land on a
/// different spec profile (e.g. software GPU → 960x540).
static GUEST_LCD: AtomicU64 = AtomicU64::new(0);

fn guest_lcd() -> (u32, u32) {
    let packed = GUEST_LCD.load(Ordering::Acquire);
    if packed != 0 {
        return ((packed >> 32) as u32, packed as u32);
    }
    if let Ok(out) = adb_run(&["shell", "wm", "size"]) {
        if let Some((w, h)) = out
            .split_whitespace()
            .last()
            .and_then(|s| s.split_once('x'))
        {
            if let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) {
                if w > 0 && h > 0 {
                    GUEST_LCD.store(((w as u64) << 32) | h as u64, Ordering::Release);
                    return (w, h);
                }
            }
        }
    }
    (1280, 720)
}

impl FastInput {
    pub fn connect() -> Result<Self, String> {
        // Console `event send` is the primary touch path — verified
        // end-to-end via getevent (full ABS_MT_POSITION/PRESSURE,
        // sub-ms). gRPC `streamInputEvent` is kept for keys/text/wheel
        // but its touch path is skin-bound and drops coordinates
        // under `-qt-hide-window`, so it never wins over the console.
        for attempt in 0..8 {
            match EmuConsole::connect() {
                Ok(c) => {
                    log_line("input path: emulator console :5554 (`event send` -> virtio-mt)");
                    return Ok(FastInput::Console(c));
                }
                Err(e) => {
                    if attempt == 7 {
                        log_line(&format!("console :5554 unavailable after 8 tries: {e}"));
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(1500));
                    }
                }
            }
        }
        match GrpcInputClient::connect() {
            Ok(g) => {
                log_line("input path: gRPC streamInputEvent :8554 (keys/wheel only — touch via skin is dead headless)");
                return Ok(FastInput::Grpc(g));
            }
            Err(e) => log_line(&format!("gRPC input :8554 unavailable: {e}")),
        }
        match touch_input_node() {
            Ok(node) => match adb_run(&["shell", "sendevent", &node, "0", "0", "0"]) {
                Ok(_) => {
                    log_line(&format!("input path: sendevent on {node}"));
                    return Ok(FastInput::Events {
                        node,
                        tracking_id: 1,
                        lcd: guest_lcd(),
                    });
                }
                Err(e) => log_line(&format!("sendevent probe failed: {e}")),
            },
            Err(e) => log_line(&format!("sendevent node probe failed: {e}")),
        }
        log_line("input path: persistent `input` fallback");
        Ok(FastInput::Shell)
    }

    /// Lazily upgrade `Shell` to `Console` if `:5554` came up late.
    /// (Grpc is NOT an upgrade target: its touch path is dead under
    /// a hidden window, so `Console`/`Events` always outrank it.)
    fn try_upgrade(&mut self) {
        if matches!(self, FastInput::Shell) {
            if let Ok(c) = EmuConsole::connect() {
                log_line("input path upgraded to emulator console :5554");
                *self = FastInput::Console(c);
            }
        }
    }

    /// gRPC write failure → drop to the next live tier and let the
    /// call retry through it.
    fn downgrade(&mut self) {
        log_line("gRPC input lost — downgrading to console/shell");
        if let Ok(c) = EmuConsole::connect() {
            *self = FastInput::Console(c);
        } else {
            *self = FastInput::Shell;
        }
    }

    pub fn is_realtime(&self) -> bool {
        matches!(
            self,
            FastInput::Grpc(_) | FastInput::Console(_) | FastInput::Events { .. }
        )
    }

    fn ev_line(node: &str, t: u32, c: u32, v: i32) -> String {
        format!("sendevent {node} {t} {c} {v}")
    }

    /// Touch/mouse DOWN at (x,y). `Grpc` touch is dead under
    /// `-qt-hide-window` — it immediately downgrades to the console
    /// tier before dispatching.
    pub fn down(&mut self, shell: Option<&mut AdbShell>, x: u32, y: u32) -> Result<(), String> {
        self.try_upgrade();
        if matches!(self, FastInput::Grpc(_)) {
            self.downgrade();
        }
        match self {
            FastInput::Grpc(_) => unreachable!(),
            FastInput::Console(c) => c.down(x, y),
            FastInput::Shell => shell
                .ok_or_else(|| "no adb shell".to_string())?
                .swipe(x as i32, y as i32, x as i32, y as i32, 600),
            FastInput::Events {
                node,
                tracking_id,
                lcd,
            } => {
                *tracking_id = tracking_id.wrapping_add(1).max(1);
                let (sx, sy) = (
                    x.saturating_mul(32767) / lcd.0.max(1),
                    y.saturating_mul(32767) / lcd.1.max(1),
                );
                let batch = [
                    Self::ev_line(node, 3, 0x39, *tracking_id),
                    Self::ev_line(node, 3, 0x35, sx as i32),
                    Self::ev_line(node, 3, 0x36, sy as i32),
                    Self::ev_line(node, 1, 0x14a, 1),
                    Self::ev_line(node, 0, 0, 0),
                ]
                .join(";");
                shell.ok_or_else(|| "no adb shell".to_string())?.cmd(&batch)
            }
        }
    }

    pub fn mv(&mut self, shell: Option<&mut AdbShell>, x: u32, y: u32) -> Result<(), String> {
        if matches!(self, FastInput::Grpc(_)) {
            self.downgrade();
        }
        match self {
            FastInput::Grpc(_) => unreachable!(),
            FastInput::Console(c) => c.mv(x, y),
            FastInput::Shell => Err("input path: no incremental move".into()),
            FastInput::Events { node, lcd, .. } => {
                let (sx, sy) = (
                    x.saturating_mul(32767) / lcd.0.max(1),
                    y.saturating_mul(32767) / lcd.1.max(1),
                );
                shell.ok_or_else(|| "no adb shell".to_string())?.cmd(
                    &[
                        Self::ev_line(node, 3, 0x35, sx as i32),
                        Self::ev_line(node, 3, 0x36, sy as i32),
                        Self::ev_line(node, 0, 0, 0),
                    ]
                    .join(";"),
                )
            }
        }
    }

    pub fn up(&mut self, shell: Option<&mut AdbShell>) -> Result<(), String> {
        if matches!(self, FastInput::Grpc(_)) {
            self.downgrade();
        }
        match self {
            FastInput::Grpc(_) => unreachable!(),
            FastInput::Console(c) => c.up(),
            FastInput::Shell => Ok(()),
            FastInput::Events { node, .. } => shell.ok_or_else(|| "no adb shell".to_string())?.cmd(
                &[
                    Self::ev_line(node, 1, 0x14a, 0),
                    Self::ev_line(node, 3, 0x39, -1),
                    Self::ev_line(node, 0, 0, 0),
                ]
                .join(";"),
            ),
        }
    }

    pub fn up_at(&mut self, shell: Option<&mut AdbShell>, _x: u32, _y: u32) -> Result<(), String> {
        self.up(shell)
    }

    /// Hardware-key press identified by an ANDROID keycode. gRPC maps
    /// common keys to DOM key names (`GoBack`, `Enter`, arrows…);
    /// console `EV_KEY` handles the same set on the qwerty2 device;
    /// everything else goes through `input keyevent` (~10 ms).
    pub fn key_event(&mut self, shell: Option<&mut AdbShell>, code: u32) -> Result<(), String> {
        if let FastInput::Grpc(g) = self {
            let dom = match code {
                3 => "GoHome",
                4 => "GoBack",
                19 => "ArrowUp",
                20 => "ArrowDown",
                21 => "ArrowLeft",
                22 => "ArrowRight",
                26 => "Power",
                61 => "Tab",
                62 => " ",
                66 => "Enter",
                67 => "Backspace",
                92 => "PageUp",
                93 => "PageDown",
                111 => "Escape",
                112 => "Delete",
                122 => "Home",
                123 => "End",
                187 => "AppSwitch",
                _ => "",
            };
            if !dom.is_empty() {
                match g.key(dom) {
                    Ok(()) => return Ok(()),
                    Err(_) => self.downgrade(),
                }
            }
        }
        if let FastInput::Console(c) = self {
            if let Some(name) = android_to_linux_key(code) {
                return c.key_tap(name);
            }
        }
        shell
            .ok_or_else(|| "no adb shell".to_string())?
            .keyevent(code)
    }

    /// Type a text string into the guest. gRPC uses KeyboardEvent.text
    /// (ASCII — sub-ms); console uses `event text`; fallback is the
    /// persistent shell's `input text`.
    pub fn text(&mut self, shell: Option<&mut AdbShell>, s: &str) -> Result<(), String> {
        if s.is_empty() {
            return Ok(());
        }
        if let FastInput::Grpc(g) = self {
            if s.is_ascii() {
                match g.text(s) {
                    Ok(()) => return Ok(()),
                    Err(_) => self.downgrade(),
                }
            }
        }
        if let FastInput::Console(c) = self {
            return c.text(s);
        }
        shell.ok_or_else(|| "no adb shell".to_string())?.text(s)
    }

    /// Mouse wheel — gRPC WheelEvent takes the raw 120-units-per-click
    /// delta straight from WM_MOUSEWHEEL. Other tiers approximate with
    /// a swipe at the cursor.
    pub fn wheel(
        &mut self,
        shell: Option<&mut AdbShell>,
        x: u32,
        y: u32,
        delta: i32,
    ) -> Result<(), String> {
        if let FastInput::Grpc(g) = self {
            match g.wheel(0, delta) {
                Ok(()) => return Ok(()),
                Err(_) => self.downgrade(),
            }
        }
        // Fallback: three-line scroll ≈ 180 px drag upward/downward.
        let dir = if delta > 0 { -1 } else { 1 };
        let dist = (delta.abs() / 40).clamp(60, 420);
        self.swipe(
            shell,
            x as i32,
            y as i32,
            x as i32,
            (y as i32 + dir * dist).max(0),
            300,
        )
    }

    pub fn swipe(
        &mut self,
        mut shell: Option<&mut AdbShell>,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        ms: u32,
    ) -> Result<(), String> {
        self.try_upgrade();
        if matches!(self, FastInput::Grpc(_)) {
            // Grpc touch is dead headless — downgrade to console, then
            // drive the swipe through down/mv/up on whatever tier won.
            self.downgrade();
            let steps = (ms / 16).clamp(4, 24) as i32;
            let step_sleep = std::time::Duration::from_millis((ms / steps as u32).max(8) as u64);
            self.down(shell.as_deref_mut(), x1.max(0) as u32, y1.max(0) as u32)?;
            for i in 1..=steps {
                std::thread::sleep(step_sleep);
                let x = (x1 + (x2 - x1) * i / steps).max(0) as u32;
                let y = (y1 + (y2 - y1) * i / steps).max(0) as u32;
                self.mv(shell.as_deref_mut(), x, y)?;
            }
            return self.up(shell);
        }
        if let FastInput::Console(c) = self {
            return c.swipe(x1, y1, x2, y2, ms);
        }
        shell
            .ok_or_else(|| "no adb shell".to_string())?
            .swipe(x1, y1, x2, y2, ms)
    }

    /// Full tap = down+up. Console is sub-ms; `Shell` falls back to
    /// `input tap`.
    pub fn tap(&mut self, mut shell: Option<&mut AdbShell>, x: u32, y: u32) -> Result<(), String> {
        self.try_upgrade();
        if matches!(self, FastInput::Grpc(_)) {
            self.downgrade();
        }
        match self {
            FastInput::Grpc(_) => unreachable!(),
            FastInput::Console(c) => c.tap(x, y),
            FastInput::Shell => shell.ok_or_else(|| "no adb shell".to_string())?.tap(x, y),
            FastInput::Events { .. } => {
                self.down(shell.as_deref_mut(), x, y)?;
                self.up(shell)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_frame, parse_screencap_header};

    fn cap(w: u32, h: u32, hdr: usize, dataspace: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&w.to_le_bytes());
        b.extend_from_slice(&h.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes()); // fmt = RGBA_8888
        if hdr == 16 {
            b.extend_from_slice(&dataspace.to_le_bytes());
        }
        // Sentinel first pixel — verifies the payload is not shifted
        // by a misparsed header length.
        b.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        b.resize(12.max(hdr) + (w as usize) * (h as usize) * 4, 0x7F);
        b
    }

    #[test]
    fn screencap_12b_and_16b_headers() {
        let (w, h) = (640u32, 480u32);
        // Android <=11: 12-byte header.
        let b12 = cap(w, h, 12, 0);
        let (pw, ph, off) = parse_screencap_header(&b12).unwrap();
        assert_eq!((pw, ph, off), (w, h, 12));
        assert_eq!(&b12[off..off + 4], &[0xDE, 0xAD, 0xBE, 0xEF]);
        // Android 12/13: same record + `dataspace` u32 -> 16-byte header.
        let b16 = cap(w, h, 16, 0x0000_0000);
        let (pw, ph, off) = parse_screencap_header(&b16).unwrap();
        assert_eq!((pw, ph, off), (w, h, 16));
        assert_eq!(&b16[off..off + 4], &[0xDE, 0xAD, 0xBE, 0xEF]);
        // Truncated payload rejected; bogus format rejected.
        assert!(parse_screencap_header(&b12[..b12.len() - 8]).is_none());
        let mut bad = b12.clone();
        bad[8..12].copy_from_slice(&9u32.to_le_bytes());
        assert!(parse_screencap_header(&bad).is_none());
    }

    /// Spec-mandated coverage for `parse_frame`: both header layouts,
    /// fmt=5 BGRA channel swap, forced alpha=255, and the all-black
    /// early-boot reject.
    #[test]
    fn parse_frame_formats_alpha_black() {
        let (w, h) = (64u32, 48u32);
        let px = (w * h) as usize;
        // Non-black RGBA record, 16-byte (Android 12/13) header.
        let mut b16 = Vec::new();
        b16.extend_from_slice(&w.to_le_bytes());
        b16.extend_from_slice(&h.to_le_bytes());
        b16.extend_from_slice(&1u32.to_le_bytes()); // fmt RGBA_8888
        b16.extend_from_slice(&1u32.to_le_bytes()); // dataspace
        for i in 0..px {
            b16.extend_from_slice(&[200, 100 + (i % 8) as u8, 50, 7]);
        }
        let (rgba, pw, ph) = parse_frame(&b16).expect("16B frame must parse");
        assert_eq!((pw, ph), (w, h));
        assert_eq!(rgba[0], 200);
        assert!(
            rgba.as_chunks::<4>().0.iter().all(|c| c[3] == 255),
            "alpha not forced"
        );

        // 12-byte header + BGRA_8888 (fmt=5) → channel swap.
        let mut b12 = Vec::new();
        b12.extend_from_slice(&w.to_le_bytes());
        b12.extend_from_slice(&h.to_le_bytes());
        b12.extend_from_slice(&5u32.to_le_bytes()); // fmt BGRA_8888
        for _ in 0..px {
            b12.extend_from_slice(&[10, 20, 200, 0]); // B=10 G=20 R=200 A=0
        }
        let (rgba2, ..) = parse_frame(&b12).expect("12B BGRA frame must parse");
        assert_eq!(rgba2[0], 200, "BGRA→RGBA red swap failed");
        assert_eq!(rgba2[2], 10, "BGRA→RGBA blue swap failed");
        assert_eq!(rgba2[3], 255);

        // All-black early-boot frame → rejected (stays on the
        // "waiting for display" chip instead of a black screen).
        let mut blk = Vec::new();
        blk.extend_from_slice(&w.to_le_bytes());
        blk.extend_from_slice(&h.to_le_bytes());
        blk.extend_from_slice(&1u32.to_le_bytes());
        blk.extend_from_slice(&1u32.to_le_bytes());
        blk.resize(16 + px * 4, 0);
        assert!(parse_frame(&blk).is_none(), "black frame must be rejected");
    }

    /// Real-device pipeline check — needs a RUNNING GorillaLuxe
    /// emulator on `emulator-5554` (manual validation only):
    /// `cargo test -p gorillaluxe-core --release -- --ignored live_`
    #[test]
    #[ignore = "requires a live Android emulator — manual validation"]
    fn live_android_frames_parse() {
        if !super::adb_exe().is_file() {
            eprintln!("runtime not installed — skipping live check");
            return;
        }
        let mut s = match super::FrameStream::open(0) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("no live emulator ({e}) — skipping");
                return;
            }
        };
        let t0 = std::time::Instant::now();
        let mut dims = None;
        for i in 0..5 {
            let (px, w, h) = s.next_frame().expect("next_frame failed on live device");
            assert_eq!(px.len(), (w as usize) * (h as usize) * 4);
            if let Some(d) = dims {
                assert_eq!(d, (w, h), "frame dims changed mid-stream");
            }
            dims = Some((w, h));
            eprintln!("live frame {i}: {w}x{h} ({} bytes)", px.len());
        }
        eprintln!("5 live frames in {:?}", t0.elapsed());
    }
}
