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
use std::process::{Child, ChildStdin, Command, Stdio};
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
/// (`vendor.img` is validated separately as a warning — some images
/// fold it into `system.img`.)
pub fn missing_required(root: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    for rel in [EMULATOR_REL, QEMU_REL, ADB_REL] {
        if !root.join(rel).is_file() {
            missing.push(rel.replace('/', "\\"));
        }
    }
    match find_sysimg(root) {
        Some(si) => {
            for f in ["system.img", "vendor.img", "ramdisk.img", "userdata.img"] {
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

/// Fetch + parse a repository manifest via ureq. `None` when offline
/// or when the body isn't SDK-repository XML.
pub fn fetch_manifest(rel: &str) -> Option<Vec<manifest::Artifact>> {
    let url = format!("{}{rel}", manifest::REPO_BASE);
    let body = ureq::get(&url)
        .timeout(std::time::Duration::from_secs(30))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    if !body.contains("<remotePackage") {
        return None;
    }
    let arts = manifest::parse_manifest(&body);
    (!arts.is_empty()).then_some(arts)
}

/// Resolve the Android 13 x86_64 system image: walks the flavor
/// manifests in preference order (playstore → google_apis → aosp).
pub fn fetch_system_image() -> Option<manifest::Artifact> {
    for (_flavor, rel) in manifest::SYSIMG_MANIFESTS {
        if let Some(arts) = fetch_manifest(rel) {
            if let Some(img) = manifest::choose_system_image(&arts) {
                return Some(img);
            }
        }
    }
    None
}

/// Resolve the three download URLs needed for the runtime.
/// `Ok` = `(platform-tools, emulator, system-image)` in download order.
pub fn resolve_runtime_urls() -> Result<[manifest::Artifact; 3], String> {
    let repo = fetch_manifest(manifest::REPO_MANIFEST)
        .ok_or_else(|| "cannot reach dl.google.com repository manifest".to_string())?;
    let tools = manifest::choose_platform_tools(&repo)
        .ok_or_else(|| "no platform-tools package in manifest".to_string())?;
    let emu = manifest::choose_emulator(&repo)
        .ok_or_else(|| "no emulator-windows_x64 package in manifest".to_string())?;
    let img = fetch_system_image()
        .ok_or_else(|| "no x86_64 system image in sys-img manifests".to_string())?;
    Ok([tools, emu, img])
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
            targets.push(ArtifactTarget {
                step: i + 1,
                label: labels[i].to_string(),
                url: a.absolute_url.clone(),
                zip: root.join(leaf),
                dest: target_dest(&root, a),
                sha256: a.sha256.clone(),
                sha1: a.sha1.clone(),
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
pub fn write_avd(root: &Path, sysimg: &Path, profile: &DeviceProfile) -> Result<PathBuf, String> {
    let home = root.join("avd");
    let avd = home.join("GorillaLuxe.avd");
    std::fs::create_dir_all(&avd).map_err(|e| e.to_string())?;
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
            "hw.lcd.width=1440\n",
            "hw.lcd.height=3088\n",
            "hw.lcd.density={dpi}\n",
            "hw.keyboard=yes\n",
            "hw.mainKeys=no\n",
            "hw.sensors.proximity=yes\n",
            "hw.camera.back=virtualscene\n",
            "hw.gpu.enabled=yes\n",
            "hw.gpu.mode=host\n",
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
        dpi = profile.dpi,
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
/// appearing while gfxstream/ANGLE renders offscreen.
fn emulator_args(profile: &DeviceProfile, gpu: &str, accel: &str) -> Vec<String> {
    let mut v = vec![
        "-avd".into(),
        "GorillaLuxe".into(),
        "-no-window".into(),
        "-no-boot-anim".into(),
        "-no-snapshot".into(),
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
    ];
    for (k, val) in [
        ("ro.product.model", profile.model),
        ("ro.product.brand", profile.brand),
        ("ro.product.device", profile.device),
        ("ro.product.board", profile.board),
        ("ro.product.hardware", profile.hardware),
    ] {
        v.push("-prop".into());
        v.push(format!("{k}={val}"));
    }
    v
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
    write_avd(&root, &sysimg, profile)?;
    let child = Command::new(&emulator)
        .args(emulator_args(profile, gpu, accel))
        .current_dir(emulator.parent().unwrap())
        // Point the engine entirely inside our runtime dir — AVD and
        // SDK resolution never touch the user's global ~/.android.
        .env("ANDROID_SDK_ROOT", &root)
        .env("ANDROID_EMULATOR_HOME", &root)
        .env("ANDROID_AVD_HOME", avd_home())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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
// ADB bridge — persistent shell + one-shot commands + frame capture
// ---------------------------------------------------------------------------

fn adb() -> std::ffi::OsString {
    adb_exe().into_os_string()
}

fn adb_run(args: &[&str]) -> Result<String, String> {
    let out = Command::new(adb())
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
        let mut child = Command::new(adb())
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
    Command::new(adb())
        .arg("wait-for-device")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
    let out = Command::new(adb())
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
