//! Performance forensics — `logs/perf_debug.log` next to the main
//! log, structured `key=value` lines so a slow session can be diffed
//! against a fast one without reading prose. Every line also lands
//! in `gorillaluxe.log` (UI LOG panel + boot log mirror) so the same
//! diagnosis is reachable from inside the app.
//!
//! What gets instrumented (spec: phase-1 diagnostics):
//!   * `host.*`        — WHPX probe, RAM, core count, all DXGI adapters
//!   * `launch.*`      — requested vs effective `-gpu` / `-accel`
//!   * `accel.active`  — parsed from emulator.log: whpx | tcg | none
//!   * `guest.gles`    — the renderer SurfaceFlinger actually bound
//!   * `capture.*`     — per-frame grab latency + active capture mode
//!   * `delivered.fps` — frames/sec reaching the compositor
//!
//! Live values are also published through atomics so the HUD can
//! render `DX12|<adapter>|grpc 1.2ms|WHPX|GLES:host` every frame.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::data_dir;

/// `perf_debug.log` is truncated once per process — a stale report
/// from a previous run must never be confused with this boot's data.
static RESET_DONE: AtomicU32 = AtomicU32::new(0);

/// Guest GL renderer string reported by SurfaceFlinger
/// (`GLES: Android Emulator EGL ...` / `SwiftShader`). Empty until
/// [`probe_guest_renderer`] runs after boot_completed.
static GUEST_GLES: Mutex<String> = Mutex::new(String::new());
/// `0` unknown, `1` WHPX operational, `2` TCG/software, `3` KVM.
static ACCEL_ACTIVE: AtomicU32 = AtomicU32::new(0);
/// Exponential moving average of frame-grab latency in microseconds
/// (alpha 1/8) — proves whether the transmission path is a bottleneck.
static CAPTURE_US_EMA: AtomicU64 = AtomicU64::new(0);
/// 0 none, 1 grpc, 2 raw-adb, 3 png-adb.
static CAPTURE_MODE: AtomicU32 = AtomicU32::new(0);
/// Frames pushed into the compositor during the last full second.
static DELIVERED_FPS: AtomicU32 = AtomicU32::new(0);
/// Host compositor adapter name (set once when Dx12 is created).
static HOST_ADAPTER: Mutex<String> = Mutex::new(String::new());

fn perf_path() -> std::path::PathBuf {
    data_dir().join("logs").join("perf_debug.log")
}

/// Append one `perf key=value` line — mirrors into the main log so
/// the UI panel shows it too.
pub fn perf_line(msg: &str) {
    if RESET_DONE
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        let p = perf_path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::remove_file(&p);
        header("perf_debug — GorillaLuxe performance forensics");
    }
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("[{secs}] {msg}");
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(perf_path())
    {
        let _ = writeln!(f, "{line}");
    }
    super::android_runtime::log_line(&format!("perf | {msg}"));
}

fn header(msg: &str) {
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(perf_path())
    {
        let _ = writeln!(f, "# {msg}");
    }
}

// ---------------------------------------------------------------------------
// Phase 1a — host environment
// ---------------------------------------------------------------------------

/// One-shot host report: WHPX presence, RAM, cores, and EVERY DXGI
/// adapter (hybrid laptops must prove which GPU surfaces actually
/// exist before blaming a wrong-adapter pick).
pub fn log_host_environment(spec: &crate::config::SpecProfile) {
    let rep = super::env_doctor::diagnose_host();
    perf_line(&format!(
        "host.whpx_ok={} host.whpx_detail={}",
        rep.whpx_ok,
        rep.detail.as_deref().unwrap_or("none")
    ));
    perf_line(&format!(
        "host.ram_mb={} host.cores={}",
        super::env_doctor::host_ram_mb(),
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    ));
    perf_line(&format!(
        "spec.guest_mem_mb={} spec.guest_cores={} spec.lcd={}x{}@{} spec.fps_cap={}",
        spec.guest_mem_mb, spec.guest_cores, spec.lcd_w, spec.lcd_h, spec.lcd_dpi, spec.fps_cap
    ));
    for (i, (name, vram_mb, sw)) in crate::gfx::device::enumerate_adapters().iter().enumerate() {
        perf_line(&format!(
            "host.adapter[{i}] name=\"{name}\" vram_mb={vram_mb} software={sw}"
        ));
    }
}

/// Record which adapter the compositor bound — the "hybrid laptop on
/// iGPU" check needs the actual choice, not the candidate list.
pub fn set_host_adapter(name: &str) {
    *HOST_ADAPTER.lock().unwrap_or_else(|e| e.into_inner()) = name.to_string();
    perf_line(&format!("host.adapter.chosen=\"{name}\""));
}

// ---------------------------------------------------------------------------
// Phase 1b — launch path: requested vs effective acceleration
// ---------------------------------------------------------------------------

pub fn log_launch(gpu_chain: &[&str], accel: &str) {
    perf_line(&format!(
        "launch.accel_requested={accel} launch.gpu_chain={}",
        gpu_chain.join(",")
    ));
}

