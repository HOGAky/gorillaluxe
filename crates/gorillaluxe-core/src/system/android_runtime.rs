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
    ///     the qcow2 device overlays `reboot -p` alone doesn't
    ///     (verified live: poweroff alone → next launch fails
    ///     "Device 'cache' does not have the requested snapshot" →
    ///     cold boot anyway);
    ///  2) `reboot -p` — guest powers off → clean userdata unmount +
    ///     qemu exit (RAM file-backs via -mem-path);
    ///  3) monitor `quit` — clean qemu exit even if adbd is wedged;
    ///  4) kill + Job close — last resort, never left orphaned.
    ///
    /// Total grace budget ~2 min: a ~2.5 GiB RAM snapshot + disk
    /// overlays take a while on laptop storage.
    pub fn shutdown(&mut self) {
        log_line("emulator shutdown: saving QuickBoot snapshot");
        match emu_console_cmd("avd snapshot save default_boot", 60_000) {
            Ok(r) => {
                if r.starts_with("KO") {
                    log_line(&format!("snapshot save failed: {}", r.trim()));
                } else {
                    log_line("QuickBoot snapshot saved");
                }
            }
            Err(e) => log_line(&format!("snapshot save: {e}")),
        }
        log_line("emulator shutdown: guest poweroff requested");
        let _ = super::silent_cmd(adb())
            .args(["shell", "reboot", "-p"])
            .output();
        if self.wait_exit(45_000) {
            log_line("emulator exited cleanly (guest poweroff)");
        } else {
            log_line("emulator still up after poweroff — monitor quit");
            if let Ok(mut m) = QemuMonitor::connect() {
                let _ = m.quit();
            }
            if self.wait_exit(30_000) {
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
    let _ = std::fs::write(avd.join(".glx_avd_v4"), b"landscape spec-profile v4\n");
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
    vec![
        "-avd".into(),
        "GorillaLuxe".into(),
        // `-no-window` is deliberate: we evaluated embedding Google's
        // Qt window (`-feature -Force32bit` / HWND reparenting) and it
        // is strictly worse — it bypasses the DX12+CAS compositor and
        // cannot be hit-tested/rotated with our input layer. The
        // persistent screencap pipe is the CAS-compatible path.
        "-no-window".into(),
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
        // gRPC without JWT = an unauthenticated local control port —
        // the engine itself warns "No gRPC protection active" in
        // emulator.log. JWT on; the token lands in the engine output
        // when a client is ever wired.
        "-grpc-use-jwt".into(),
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
pub fn gpu_chain(force_swiftshader: bool) -> Vec<&'static str> {
    if force_swiftshader {
        return vec!["swiftshader"];
    }
    vec!["host", "auto", "swiftshader"]
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
        let mut anr_tick = 0u32;
        let mut anr_ticks = 8u32;
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
                    // ARM64 support: the `-prop ro.dalvik.vm.native.bridge`
                    // launch arg is REJECTED by emulator ≥35 ("only
                    // 'qemu.*' properties are supported" in emulator.log)
                    // — the supported switch is the persist flag, set
                    // here before zygote spawns app processes. Best-effort:
                    // harmless on images without libndk_translation.
                    let _ = adb_run(&["-e", "shell", "setprop", "persist.sys.nativebridge", "1"]);
                    // Background-app ANR dialogs are pure noise for a
                    // consumer product — the app watchdog already kills
                    // them silently when this is 0. Foreground ANRs
                    // (SystemUI) still show and are handled by
                    // dismiss_anr_dialogs.
                    let _ = adb_run(&[
                        "-e",
                        "shell",
                        "settings",
                        "put",
                        "global",
                        "anr_show_background",
                        "0",
                    ]);
                    log_line("guest wake + keyguard dismiss sent");
                    woke = true;
                }
                // ~15 s cadence — `dumpsys window windows` itself costs
                // guest CPU; running it every 2 s poll would feed the
                // ANR pressure it is meant to clear.
                anr_tick += 1;
                if BOOT_COMPLETED.load(Ordering::Acquire) && anr_tick >= anr_ticks {
                    // During a storm (several stacked dialogs) poll
                    // every tick (~2 s) — a visible ANR window eats
                    // every user touch until it's gone. Quiet periods
                    // stay at ~15 s to keep dumpsys load negligible.
                    let anr_active = dismiss_anr_dialogs() > 0;
                    anr_tick = 0;
                    anr_ticks = if anr_active { 1 } else { 8 };
                }
                if !BOOT_COMPLETED.load(Ordering::Acquire)
                    && adb_run(&["-e", "shell", "getprop", "sys.boot_completed"])
                        .map(|s| s.trim() == "1")
                        .unwrap_or(false)
                {
                    BOOT_COMPLETED.store(true, Ordering::Release);
                    set_stage(stage::BOOTED);
                    log_line("sys.boot_completed=1");
                }
            } else {
                // During early boot the guest legitimately misses a few
                // probes. "Still alive" = our spawned child OR an AVD
                // pid-file record — the latter only appears after the
                // launcher's init (30-60 s on a fresh AVD), the former
                // covers exactly that gap. Observed live: fresh-AVD
                // boot raced the 10-miss window and the poller exited
                // while qemu kept booting → permanent "Загрузка".
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

/// Dismiss guest "Application Not Responding" dialogs by tapping
/// "Wait" — the only non-destructive button (tapping the top row kills
/// the app; a dead SystemUI respawns itself anyway). The ANR window's
/// frame is parsed from `dumpsys window windows`: the bottom row is
/// always "Wait" (locale-independent layout). Runs on the poller's
/// cadence — guest ANRs are transient first-boot churn but a stuck
/// dialog covers the whole display the user paid to see. Returns
/// how many ANR windows were found (drives adaptive cadence).
fn dismiss_anr_dialogs() -> usize {
    let Ok(dump) = adb_run(&["-e", "shell", "dumpsys", "window", "windows"]) else {
        return 0;
    };
    let mut found = 0usize;
    // Find each `Application Not Responding:` window's mFrame.
    let mut in_anr = false;
    for line in dump.lines() {
        if line.contains("Application Not Responding:") {
            in_anr = true;
            found += 1;
            continue;
        }
        if !in_anr {
            continue;
        }
        if line.contains("Window #") {
            in_anr = false;
            continue;
        }
        // `frame=[224,212][1055,507]` → bottom-row ("Wait") tap point.
        if let Some(pos) = line.find("frame=[") {
            let coords: Vec<i32> = line[pos + 6..]
                .split(['[', ']', ','])
                .filter_map(|t| t.parse::<i32>().ok())
                .collect();
            if coords.len() >= 4 {
                let (x1, y1, x2, y2) = (coords[0], coords[1], coords[2], coords[3]);
                if x2 > x1 && y2 > y1 {
                    let tx = (x1 + x2) / 2;
                    // 0.70 down the dialog = dead-center of the bottom
                    // ("Wait") row; verified live at 0.71.
                    let ty = y1 + ((y2 - y1) * 70) / 100;
                    let _ = adb_run(&[
                        "-e",
                        "shell",
                        "input",
                        "tap",
                        &tx.to_string(),
                        &ty.to_string(),
                    ]);
                    log_line(&format!("ANR dialog dismissed via Wait tap ({tx},{ty})"));
                }
            }
            in_anr = false;
        }
    }
    found
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
            .arg("shell")
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
            const PF_ARGB32: i32 = 0x00200931;
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

/// One-shot emulator-console command (TCP :5554, token auth).
/// Used for `avd snapshot save` — the ONLY path that persists the
/// full QuickBoot snapshot (RAM is file-backed via -mem-path but
/// `cache`/`userdata` qcow2 overlays are written solely by this
/// command; `reboot -p`/monitor `quit` alone produce a snapshot the
/// next launch rejects as "no snapshot → cold boot").
fn emu_console_cmd(line: &str, reply_budget_ms: u64) -> Result<String, String> {
    use std::io::{BufRead, BufReader, Write};
    let mut stream =
        std::net::TcpStream::connect("127.0.0.1:5554").map_err(|e| format!("console 5554: {e}"))?;
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
    for _ in 0..8 {
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
        return Err("no .emulator_console_auth_token".into());
    }
    writeln!(stream, "auth {token}").map_err(|e| e.to_string())?;
    stream.flush().ok();
    writeln!(stream, "{line}").map_err(|e| e.to_string())?;
    stream.flush().ok();
    // Read until OK/KO or the budget's read timeout elapses —
    // `avd snapshot save` can take tens of seconds of disk I/O.
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

/// Unified injector ordered by what ACTUALLY reaches the guest on a
/// `ro.build.type=user` image in `-no-window` mode (all paths were
/// verified live against `getevent`):
///
/// * `Events` — `sendevent` batches, eng/userdebug images only.
///   Probed with a real write: on user builds `/dev/input/eventN`
///   opens return EACCES even though the node "exists".
/// * `Shell` — `input tap`/`input swipe` through the persistent
///   `adb shell` pipe (~300-800 ms/event, always works as shell).
///
/// Dead paths removed after live verification:
///   * emulator console `event send` — replies OK, dropped silently
///     (user-event pipes are plumbed only when a Qt window exists);
///   * QEMU HMP `mouse_move`/`mouse_button` — routes to the
///     PS/2-class handler which doesn't exist on ranchu;
///   * QMP `input-send-event` — reaches `qwerty2` for *keys* only;
///     `abs x/y` has no pointer device to land on in this build.
///
///   The remaining sub-ms upgrade for touch is the gRPC
///   `injectTouch` API on :8554 (needs a client-signed JWK —
///   non-trivial, tracked in the audit).
pub enum FastInput {
    /// `sendevent` path — eng/userdebug images only. `lcd` scales
    /// guest px → the device's 0..32767 ABS range.
    Events {
        node: String,
        tracking_id: i32,
        lcd: (u32, u32),
    },
    Shell,
}

/// `wm size` → (w,h); falls back to the spec profile LCD when the
/// guest is too early in boot to answer.
fn guest_lcd() -> (u32, u32) {
    if let Ok(out) = adb_run(&["shell", "wm", "size"]) {
        if let Some((w, h)) = out
            .split_whitespace()
            .last()
            .and_then(|s| s.split_once('x'))
        {
            if let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) {
                if w > 0 && h > 0 {
                    return (w, h);
                }
            }
        }
    }
    (1280, 720)
}

impl FastInput {
    pub fn connect() -> Result<Self, String> {
        // sendevent survives only on images where shell can open
        // /dev/input — probe a real write, not just the node name.
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
        log_line("input path: persistent `input` (sendevent unavailable)");
        Ok(FastInput::Shell)
    }

    fn ev_line(node: &str, t: u32, c: u32, v: i32) -> String {
        format!("sendevent {node} {t} {c} {v}")
    }

    /// DOWN at (x,y) — writes one `;`-chained batch so the persistent
    /// shell executes all five events in a single write.
    pub fn down(&mut self, shell: &mut AdbShell, x: u32, y: u32) -> Result<(), String> {
        match self {
            // `input` can't hold — a 600 ms still-swipe is the only
            // way to leave the finger down for follow-up moves.
            FastInput::Shell => shell.swipe(x as i32, y as i32, x as i32, y as i32, 600),
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
                    Self::ev_line(node, 3, 0x39, *tracking_id), // ABS_MT_TRACKING_ID
                    Self::ev_line(node, 3, 0x35, sx as i32),    // ABS_MT_POSITION_X
                    Self::ev_line(node, 3, 0x36, sy as i32),    // ABS_MT_POSITION_Y
                    Self::ev_line(node, 1, 0x14a, 1),           // BTN_TOUCH down
                    Self::ev_line(node, 0, 0, 0),               // SYN_REPORT
                ]
                .join(";");
                shell.cmd(&batch)
            }
        }
    }

    pub fn mv(&mut self, shell: &mut AdbShell, x: u32, y: u32) -> Result<(), String> {
        match self {
            FastInput::Shell => Err("input path: no incremental move".into()),
            FastInput::Events { node, lcd, .. } => {
                let (sx, sy) = (
                    x.saturating_mul(32767) / lcd.0.max(1),
                    y.saturating_mul(32767) / lcd.1.max(1),
                );
                shell.cmd(
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

    pub fn up(&mut self, _shell: &mut AdbShell) -> Result<(), String> {
        match self {
            FastInput::Shell => Ok(()), // `input` down is self-completing
            FastInput::Events { node, .. } => _shell.cmd(
                &[
                    Self::ev_line(node, 1, 0x14a, 0),
                    Self::ev_line(node, 3, 0x39, -1),
                    Self::ev_line(node, 0, 0, 0),
                ]
                .join(";"),
            ),
        }
    }

    /// Full tap = down+up. `Events` is ~ms; `Shell` falls back to a
    /// self-contained `input tap` (~300-800 ms).
    pub fn tap(&mut self, shell: &mut AdbShell, x: u32, y: u32) -> Result<(), String> {
        if let FastInput::Shell = self {
            return shell.tap(x, y);
        }
        self.down(shell, x, y)?;
        self.up(shell)
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
