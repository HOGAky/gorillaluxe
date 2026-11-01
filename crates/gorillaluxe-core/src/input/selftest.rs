//! Automated Verification Test 3 (spec §4, PHASE 3):
//!   1. Inject 10,000 px of continuous horizontal mouse motion into
//!      the Infinite Swipe state machine on 1 ms ticks.
//!   2. Verify smooth coordinate progression, seamless MT-B slot
//!      resets (`ABS_MT_TRACKING_ID` cycling in < 2 ms), no stall at
//!      screen bounds, and distinct tracking IDs per reset.
//!   3. `keymap.json` save/load roundtrip.
//!   4. WASAPI shared-mode initialization (graceful fallback on
//!      headless runners without an audio endpoint).

use std::collections::HashSet;

use crate::audio::wasapi::AudioOutput;
use crate::ui::keymapper::{Keymapper, WidgetKind};

use super::infinite_swipe::InfiniteSwipe;
use super::virtio_input::*;

#[derive(Debug)]
pub struct Report {
    pub total_dx_px: f32,
    pub ticks: u64,
    pub resets: u32,
    /// Worst-case ticks between a TRACKING_ID release and the next
    /// assignment (1 tick = 1 ms). Acceptance: <= 2.
    pub max_reset_gap_ticks: u64,
    pub distinct_ids: usize,
    pub events: usize,
    /// Ticks during active tracking that produced no position event
    /// despite a non-zero delta (screen-bound stall detector).
    pub stalled_ticks: u64,
    pub keymap_roundtrip: bool,
    pub wasapi_ready: bool,
    pub wasapi_period_ms: f64,
    pub wasapi_detail: String,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.resets > 0
            && self.max_reset_gap_ticks <= 2
            && self.stalled_ticks == 0
            && self.distinct_ids >= self.resets as usize
            && self.keymap_roundtrip
            && self.wasapi_attempted()
    }
    fn wasapi_attempted(&self) -> bool {
        self.wasapi_ready || !self.wasapi_detail.is_empty()
    }
}

/// Drive `dx_px` per tick until `target_px` cumulative motion.
fn swipe_harness(target_px: f32, dx_px: f32) -> (u64, u32, u64, usize, usize, u64) {
    let mut sw = InfiniteSwipe::new(1920, 1080);
    let mut all: Vec<EvDevEvent> = Vec::new();
    all.extend(sw.begin(0));
    let mut tick = 0u64;
    let mut traveled = 0.0f32;
    let mut stalled = 0u64;
    let mut release_tick = 0u64;
    let mut awaiting_assign = false;
    let mut max_gap = 0u64;
    let mut ids = HashSet::new();

    while traveled < target_px {
        tick += 1;
        let ev = sw.feed(dx_px, 0.0, tick);
        traveled += dx_px;
        if sw.is_active() && dx_px > 0.0 && ev.is_empty() {
            stalled += 1;
        }
        for e in &ev {
            if e.code == ABS_MT_TRACKING_ID {
                if e.value == TRACKING_ID_RELEASE {
                    release_tick = tick;
                    awaiting_assign = true;
                } else {
                    ids.insert(e.value);
                    if awaiting_assign {
                        max_gap = max_gap.max(tick - release_tick);
                        awaiting_assign = false;
                    }
                }
            }
        }
        all.extend(ev);
    }
    // Terminal release for a clean event stream.
    all.extend(sw.end(tick + 1));
    (tick, sw.reset_count, max_gap, ids.len(), all.len(), stalled)
}

fn keymap_roundtrip() -> bool {
    let mut km = Keymapper::default();
    let a = km.place(WidgetKind::Tap, 0.31, 0.42);
    km.bind(a, 'W' as u16);
    let b = km.place(WidgetKind::Wasd, 0.18, 0.80);
    km.bind(b, 0x20); // VK_SPACE
    let c = km.place(WidgetKind::FpsAim, 0.75, 0.50);
    km.bind(c, 0x01); // VK_LBUTTON
    km.place(WidgetKind::Macro, 0.60, 0.15);
    // Temp path only — the self-test must never touch the user's real
    // %LOCALAPPDATA%\GorillaLuxe\keymap.json.
    let tmp = std::env::temp_dir().join(format!(
        "gorillaluxe-selftest-keymap-{}.json",
        std::process::id()
    ));
    let ok = km.save_to(&tmp).is_ok() && Keymapper::load_from(&tmp).widgets == km.widgets;
    let _ = std::fs::remove_file(&tmp);
    ok
}

fn wasapi_probe() -> (bool, f64, String) {
    match AudioOutput::init() {
        Ok(a) => (
            true,
            a.period_hns as f64 / 10_000.0,
            format!(
                "{} Hz {} ch {} frames {} ms",
                a.sample_rate,
                a.channels,
                a.buffer_frames,
                a.period_hns as f64 / 10_000.0
            ),
        ),
        Err(e) => (false, -1.0, e),
    }
}

/// Run the full Phase 3 acceptance harness.
pub fn run() -> Report {
    // 10,000 px continuous horizontal motion, 32 px per 1 ms tick.
    let (ticks, resets, max_gap, ids, events, stalled) = swipe_harness(10_000.0, 32.0);
    let keymap = keymap_roundtrip();
    let (ready, period_ms, detail) = wasapi_probe();
    Report {
        total_dx_px: 32.0 * ticks as f32,
        ticks,
        resets,
        max_reset_gap_ticks: max_gap,
        distinct_ids: ids,
        events,
        stalled_ticks: stalled,
        keymap_roundtrip: keymap,
        wasapi_ready: ready,
        wasapi_period_ms: period_ms,
        wasapi_detail: detail,
    }
}
