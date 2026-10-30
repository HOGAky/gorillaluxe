//! Host environment management: WHPX feature detection and the
//! zero-friction "Enable WHPX in 1 Click" auto-setup (UAC elevation),
//! Android runtime bootstrapper, device profiles, virtual sensors,
//! shared-memory frames, snapshots and hybrid CPU pinning.

pub mod android_runtime;
pub mod cpu_pinning;
pub mod device_profile;
pub mod downloader;
pub mod env_doctor;
pub mod logger;
pub mod manifest;
pub mod selftest;
pub mod sensors;
pub mod shm_publisher;
pub mod snapshot;

/// Spawn `program` with `CREATE_NO_WINDOW` so console subprocesses
/// (curl.exe, adb.exe, emulator.exe, dism.exe) NEVER flash a black
/// CMD window on the user's screen. EVERY background `Command` in the
/// workspace must go through this helper.
pub fn silent_cmd<P: AsRef<std::ffi::OsStr>>(program: P) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut c = std::process::Command::new(program);
    c.creation_flags(CREATE_NO_WINDOW);
    c
}
