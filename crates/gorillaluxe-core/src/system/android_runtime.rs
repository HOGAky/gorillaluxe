//! Real Android runtime bootstrapper + headless Google emulator
//! engine bridge (spec PHASE 4 task 1).
//!
//! - Resolves official Google packages (emulator, platform-tools,
//!   Android x86_64 system image with ARM64 translation support) from
//!   `dl.google.com/android/repository` manifests.
//! - Downloads resumably via `scripts/fetch-android-image.ps1` into
//!   `%LOCALAPPDATA%\GorillaLuxe\runtime\`.
//! - Boots Google's open-source headless emulator
//!   (`qemu-system-x86_64-headless` + `gfxstream`/`ANGLE` backend) with
//!   `-accel whpx -no-window` — Google's Qt window never appears.
//! - Streams the guest framebuffer to the DX12/CAS compositor via
//!   `adb exec-out screencap` → GDI+ PNG decode → RGBA (the fast
//!   shared-memory pipe is wired separately through `shm_publisher`).
//! - Forwards taps/swipes/keys, `.apk` installs, device-profile props,
//!   and RU/EN locale sync into the running guest.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use windows::core::PCWSTR;
// Explicit imports only — a `GdiPlus::*` glob would shadow
// `Result::Ok` with `Status::Ok`.
use windows::Win32::Graphics::GdiPlus::{
    BitmapData, GdipBitmapLockBits, GdipBitmapUnlockBits, GdipCreateBitmapFromStream,
    GdipDisposeImage, GdipGetImageHeight, GdipGetImageWidth, GdiplusShutdown,
    GdiplusStartup, GdiplusStartupInput, GpBitmap, GpImage, ImageLockModeRead, Rect,
};

use super::device_profile::DeviceProfile;
use super::manifest;
use crate::config::data_dir;

const EMULATOR_REL: &str = r"emulator\emulator.exe";
const ADB_REL: &str = r"platform-tools\adb.exe";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// Install state
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

/// What's installed under `runtime\`.
#[derive(Debug)]
pub enum InstallState {
    Missing,
    Partial(&'static str),
    Ready {
        emulator: PathBuf,
        adb: PathBuf,
        sysimg: PathBuf,
    },
}

pub fn install_state() -> InstallState {
    let root = runtime_dir();
    let emu = emulator_exe();
    if !emu.is_file() {
        // Zip-in-progress / never downloaded.
        return if root.join(".dl_progress").is_file() {
            InstallState::Partial("downloading")
        } else if root.exists() {
            InstallState::Partial("emulator missing")
        } else {
            InstallState::Missing
        };
    }
    let adb = adb_exe();
    if !adb.is_file() {
        return InstallState::Partial("platform-tools missing");
    }
    match find_sysimg(&root) {
        Some(sysimg) => InstallState::Ready {
            emulator: emu,
            adb,
            sysimg,
        },
        None => InstallState::Partial("system image missing"),
    }
}

// ---------------------------------------------------------------------------
// Manifest resolution + download
// ---------------------------------------------------------------------------

/// Download `url` to `path` via `URLDownloadToFileW` (IE cache,
/// resumable-ish for single GETs; chunked resume lives in the PS
/// bootstrapper for large files).
pub fn url_download(url: &str, path: &Path) -> Result<(), String> {
    unsafe {
        windows::Win32::System::Com::Urlmon::URLDownloadToFileW(
            None,
            PCWSTR(wide(url).as_ptr()),
            PCWSTR(wide(&path.to_string_lossy()).as_ptr()),
            0,
            None,
        )
        .map_err(|e| format!("URLDownloadToFileW {url}: {e}"))
    }
}

/// Fetch + parse a repository manifest. `None` when offline.
pub fn fetch_manifest(name: &str) -> Option<Vec<manifest::Artifact>> {
    let tmp = std::env::temp_dir().join(format!("gorillaluxe-{name}"));
    url_download(&format!("{}{name}", manifest::REPO_BASE), &tmp).ok()?;
    let body = std::fs::read_to_string(&tmp).ok()?;
    let arts = manifest::parse_manifest(&body);
    if arts.is_empty() {
        None
    } else {
        Some(arts)
    }
}

/// Resolve the three download URLs needed for the runtime.
/// `Ok` contains `(emulator, platform-tools, system-image)`.
pub fn resolve_runtime_urls() -> Result<[manifest::Artifact; 3], String> {
    let repo = fetch_manifest(manifest::REPO_MANIFEST)
        .ok_or_else(|| "cannot reach dl.google.com repository manifest".to_string())?;
    let emu = manifest::choose_emulator(&repo)
        .ok_or_else(|| "no emulator-windows_x64 package in manifest".to_string())?;
    let tools = manifest::choose_platform_tools(&repo)
        .ok_or_else(|| "no platform-tools package in manifest".to_string())?;
    let imgs = fetch_manifest(manifest::SYSIMG_MANIFEST)
        .ok_or_else(|| "cannot reach sys-img manifest".to_string())?;
    let img = manifest::choose_system_image(&imgs)
        .ok_or_else(|| "no x86_64 system image in manifest".to_string())?;
    Ok([emu, tools, img])
}

/// Spawn the PowerShell bootstrapper for resumable chunked downloads
/// + extraction. Progress is polled via `.dl_progress` in the runtime
/// dir (written by the script every chunk).
pub struct DownloadJob {
    child: Child,
    dir: PathBuf,
    total_bytes: u64,
}

impl DownloadJob {
    pub fn start(urls: &[manifest::Artifact; 3]) -> Result<Self, String> {
        let dir = runtime_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let script = find_fetch_script()
            .ok_or_else(|| "fetch-android-image.ps1 not found".to_string())?;
        let total = urls.iter().map(|a| a.size).sum();
        let _ = std::fs::remove_file(dir.join(".dl_progress"));
        let mut cmd = Command::new("powershell.exe");
        cmd.args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .arg("-OutDir")
        .arg(&dir);
        for a in urls {
            cmd.arg("-Url").arg(&a.absolute_url);
        }
        let child = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn fetch script: {e}"))?;
        Ok(DownloadJob {
            child,
            dir,
            total_bytes: total,
        })
    }

