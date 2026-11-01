//! "One-Click WHPX Auto-Setup" (Phase 3 UX requirement).
//!
//! `diagnose_host()` detects whether WHPX is usable. When it is not,
//! the UI shows a Luxe Dark banner whose golden button triggers a
//! UAC-elevated `gorillaluxe.exe --elevated-enable-whpx` process that
//! enables `HypervisorPlatform` + `VirtualMachinePlatform` and sets
//! `bcdedit hypervisorlaunchtype auto`. End users never touch
//! `optionalfeatures.exe` or `bcdedit` manually.

use std::path::PathBuf;

use crate::config::data_dir;
use crate::whpx::Whpx;

#[derive(Clone, Debug)]
pub struct HostReport {
    /// `true` when WHPX is loadable and the hypervisor is present.
    pub whpx_ok: bool,
    /// Human-readable reason when `whpx_ok` is false.
    pub detail: Option<String>,
    /// Whether this process already runs elevated.
    pub elevated: bool,
}

/// Probe WHPX availability (DLL load + hypervisor presence).
pub fn diagnose_host() -> HostReport {
    let elevated = is_elevated();
    match Whpx::instance() {
        Err(e) => HostReport {
            whpx_ok: false,
            detail: Some(format!("hypervisor platform unavailable: {e}")),
            elevated,
        },
        Ok(w) => match w.hypervisor_present() {
            Ok(true) => HostReport {
                whpx_ok: true,
                detail: None,
                elevated,
            },
            Ok(false) => HostReport {
                whpx_ok: false,
                detail: Some("hypervisor_not_present".into()),
                elevated,
            },
            Err(e) => HostReport {
                whpx_ok: false,
                detail: Some(format!("hypervisor probe failed: {e}")),
                elevated,
            },
        },
    }
}

/// Total physical host RAM in MiB via `GlobalMemoryStatusEx`
/// (0 on failure — callers treat that as "unknown, assume capable").
/// Drives the weak-PC spec profile: ≤4 GiB hosts get a 960x540 /
/// 30 FPS / 1 GiB-guest-RAM profile so the host never swaps.
pub fn host_ram_mb() -> u64 {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    unsafe {
        let mut s = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        if GlobalMemoryStatusEx(&mut s).is_ok() {
            s.ullTotalPhys / (1024 * 1024)
        } else {
            0
        }
    }
}

/// `true` when the current process token is elevated.
pub fn is_elevated() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const TOKEN_QUERY: u32 = 0x0008;
    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        if OpenProcessToken(
            GetCurrentProcess(),
            windows::Win32::Security::TOKEN_ACCESS_MASK(TOKEN_QUERY),
            &mut token,
        )
        .is_err()
        {
            return false;
        }
        let mut elev = TOKEN_ELEVATION::default();
        let mut ret = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elev as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elev.TokenIsElevated != 0
    }
}

fn marker_path() -> PathBuf {
    data_dir().join("whpx_setup_done.flag")
}

/// `true` when an elevated setup run already finished successfully.
pub fn setup_completed() -> bool {
    marker_path().exists()
}

/// Launch `gorillaluxe.exe --elevated-enable-whpx` with the UAC
/// `runas` verb. Returns Err when the user cancels the prompt.
pub fn launch_elevated_setup() -> Result<(), String> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let exe_w: Vec<u16> = exe.to_string_lossy().encode_utf16().chain([0]).collect();
    unsafe {
        let r = ShellExecuteW(
            None,
            windows::core::w!("runas"),
            windows::core::PCWSTR(exe_w.as_ptr()),
            windows::core::w!("--elevated-enable-whpx"),
            windows::core::PCWSTR::null(),
            SW_HIDE,
        );
        // ShellExecute returns >32 on success.
        if (r.0 as isize) > 32 {
            Ok(())
        } else {
            Err(format!("ShellExecuteW(runas) declined: {:?}", r.0))
        }
    }
}

/// Run inside the elevated process: enable both Windows features and
/// set hypervisor launch type, then drop the marker + log file.
pub fn run_elevated_setup() -> Result<String, String> {
    if !is_elevated() {
        return Err("setup must run elevated".into());
    }
    let cmds: &[(&str, &[&str])] = &[
        (
            "dism.exe",
            &[
                "/Online",
                "/Enable-Feature",
                "/FeatureName:HypervisorPlatform",
                "/All",
                "/NoRestart",
            ],
        ),
        (
            "dism.exe",
            &[
                "/Online",
                "/Enable-Feature",
                "/FeatureName:VirtualMachinePlatform",
                "/All",
                "/NoRestart",
            ],
        ),
        ("bcdedit.exe", &["/set", "hypervisorlaunchtype", "auto"]),
    ];
    let mut log = String::new();
    for (exe, args) in cmds {
        let out = super::silent_cmd(exe)
            .args(*args)
            .output()
            .map_err(|e| format!("spawn {exe}: {e}"))?;
        log.push_str(&format!(
            "$ {} {}\nexit={}\n{}\n{}\n\n",
            exe,
            args.join(" "),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).trim_end(),
            String::from_utf8_lossy(&out.stderr).trim_end(),
        ));
        if !out.status.success() {
            return Err(format!("{exe} exited {:?}", out.status.code()));
        }
    }
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("whpx_setup.log"), &log);
    std::fs::write(marker_path(), "done\n").map_err(|e| format!("marker: {e}"))?;
    Ok(log)
}

/// Trigger a Windows restart (`shutdown /r /t 0`).
pub fn restart_pc() -> Result<(), String> {
    super::silent_cmd("shutdown.exe")
        .args(["/r", "/t", "0"])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("shutdown.exe: {e}"))
}
