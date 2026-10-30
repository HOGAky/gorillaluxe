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
            // 1080x1920 @ 420dpi — fast universal default (~8 MB raw
            // frames) that streams well on iGPU and dGPU alike.
            "hw.lcd.width=1080\n",
            "hw.lcd.height=1920\n",
            "hw.lcd.density=420\n",
            "hw.keyboard=yes\n",
            "hw.mainKeys=no\n",
            "hw.sensors.proximity=yes\n",
            "hw.camera.back=virtualscene\n",
            "hw.gpu.enabled=yes\n",
            "hw.gpu.mode={gpu_mode}\n",
            "vm.heapSize=576\n",
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
            "fastboot.forceColdBoot=yes\n"
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
        "-no-snapshot".into(),
        "-no-metrics".into(),
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

/// `true` while the emulator process is alive.
pub fn alive(emu: &mut EmulatorProc) -> bool {
    matches!(emu.child.try_wait(), Ok(None))
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

/// Spawn → watch 12 s → if the engine died, retry with the next `-gpu`
/// mode in the vendor-aware chain. Guarantees a boot on every GPU
/// (NVIDIA/AMD `host`, Intel `angle_indirect`, software last resort)
/// without user intervention. `accel` = `"whpx"` or `"auto"`.
pub fn launch_resilient(
    profile: &DeviceProfile,
    accel: &str,
    force_swiftshader: bool,
) -> Result<EmulatorProc, String> {
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
            if !alive(&mut emu) {
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
            return Ok(emu);
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

/// Install an `.apk`/`.xapk` dropped onto the window (`-r` = reinstall).
pub fn install_apk(path: &Path) -> Result<String, String> {
    adb_run(&["install", "-r", &path.to_string_lossy()])
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