    /// `(bytes_done, total_bytes)` — reads the progress file written
    /// by the bootstrapper.
    pub fn progress(&self) -> (u64, u64) {
        let done = std::fs::read_to_string(self.dir.join(".dl_progress"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        (done, self.total_bytes)
    }

    /// `true` while the script is still running.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// `Some(exit_ok)` once finished.
    pub fn finished_ok(&mut self) -> Option<bool> {
        match self.child.try_wait() {
            Ok(Some(st)) => Some(st.success()),
            Ok(None) => None,
            Err(_) => Some(false),
        }
    }
}

/// Locate `fetch-android-image.ps1`: dev layout puts it at
/// `<repo>\scripts\`; the release zip ships it next to the exe.
fn find_fetch_script() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))?;
    let flat = exe_dir.join("fetch-android-image.ps1");
    if flat.is_file() {
        return Some(flat);
    }
    let dev = exe_dir
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join(r"scripts\fetch-android-image.ps1"))?;
    if dev.is_file() {
        return Some(dev);
    }
    let cwd = PathBuf::from(r"scripts\fetch-android-image.ps1");
    cwd.is_file().then_some(cwd)
}

// ---------------------------------------------------------------------------
// Emulator launch
// ---------------------------------------------------------------------------

/// Running headless emulator process.
pub struct EmulatorProc {
    pub child: Child,
    pub profile_name: String,
    pub launched_at: Instant,
}

fn avd_dir() -> PathBuf {
    dirs_home().join(".android").join("avd")
}

fn dirs_home() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
}

