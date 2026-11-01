//! Structured runtime logging — one line per event at
//! `%LOCALAPPDATA%\GorillaLuxe\logs\gorillaluxe.log` with 5 MiB
//! rotation (`gorillaluxe.1.log` keeps the previous segment).
//! Every download, extraction, validation, emulator launch, adb and
//! frame-bridge event lands here so a real-PC failure is diagnosable
//! in under a minute.
//!
//! Additionally every line is mirrored to `gorillaluxe_boot.log`
//! next to the executable (boot diagnostics must travel with the
//! build, not hide in AppData) and to an in-memory ring the UI's LOG
//! panel renders live.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::data_dir;

const MAX_BYTES: u64 = 5 * 1024 * 1024;
/// Lines kept for the UI LOG panel.
const RING_CAP: usize = 400;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Info => "INFO ",
            Level::Warn => "WARN ",
            Level::Error => "ERROR",
        }
    }
}

pub fn log_path() -> PathBuf {
    data_dir().join("logs").join("gorillaluxe.log")
}

/// `gorillaluxe_boot.log` beside the running executable — the spec
/// mandates boot diagnostics next to the binary so a bare release
/// archive is self-diagnosing. `None` when the exe dir is
/// unresolvable/unwritable (LOCALAPPDATA log still gets everything).
fn boot_log_path() -> Option<PathBuf> {
    use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(None, &mut buf) };
    if n == 0 {
        return None;
    }
    let exe = PathBuf::from(String::from_utf16_lossy(&buf[..n as usize]));
    exe.parent().map(|p| p.join("gorillaluxe_boot.log"))
}

fn ring() -> &'static Mutex<VecDeque<String>> {
    static RING: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
    &RING
}

/// Last `n` log lines (oldest→newest) for the UI LOG panel.
pub fn recent(n: usize) -> Vec<String> {
    let r = ring().lock().unwrap_or_else(|e| e.into_inner());
    r.iter().rev().take(n).rev().cloned().collect()
}

/// Called once at process start — each launch owns a fresh
/// `gorillaluxe_boot.log` (spec: create/overwrite, never append to a
/// stale boot's diagnostics).
pub fn boot_log_reset() {
    if let Some(bp) = boot_log_path() {
        let _ = fs::remove_file(&bp);
    }
}

fn rotate_if_needed(path: &PathBuf) {
    if fs::metadata(path)
        .map(|m| m.len() > MAX_BYTES)
        .unwrap_or(false)
    {
        let old = path.with_file_name("gorillaluxe.1.log");
        let _ = fs::remove_file(&old);
        let _ = fs::rename(path, &old);
    }
}

/// Append one timestamped leveled line (best effort, never panics).
pub fn log(level: Level, msg: &str) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("[{secs}] {} {msg}", level.tag());
    // UI ring first — the panel must update even if the disk write fails.
    {
        let mut r = ring().lock().unwrap_or_else(|e| e.into_inner());
        r.push_back(line.clone());
        while r.len() > RING_CAP {
            r.pop_front();
        }
    }
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    rotate_if_needed(&path);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{line}");
    }
    if let Some(bp) = boot_log_path() {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&bp) {
            let _ = writeln!(f, "{line}");
        }
    }
}

pub fn info(msg: &str) {
    log(Level::Info, msg);
}
pub fn warn(msg: &str) {
    log(Level::Warn, msg);
}
pub fn error(msg: &str) {
    log(Level::Error, msg);
}
