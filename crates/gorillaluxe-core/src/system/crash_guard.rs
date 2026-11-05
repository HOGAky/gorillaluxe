//! Last-resort crash forensics — every panic lands in
//! `gorillaluxe.log` (with backtrace) AND a `.dmp` minidump in
//! `%LOCALAPPDATA%\GorillaLuxe\crash\` (spec: zero crashes without
//! diagnostics).
//!
//! Two deliberate deviations from the normal logger:
//! * The crash line is written with a fresh `CreateFile` append —
//!   never through `logger::log`'s static `Mutex`, which the
//!   panicking thread may itself hold (deadlock inside the hook).
//! * `MiniDumpWriteDump` is bound through `GetProcAddress` (the same
//!   pattern as every other optional DLL in this codebase) so a
//!   missing/odd dbghelp can never turn a panic into a nested abort.

use std::fs::OpenOptions;
use std::io::Write;
use std::panic;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::data_dir;

/// Install once per process (both gorillaluxe and gorillaluxe-cli).
pub fn install() {
    let _ = std::fs::create_dir_all(data_dir().join("crash"));
    panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "?".into());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "?".into());
        let thread = std::thread::current();
        let bt = std::backtrace::Backtrace::force_capture();
        crash_log(&format!(
            "PANIC in '{}' at {loc}: {msg}\n{bt}",
            thread.name().unwrap_or("unnamed"),
        ));
        write_minidump(&loc);
    }));
}

/// Append a raw crash line to gorillaluxe.log bypassing the logger's
/// lock — plus a mirrored copy into `crash\crash.log`.
fn crash_log(line: &str) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let out = format!("[{secs}] PANIC {line}");
    for path in [
        data_dir().join("logs").join("gorillaluxe.log"),
        data_dir().join("crash").join("crash.log"),
    ] {
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{out}");
        }
    }
}

type MiniDumpWriteDumpFn = unsafe extern "system" fn(
    hprocess: isize,
    processid: u32,
    hfile: isize,
    dumptype: u32,
    exceptionparam: *const u8,
    userstreamparam: *const u8,
    callbackparam: *const u8,
) -> i32;

/// `crash\panic-<unix_secs>.dmp` — MiniDumpNormal (0) keeps the file
/// a few hundred KB; a full-memory dump of a several-hundred-MB
/// process would be far heavier than a bug report can carry.
fn write_minidump(loc: &str) {
    use windows::core::{s, PCSTR};
    use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = data_dir().join("crash");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("panic-{secs}.dmp"));
    unsafe {
        let Ok(dbg) = GetModuleHandleA(s!("dbghelp.dll")).or_else(|_| {
            windows::Win32::System::LibraryLoader::LoadLibraryA(PCSTR(
                c"dbghelp.dll".as_ptr().cast(),
            ))
        }) else {
            crash_log("minidump: dbghelp.dll unavailable");
            return;
        };
        let Some(proc_) = GetProcAddress(dbg, s!("MiniDumpWriteDump")) else {
            crash_log("minidump: MiniDumpWriteDump not exported");
            return;
        };
        let write_dump: MiniDumpWriteDumpFn = std::mem::transmute(proc_);
        let f = std::fs::File::create(&path);
        let Ok(f) = f else {
            crash_log("minidump: cannot create .dmp file");
            return;
        };
        use std::os::windows::io::AsRawHandle;
        let ok = write_dump(
            GetCurrentProcess().0 as isize,
            GetCurrentProcessId(),
            f.as_raw_handle() as isize,
            0, // MiniDumpNormal
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        );
        crash_log(&format!(
            "minidump {} -> {} ({loc})",
            if ok != 0 { "written" } else { "failed" },
            path.display()
        ));
    }
}