/// After the engine is stable, parse `emulator.log` for the REAL
/// accelerator that engaged. `-accel on` cannot fall back to TCG —
/// the risk path is our `accel=auto` retry. A TCG guest is a fatal
/// performance event: surface it loudly.
pub fn verify_accel_engaged(accel_requested: &str) {
    let log =
        std::fs::read_to_string(data_dir().join("logs").join("emulator.log")).unwrap_or_default();
    let whpx = log.contains("Windows Hypervisor Platform accelerator is operational")
        || log.contains("-enable-whpx");
    let tcg = log.contains("tcg,")
        || log.contains("AccelMode=TCG")
        || log.contains("whpx not installed")
        || log.contains("WHPX: is not available");
    let mode = if whpx {
        "whpx"
    } else if tcg {
        "tcg"
    } else {
        "unknown"
    };
    ACCEL_ACTIVE.store(
        match mode {
            "whpx" => 1,
            "tcg" => 2,
            _ => 0,
        },
        Ordering::Release,
    );
    perf_line(&format!(
        "accel.requested={accel_requested} accel.active={mode}"
    ));
    if mode == "tcg" {
        super::android_runtime::log_line(
            "FATAL-PERF: guest is running under TCG software CPU emulation — \
             every instruction is interpreted (expect ~1-5 FPS). Enable \
             Windows Hypervisor Platform and relaunch.",
        );
    }
}

/// What the accel verifier found — for the HUD/status line.
pub fn accel_label() -> &'static str {
    match ACCEL_ACTIVE.load(Ordering::Acquire) {
        1 => "WHPX",
        2 => "TCG!",
        _ => "?",
    }
}

/// `true` when the guest is on software CPU — callers can surface a
/// persistent UI warning (never silent).
pub fn accel_is_tcg() -> bool {
    ACCEL_ACTIVE.load(Ordering::Acquire) == 2
}

// ---------------------------------------------------------------------------
// Phase 1c — guest rendering engine
// ---------------------------------------------------------------------------

/// After `sys.boot_completed` ask SurfaceFlinger which GL renderer
/// the guest bound. `SwiftShader`/`llvmpipe`/`softpipe` = all frames
/// rasterised on the CPU — the classic "game won't even start" cause.
pub fn probe_guest_renderer() {
    let out = super::android_runtime::adb_run(&[
        "-e",
        "shell",
        "dumpsys SurfaceFlinger | grep -iE 'GLES:|GL_RENDERER|vendor' | head -3",
    ])
    .unwrap_or_default();
    let gles = out
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("unknown")
        .to_string();
    *GUEST_GLES.lock().unwrap_or_else(|e| e.into_inner()) = gles.clone();
    let software = ["swiftshader", "llvmpipe", "softpipe", "swangle"]
        .iter()
        .any(|s| gles.to_lowercase().contains(s));
    perf_line(&format!(
        "guest.gles=\"{gles}\" guest.gles_software={software}"
    ));
    if software {
        super::android_runtime::log_line(
            "WARN-PERF: guest renderer is SOFTWARE — all composition \
             runs on host CPU. Games/Chrome will be slow regardless \
             of host hardware; check gpu_chain/-gpu selection.",
        );
    }
}

/// Short guest GL label for the HUD ("" until probed).
pub fn guest_gles_label() -> String {
    let g = GUEST_GLES.lock().unwrap_or_else(|e| e.into_inner()).clone();
    // "GLES: Android Emulator EGL Translator (SwiftShader..." → keep
    // the renderer token, trim the boilerplate.
    g.replace("GLES:", "").trim().chars().take(40).collect()
}

// ---------------------------------------------------------------------------
// Phase 1d — frame transmission: per-capture latency + delivered fps
// ---------------------------------------------------------------------------

/// Called by the frame pump on every successful grab. `us` = wall
/// time of the capture call; `mode` = 1 grpc / 2 raw / 3 png.
pub fn note_capture(us: u64, mode: u32) {
    CAPTURE_MODE.store(mode, Ordering::Relaxed);
    let prev = CAPTURE_US_EMA.load(Ordering::Relaxed);
    let ema = if prev == 0 {
        us
    } else {
        prev - prev / 8 + us / 8
    };
    CAPTURE_US_EMA.store(ema, Ordering::Relaxed);
}

/// Rolling FPS of frames actually delivered to the compositor —
/// called once a second by the pump/consumer.
pub fn note_delivered_fps(fps: u32) {
    DELIVERED_FPS.store(fps, Ordering::Relaxed);
}

/// Periodic perf heartbeat (called from the pump every ~10 s).
pub fn log_frame_stats() {
    perf_line(&format!(
        "capture.mode={} capture.avg_ms={:.1} delivered.fps={}",
        capture_mode_label(),
        CAPTURE_US_EMA.load(Ordering::Relaxed) as f64 / 1000.0,
        DELIVERED_FPS.load(Ordering::Relaxed),
    ));
}

pub fn capture_mode_label() -> &'static str {
    match CAPTURE_MODE.load(Ordering::Relaxed) {
        1 => "grpc",
        2 => "raw-adb",
        3 => "png-adb",
        _ => "none",
    }
}

/// HUD backend string: `DX12|Intel UHD|grpc 1.4ms|WHPX|GLES:host`.
pub fn hud_backend(is_warp: bool) -> String {
    let adapter = HOST_ADAPTER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let adapter = if adapter.is_empty() {
        if is_warp {
            "WARP".to_string()
        } else {
            "GPU".to_string()
        }
    } else {
        adapter
    };
    let ms = CAPTURE_US_EMA.load(Ordering::Relaxed) as f64 / 1000.0;
    let gles = guest_gles_label();
    format!(
        "DX12|{adapter}|{} {ms:.1}ms|{}|{}",
        capture_mode_label(),
        accel_label(),
        if gles.is_empty() { "GLES:?" } else { &gles },
    )
}
