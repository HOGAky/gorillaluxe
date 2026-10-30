//! Structured runtime logging — one line per event at
//! `%LOCALAPPDATA%\GorillaLuxe\logs\gorillaluxe.log` with 5 MiB
//! rotation (`gorillaluxe.1.log` keeps the previous segment).
//! Every download, extraction, validation, emulator launch, adb and
//! frame-bridge event lands here so a real-PC failure is diagnosable
//! in under a minute.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::data_dir;

const MAX_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Clone, Copy)]
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
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    rotate_if_needed(&path);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "[{secs}] {} {msg}", level.tag());
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
