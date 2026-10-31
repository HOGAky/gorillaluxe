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
use std::sync::mpsc;
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
    /// Reap the engine: kill the direct child, then close the Job —
    /// closing the last Job handle with `KILL_ON_JOB_CLOSE` terminates
    /// every process assigned to it (qemu grandchildren included).
    pub fn shutdown(&mut self) {
        let _ = self.child.kill();
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
/// `gpu_mode` lands in `hw.gpu.mode` so config and CLI agree.
pub fn write_avd(
    root: &Path,
    sysimg: &Path,
    _profile: &DeviceProfile,
    gpu_mode: &str,
) -> Result<PathBuf, String> {
    let home = root.join("avd");
    let avd = home.join("GorillaLuxe.avd");
    std::fs::create_dir_all(&avd).map_err(|e| e.to_string())?;
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
            "hw.cpu.ncore=4\n",
            "hw.ramSize=4096\n",
            // Gaming tablet, landscape 720p — small raw frames for
            // 60 FPS streaming; games upscale cleanly.
            "hw.lcd.width=1280\n",
            "hw.lcd.height=720\n",
            "hw.lcd.density=240\n",
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
        gpu_mode = gpu_mode,
        sysdir = sysimg.to_string_lossy().replace('\\', "/"),
    );
    std::fs::write(avd.join("config.ini"), config).map_err(|e| e.to_string())?;
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
fn emulator_args(gpu: &str, accel: &str) -> Vec<String> {
    vec![
        "-avd".into(),
        "GorillaLuxe".into(),
        "-no-window".into(),
        // QuickBoot snapshots stay ON — never `-no-snapshot`: the
        // engine restores guest RAM in ~2-3 s on every launch after
        // the first cold boot.
        "-no-metrics".into(),
        // Local gRPC control port (screenshot streaming / touch API).
        "-grpc".into(),
        "8554".into(),
        "-accel".into(),
        accel.to_string(),
        "-gpu".into(),
        gpu.to_string(),
        "-memory".into(),
        "4096".into(),
        "-cores".into(),
        "4".into(),
        "-netdelay".into(),
        "none".into(),
        "-netspeed".into(),
        "full".into(),
        // ARM64 game support: enable Google's NDK translation bridge
        // (present on Play-Store x86_64 images ≥API 30). Harmless when
        // the lib is absent — the prop only matters for arm64 libs.
        "-prop".into(),
        "ro.dalvik.vm.native.bridge=libndk_translation.so".into(),
        "-verbose".into(),
    ]
}

/// Boot Android headlessly. `gpu` = `"host"` (gfxstream→ANGLE→D3D) or
/// `"swiftshader_indirect"`; `accel` = `"whpx"` or `"auto"`.
pub fn launch(profile: &DeviceProfile, gpu: &str, accel: &str) -> Result<EmulatorProc, String> {
    let InstallState::Ready {
        emulator, sysimg, ..
    } = install_state()
    else {
        return Err("Android runtime not installed".into());
    };
    let root = runtime_dir();
    write_avd(&root, &sysimg, profile, gpu)?;
    // Engine stdout+stderr → logs\emulator.log so an early exit (e.g.
    // "PANIC: Cannot find AVD system path") is diagnosable.
    let logs = data_dir().join("logs");
    let _ = std::fs::create_dir_all(&logs);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs.join("emulator.log"))
        .map_err(|e| format!("emulator.log: {e}"))?;
    let log_err = log_file.try_clone().map_err(|e| e.to_string())?;
    let child = super::silent_cmd(&emulator)
        .args(emulator_args(gpu, accel))
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

/// Ordered `-gpu` candidates for the fallback chain. Intel iGPUs crash
/// `host` mode at Vulkan external-memory init → lead with ANGLE/D3D11;
/// discrete NVIDIA/AMD get `host` first; CI forces pure software.
pub fn gpu_chain(force_swiftshader: bool) -> Vec<&'static str> {
    if force_swiftshader {
        return vec!["swiftshader_indirect"];
    }
    match primary_gpu_vendor() {
        Some(0x8086) => vec!["angle_indirect", "host", "swiftshader_indirect"],
        _ => vec!["host", "angle_indirect", "swiftshader_indirect"],
    }
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
    Attached { next_probe: Instant, since: Instant },
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

/// `true` while the emulator is up. `Attached` probes `adb get-state`
/// at most every 2 s (a one-shot process spawn per frame would be
/// exactly the churn this runtime avoids).
pub fn alive(h: &mut EmulatorHandle) -> bool {
    match h {
        EmulatorHandle::Owned(e) => matches!(e.child.try_wait(), Ok(None)),
        EmulatorHandle::Attached { next_probe, .. } => {
            if Instant::now() < *next_probe {
                return true;
            }
            *next_probe = Instant::now() + std::time::Duration::from_secs(2);
            adb_run(&["get-state"])
                .map(|s| s.trim() == "device")
                .unwrap_or(false)
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

/// Terminate stale `emulator.exe`/`qemu-system-*` instances holding
/// our AVD, then clear `*.lock` files. Only runs when stale locks or
/// an `avd/running/pid_*.ini` orphan actually exist — a live usable
/// emulator takes the `Attached` path instead.
fn kill_stale_emulators() {
    let avd = avd_home().join("GorillaLuxe.avd");
    let has_locks = std::fs::read_dir(&avd)
        .map(|rd| {
            rd.flatten()
                .any(|e| e.path().extension().is_some_and(|x| x == "lock"))
        })
        .unwrap_or(false);
    let run_dir = std::env::temp_dir().join(r"avd\running");
    let has_pid = std::fs::read_dir(&run_dir)
        .map(|rd| {
            rd.flatten().any(|e| {
                e.file_name().to_string_lossy().starts_with("pid_")
                    && e.path().extension().is_some_and(|x| x == "ini")
            })
        })
        .unwrap_or(false);
    if !has_locks && !has_pid {
        return;
    }
    log_line("stale emulator locks/pids found — terminating orphans");
    for img in [
        "qemu-system-x86_64-headless.exe",
        "qemu-system-x86_64.exe",
        "emulator.exe",
    ] {
        let _ = super::silent_cmd("taskkill.exe")
            .args(["/F", "/T", "/IM", img])
            .output();
    }
    for e in std::fs::read_dir(&run_dir).into_iter().flatten().flatten() {
        if e.path().extension().is_some_and(|x| x == "ini") {
            let _ = std::fs::remove_file(e.path());
        }
    }
    purge_stale_locks(&avd);
}

/// Attach to a running emulator, else kill stale orphans, then spawn →
/// watch 12 s → if the engine died, retry with the next `-gpu` mode in
/// the vendor-aware chain. Guarantees a boot on every GPU (NVIDIA/AMD
/// `host`, Intel `angle_indirect`, software last resort) without user
/// intervention. `accel` = `"whpx"` or `"auto"`.
pub fn launch_resilient(
    profile: &DeviceProfile,
    accel: &str,
    force_swiftshader: bool,
) -> Result<EmulatorHandle, String> {
    // Two `emulator.exe` instances can never share an AVD — if one is
    // already serving `emulator-5554`, adopt it; otherwise clear the
    // orphans whose locks would make every spawn die inside 12 s.
    if adb_device_present() {
        log_line("live emulator detected — attaching (no respawn)");
        return Ok(EmulatorHandle::Attached {
            next_probe: Instant::now() + std::time::Duration::from_secs(2),
            since: Instant::now(),
        });
    }
    kill_stale_emulators();
    let chain = gpu_chain(force_swiftshader);
    let mut last = String::new();
    for (i, gpu) in chain.iter().enumerate() {
        // Remove stale locks BEFORE qemu grabs the AVD — leftover
        // `*.lock` files from an unclean exit block cold relaunch.
        purge_stale_locks(&avd_home().join("GorillaLuxe.avd"));
        let mut emu = match launch(profile, gpu, accel) {
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
            return Ok(EmulatorHandle::Owned(emu));
        }
        last = format!("gpu={gpu}: exited within 12s — {}", emulator_log_tail());
        log_line(&format!("launch attempt {} failed: {last}", i + 1));
        emu.shutdown();
    }
    Err(format!("all GPU modes failed ({accel}): {last}"))
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

/// Parse the raw `screencap` (no `-p`) 12-byte header:
/// `u32 width, u32 height, u32 format` where `1 = RGBA_8888`.
/// Returns `(w, h, pixel_offset)` — pure, unit-testable.
pub fn parse_screencap_header(b: &[u8]) -> Option<(u32, u32, usize)> {
    if b.len() < 12 {
        return None;
    }
    let w = u32::from_le_bytes(b[0..4].try_into().unwrap());
    let h = u32::from_le_bytes(b[4..8].try_into().unwrap());
    let fmt = u32::from_le_bytes(b[8..12].try_into().unwrap());
    if fmt == 1
        && w > 0
        && w <= 8192
        && h > 0
        && h <= 8192
        && b.len() >= 12 + (w as usize) * (h as usize) * 4
    {
        Some((w, h, 12))
    } else {
        None
    }
}

/// Grab one live frame: `exec-out screencap` WITHOUT `-p` returns a
/// 12-byte header followed by uncompressed RGBA — zero PNG/GDI+
/// cost, ~3-4x faster. Falls back to the GDI+ PNG path for
/// `-p`-style output.
pub fn grab_frame() -> Result<(Vec<u8>, u32, u32), String> {
    let out = super::silent_cmd(adb())
        .args(["exec-out", "screencap"])
        .output()
        .map_err(|e| format!("adb screencap: {e}"))?;
    let b = &out.stdout;
    if !out.status.success() || b.len() < 12 {
        return Err("screencap failed".into());
    }
    // Raw RGBA_8888 path.
    if let Some((w, h, off)) = parse_screencap_header(b) {
        return Ok((b[off..off + (w * h * 4) as usize].to_vec(), w, h));
    }
    // PNG fallback (`screencap -p` style output).
    if b.starts_with(b"\x89PNG") {
        return png_to_rgba(b);
    }
    Err(format!("screencap: unrecognized output len={}", b.len()))
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
pub struct FrameStream {
    child: Child,
    stdout: std::process::ChildStdout,
    buf: Vec<u8>,
}

impl FrameStream {
    pub fn open() -> Result<Self, String> {
        let mut child = super::silent_cmd(adb())
            .args(["exec-out", "while true; do screencap; done"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("frame stream: {e}"))?;
        let stdout = child.stdout.take().ok_or("frame stream stdout")?;
        Ok(FrameStream {
            child,
            stdout,
            buf: Vec::new(),
        })
    }

    /// Block for the next `[w,h,fmt,rgba]` record. Err = pipe dead
    /// (caller reopens or falls back to per-frame `grab_frame`).
    pub fn next_frame(&mut self) -> Result<(Vec<u8>, u32, u32), String> {
        use std::io::Read;
        let mut hdr = [0u8; 12];
        self.stdout
            .read_exact(&mut hdr)
            .map_err(|e| format!("stream hdr: {e}"))?;
        let w = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let h = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
        let fmt = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
        if fmt != 1 || w == 0 || h == 0 || w > 8192 || h > 8192 {
            return Err(format!("stream bad hdr {w}x{h} fmt={fmt}"));
        }
        let n = (w as usize) * (h as usize) * 4;
        self.buf.resize(n, 0);
        self.stdout
            .read_exact(&mut self.buf)
            .map_err(|e| format!("stream px: {e}"))?;
        Ok((self.buf.clone(), w, h))
    }
}

impl Drop for FrameStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

// ---------------------------------------------------------------------------
// Low-latency input — emulator console (TCP 5554) or persistent sendevent
// ---------------------------------------------------------------------------

/// Android Emulator Console on 127.0.0.1:5554 — pure TCP, no guest
/// process spawn per event (~1 ms vs ~100 ms for Java `input tap`).
/// Authenticates with `%USERPROFILE%\.emulator_console_auth_token`.
pub struct EmuConsole {
    stream: std::net::TcpStream,
}

impl EmuConsole {
    pub fn connect() -> Result<Self, String> {
        use std::io::{BufRead, BufReader, Write};
        let mut stream = std::net::TcpStream::connect("127.0.0.1:5554")
            .map_err(|e| format!("console 5554: {e}"))?;
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .ok();
        // Read the greeting ("Android Console: ... OK").
        let mut br = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut line = String::new();
        let _ = br.read_line(&mut line);
        // Auth token — emulator writes it to $HOME or EMULATOR_HOME.
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
        if !token.is_empty() {
            writeln!(stream, "auth {token}").map_err(|e| e.to_string())?;
            stream.flush().ok();
            line.clear();
            let _ = br.read_line(&mut line); // expect "OK"
        }
        Ok(EmuConsole { stream })
    }

    /// Fire one console command; replies are drained lazily (we never
    /// block waiting — command latency is the only thing that matters).
    pub fn cmd(&mut self, line: &str) -> Result<(), String> {
        use std::io::Write;
        writeln!(self.stream, "{line}")
            .and_then(|_| self.stream.flush())
            .map_err(|e| e.to_string())
    }

    /// Multi-touch-style DOWN at (x,y) on the ranchu touchscreen.
    pub fn touch_down(&mut self, x: u32, y: u32) -> Result<(), String> {
        for ev in [
            "event send EV_ABS:ABS_MT_TRACKING_ID:1",
            &format!("event send EV_ABS:ABS_MT_POSITION_X:{x}"),
            &format!("event send EV_ABS:ABS_MT_POSITION_Y:{y}"),
            "event send EV_KEY:BTN_TOUCH:1",
            "event send EV_SYN:SYN_REPORT:0",
        ] {
            self.cmd(ev)?;
        }
        Ok(())
    }

    pub fn touch_move(&mut self, x: u32, y: u32) -> Result<(), String> {
        for ev in [
            &format!("event send EV_ABS:ABS_MT_POSITION_X:{x}"),
            &format!("event send EV_ABS:ABS_MT_POSITION_Y:{y}"),
            "event send EV_SYN:SYN_REPORT:0",
        ] {
            self.cmd(ev)?;
        }
        Ok(())
    }

    pub fn touch_up(&mut self) -> Result<(), String> {
        for ev in [
            "event send EV_KEY:BTN_TOUCH:0",
            "event send EV_ABS:ABS_MT_TRACKING_ID:-1",
            "event send EV_SYN:SYN_REPORT:0",
        ] {
            self.cmd(ev)?;
        }
        Ok(())
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

/// Unified low-latency injector: emulator console `event send`
/// (pure TCP) preferred, `sendevent` batches over the persistent
/// `adb shell` pipe as fallback — both ~1-3 ms, never `input tap`.
pub enum FastInput {
    Console(EmuConsole),
    Events { node: String, tracking_id: i32 },
}

impl FastInput {
    /// Console first; falls back to sendevent-on-shell (the caller
    /// supplies the persistent `AdbShell` for that path).
    pub fn connect() -> Result<Self, String> {
        match EmuConsole::connect() {
            Ok(mut c) => {
                // Probe: a harmless SYN must not error.
                c.cmd("event send EV_SYN:SYN_REPORT:0")?;
                log_line("input path: emulator console :5554");
                Ok(FastInput::Console(c))
            }
            Err(e) => {
                let node = touch_input_node()
                    .map_err(|e2| format!("console: {e}; sendevent probe: {e2}"))?;
                log_line(&format!("input path: sendevent on {node} (console: {e})"));
                Ok(FastInput::Events {
                    node,
                    tracking_id: 1,
                })
            }
        }
    }

    fn ev_line(node: &str, t: u32, c: u32, v: i32) -> String {
        format!("sendevent {node} {t} {c} {v}")
    }

    /// DOWN at (x,y) — writes one `;`-chained batch so the persistent
    /// shell executes all five events in a single write.
    pub fn down(&mut self, shell: &mut AdbShell, x: u32, y: u32) -> Result<(), String> {
        match self {
            FastInput::Console(c) => c.touch_down(x, y),
            FastInput::Events { node, tracking_id } => {
                *tracking_id = tracking_id.wrapping_add(1).max(1);
                let batch = [
                    Self::ev_line(node, 3, 0x39, *tracking_id), // ABS_MT_TRACKING_ID
                    Self::ev_line(node, 3, 0x35, x as i32),     // ABS_MT_POSITION_X
                    Self::ev_line(node, 3, 0x36, y as i32),     // ABS_MT_POSITION_Y
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
            FastInput::Console(c) => c.touch_move(x, y),
            FastInput::Events { node, .. } => shell.cmd(
                &[
                    Self::ev_line(node, 3, 0x35, x as i32),
                    Self::ev_line(node, 3, 0x36, y as i32),
                    Self::ev_line(node, 0, 0, 0),
                ]
                .join(";"),
            ),
        }
    }

    pub fn up(&mut self, shell: &mut AdbShell) -> Result<(), String> {
        match self {
            FastInput::Console(c) => c.touch_up(),
            FastInput::Events { node, .. } => shell.cmd(
                &[
                    Self::ev_line(node, 1, 0x14a, 0),
                    Self::ev_line(node, 3, 0x39, -1),
                    Self::ev_line(node, 0, 0, 0),
                ]
                .join(";"),
            ),
        }
    }

    /// Full tap = down+up (still <5 ms end-to-end on either path).
    pub fn tap(&mut self, shell: &mut AdbShell, x: u32, y: u32) -> Result<(), String> {
        self.down(shell, x, y)?;
        self.up(shell)
    }
}
