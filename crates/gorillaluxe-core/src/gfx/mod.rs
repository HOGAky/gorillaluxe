//! DirectX 12 graphics engine (Phase 2).
//!
//! * `device` — D3D12 device/queue/fence, HW adapter + WARP (`--ci-software-gpu`)
//! * `shader` — runtime HLSL compilation via `d3dcompiler_47.dll`
//! * `pattern` — procedural guest-framebuffer stub (GPU write, zero CPU copy)
//! * `cas` — AMD FidelityFX CAS sharpening compute pass (+ Rust reference)
//! * `buffers` — shared handles, readback heaps, upload helpers
//! * `composite` — the compositor: guest texture -> CAS -> target -> UI overlay
//! * `swapchain` — DXGI 1.6 flip-model swap chain + frame-latency waitable
//! * `selftest` — Automated Verification Test 2

pub mod buffers;
pub mod cas;
pub mod composite;
pub mod device;
pub mod pattern;
pub mod selftest;
pub mod shader;
pub mod swapchain;

use std::sync::atomic::{AtomicU64, Ordering};

/// Global GPU-path accounting. `steady_state_cpu_copy_bytes` counts any
/// host-side CPU pixel copy performed during steady-state rendering;
/// the Phase 2 contract requires it to remain zero (zero-copy pipeline).
pub struct GpuStats {
    pub steady_state_cpu_copy_bytes: AtomicU64,
    pub verification_copy_bytes: AtomicU64,
}

static STATS: GpuStats = GpuStats {
    steady_state_cpu_copy_bytes: AtomicU64::new(0),
    verification_copy_bytes: AtomicU64::new(0),
};

pub fn stats() -> &'static GpuStats {
    &STATS
}

pub fn reset_steady_state_counter() {
    STATS.steady_state_cpu_copy_bytes.store(0, Ordering::SeqCst);
}

#[allow(dead_code)] // hot path must never call this — spec metric stays 0
pub(crate) fn count_steady_state_bytes(n: u64) {
    STATS
        .steady_state_cpu_copy_bytes
        .fetch_add(n, Ordering::SeqCst);
}

pub(crate) fn count_verification_bytes(n: u64) {
    STATS.verification_copy_bytes.fetch_add(n, Ordering::SeqCst);
}

pub type GResult<T> = Result<T, String>;

pub fn hr(ctx: &str, e: windows::core::Error) -> String {
    format!("{} failed: {}", ctx, e)
}