/// Create/refresh the `GorillaLuxe` AVD pointing at our extracted
/// system image — the standard way `emulator.exe` consumes a sysdir.
fn write_avd(sysimg: &Path, profile: &DeviceProfile) -> Result<(), String> {
    let base = avd_dir();
    let avd = base.join("GorillaLuxe.avd");
    std::fs::create_dir_all(&avd).map_err(|e| e.to_string())?;
    let ini = base.join("GorillaLuxe.ini");
    std::fs::write(
        &ini,
        format!(
            "avd.ini.encoding=UTF-8\npath={}\npath.rel=avd/GorillaLuxe.avd\ntarget=android-34\n",
            avd.display()
        ),
    )
    .map_err(|e| e.to_string())?;
    // sysdir relative to the emulator dir (`image.sysdir.1` accepts
    // absolute paths too — absolute is more robust here).
    let config = format!(
        concat!(
            "avd.ini.encoding=UTF-8\n",
            "hw.cpu.arch=x86_64\n",
            "hw.cpu.ncore=4\n",
            "hw.ramSize=6144\n",
            "hw.lcd.width=1440\n",
            "hw.lcd.height=3088\n",
            "hw.lcd.density={dpi}\n",
            "hw.keyboard=yes\n",
            "hw.mainKeys=no\n",
            "hw.sensors.proximity=yes\n",
            "hw.camera.back=virtualscene\n",
            "hw.gpu.mode=auto\n",
            "hw.audioInput=no\n",
            "vm.heapSize=576\n",
            "image.sysdir.1={sysdir}\n",
            "tag.display=Google APIs\n",
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
    Ok(())
}

/// Launch arguments — every flag exists so Google's own Qt window
/// never appears (`-no-window`) while gfxstream/ANGLE still renders
/// into the offscreen framebuffer we capture.
fn emulator_args(profile: &DeviceProfile, gpu: &str) -> Vec<String> {
    let mut v = vec![
        "-avd".into(),
        "GorillaLuxe".into(),
        "-no-window".into(),
        "-no-boot-anim".into(),
        "-no-snapshot".into(),
        "-no-audio".into(),
        "-accel".into(),
        "whpx".into(),
        "-gpu".into(),
        gpu.to_string(),
        "-memory".into(),
        "6144".into(),
        "-cores".into(),
        "4".into(),
        "-netdelay".into(),
        "none".into(),
        "-netspeed".into(),
        "full".into(),
    ];
    // Device-profile identity injection (early-boot props).
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

/// Boot Android headlessly with the given GPU mode
/// (`"swiftshader_indirect"` for headless CI / no GPU,
/// `"host"` for gfxstream→ANGLE→D3D on real hardware).
pub fn launch(profile: &DeviceProfile, gpu: &str) -> Result<EmulatorProc, String> {
    let InstallState::Ready { emulator, sysimg, .. } = install_state() else {
        return Err("Android runtime not installed".into());
    };
    write_avd(&sysimg, profile)?;
    let child = Command::new(&emulator)
        .args(emulator_args(profile, gpu))
        .current_dir(emulator.parent().unwrap())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn emulator: {e}"))?;
    Ok(EmulatorProc {
        child,
        profile_name: profile.name.to_string(),
        launched_at: Instant::now(),
    })
}

/// `true` while the emulator process is alive.
pub fn alive(emu: &mut EmulatorProc) -> bool {
    matches!(emu.child.try_wait(), Ok(None))
}

// ---------------------------------------------------------------------------
// ADB bridge — input, install, locale, frames
// ---------------------------------------------------------------------------

fn adb(args: &[&str]) -> Result<String, String> {
    let out = Command::new(adb_exe())
        .args(args)
        .output()
        .map_err(|e| format!("adb: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Install an `.apk`/`.xapk` dropped onto the window (`-r` = reinstall).
pub fn install_apk(path: &Path) -> Result<String, String> {
    adb(&["install", "-r", &path.to_string_lossy()])
}

/// Forward a tap at normalized window coords.
pub fn send_tap(x: u32, y: u32) -> Result<(), String> {
    adb(&["shell", "input", "tap", &x.to_string(), &y.to_string()]).map(|_| ())
}

/// Forward an infinite-swipe drag.
pub fn send_swipe(x1: i32, y1: i32, x2: i32, y2: i32, ms: u32) -> Result<(), String> {
    let x1 = x1.to_string();
    let y1 = y1.to_string();
    let x2 = x2.to_string();
    let y2 = y2.to_string();
    let d = ms.to_string();
    adb(&["shell", "input", "swipe", &x1, &y1, &x2, &y2, &d]).map(|_| ())
}

/// Guest locale sync (spec §3.5): `ru-RU` / `en-US`.
pub fn set_locale(locale: &str) -> Result<(), String> {
    let lang = locale.split('-').next().unwrap_or("en");
    adb(&["shell", "setprop", "persist.sys.locale", locale])?;
    adb(&["shell", "setprop", "persist.sys.language", lang]).map(|_| ())
}

/// Grab one live frame from the guest:
/// `adb exec-out screencap` (PNG) → GDI+ decode → tightly packed RGBA.
pub fn grab_frame() -> Result<(Vec<u8>, u32, u32), String> {
    let out = Command::new(adb_exe())
        .args(["exec-out", "screencap"])
        .output()
        .map_err(|e| format!("adb screencap: {e}"))?;
    if !out.status.success() || out.stdout.len() < 8 {
        return Err("screencap failed".into());
    }
    png_to_rgba(&out.stdout)
}

/// PNG bytes → RGBA via GDI+ (`GdiplusStartup` is cheap to repeat and
/// keeps this usable from any thread).
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
                .ok_or("SHCreateMemStream")?;
            let mut bmp: *mut GpBitmap = std::ptr::null_mut();
            if GdipCreateBitmapFromStream(&stream, &mut bmp).0 != 0 || bmp.is_null() {
                return Err("GdipCreateBitmapFromStream".into());
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
            if GdipBitmapLockBits(bmp, &rect, ImageLockModeRead.0 as u32, PF_ARGB32, &mut data)
                .0
                != 0
            {
                GdipDisposeImage(bmp as *mut GpImage);
                return Err("GdipBitmapLockBits".into());
            }
            // 32bpp ARGB little-endian → BGRA byte order; swap to RGBA.
            let mut rgba = vec![0u8; (w * h * 4) as usize];
            for y in 0..h as usize {
                let src = (data.Scan0 as *const u8).add(y * data.Stride as usize);
                let dst = rgba.as_mut_ptr().add(y * w as usize * 4);
                for x in 0..w as usize {
                    let s = src.add(x * 4);
                    let d = dst.add(x * 4);
                    *d = *s.add(2); // R
                    *d.add(1) = *s.add(1); // G
                    *d.add(2) = *s; // B
                    *d.add(3) = *s.add(3); // A
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
