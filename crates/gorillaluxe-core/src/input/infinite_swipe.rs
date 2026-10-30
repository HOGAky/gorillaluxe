//! FPS "Infinite Swipe" state machine (spec §3.2).
//!
//! While FPS mode is active, host relative mouse deltas drive a
//! virtual touch contact on MT slot 0. When the virtual position
//! reaches within 10 % of any screen edge the contact is released and
//! re-anchored at `(0.75·W, 0.50·H)` on the next 1 ms tick — so camera
//! rotation never stalls against the screen border.
//!
//! Time is caller-provided in **tick units** (1 unit = 1 ms), which
//! makes the boundary-reset latency provable in the Phase 3 harness.

use super::virtio_input::{EvDevEvent, TouchScreen};

const SLOT: usize = 0;

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Idle,
    Tracking,
    /// Released at `at_tick`; re-touch + apply `carry` on tick > at_tick.
    PendingReset { carry: (f32, f32), at_tick: u64 },
}

pub struct InfiniteSwipe {
    w: f32,
    h: f32,
    /// Anchor (0.75·W, 0.50·H).
    x0: f32,
    y0: f32,
    /// Current virtual position.
    x: f32,
    y: f32,
    /// 10 % edge margins.
    mx: f32,
    my: f32,
    state: State,
    touch: TouchScreen,
    /// Diagnostics for the acceptance harness.
    pub reset_count: u32,
    /// Ticks between TRACKING_ID=-1 release and next assignment.
    pub last_reset_gap: u64,
}

impl InfiniteSwipe {
    pub fn new(w: u32, h: u32) -> Self {
        let wf = w as f32;
        let hf = h as f32;
        InfiniteSwipe {
            w: wf,
            h: hf,
            x0: 0.75 * wf,
            y0: 0.50 * hf,
            x: 0.75 * wf,
            y: 0.50 * hf,
            mx: 0.10 * wf,
            my: 0.10 * hf,
            state: State::Idle,
            touch: TouchScreen::new(10),
            reset_count: 0,
            last_reset_gap: 0,
        }
    }

    pub fn is_active(&self) -> bool {
        self.state != State::Idle
    }

    /// Enter FPS mode: slot 0 goes down at the anchor point.
    pub fn begin(&mut self, tick: u64) -> Vec<EvDevEvent> {
        self.x = self.x0;
        self.y = self.y0;
        self.state = State::Tracking;
        self.touch.touch_down(SLOT, self.x as i32, self.y as i32, tick);
        self.touch.drain()
    }

    /// Leave FPS mode: release the contact.
    pub fn end(&mut self, tick: u64) -> Vec<EvDevEvent> {
        self.state = State::Idle;
        self.touch.touch_up(SLOT, tick);
        self.touch.drain()
    }

    /// Feed one tick of relative mouse motion `(dx, dy)` at `tick`
    /// (1 unit = 1 ms). Returns the evdev batch for the tick.
    pub fn feed(&mut self, dx: f32, dy: f32, tick: u64) -> Vec<EvDevEvent> {
        let mut carry_dx = 0.0f32;
        let mut carry_dy = 0.0f32;
        if let State::PendingReset { carry, at_tick } = self.state {
            if tick > at_tick {
                // Spec step 3: new TRACKING_ID at anchor on the next 1 ms
                // tick, then apply the carried-over delta.
                self.touch.touch_down(SLOT, self.x0 as i32, self.y0 as i32, tick);
                self.last_reset_gap = tick - at_tick;
                self.x = self.x0;
                self.y = self.y0;
                carry_dx = carry.0;
                carry_dy = carry.1;
                self.state = State::Tracking;
            } else {
                return self.touch.drain();
            }
        }
        if self.state != State::Tracking {
            return self.touch.drain();
        }

        let nx = self.x + carry_dx + dx;
        let ny = self.y + carry_dy + dy;
        let out = nx < self.mx || nx > self.w - self.mx || ny < self.my || ny > self.h - self.my;
        if out {
            // Spec steps 1-2: release slot 0, reset to anchor; leftover
            // delta past the edge is carried to the re-touch tick.
            let bx = nx.clamp(self.mx, self.w - self.mx);
            let by = ny.clamp(self.my, self.h - self.my);
            self.touch.touch_move(SLOT, bx as i32, by as i32);
            self.touch.touch_up(SLOT, tick);
            self.x = bx;
            self.y = by;
            self.reset_count += 1;
            self.state = State::PendingReset { carry: (nx - bx, ny - by), at_tick: tick };
        } else {
            self.touch.touch_move(SLOT, nx as i32, ny as i32);
            self.x = nx;
            self.y = ny;
        }
        self.touch.drain()
    }

    /// Current virtual position (rounded to evdev units).
    pub fn pos(&self) -> (i32, i32) {
        (self.x as i32, self.y as i32)
    }

    pub fn screen(&self) -> (u32, u32) {
        (self.w as u32, self.h as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::virtio_input::*;

    #[test]
    fn resets_at_border() {
        let mut s = InfiniteSwipe::new(1920, 1080);
        s.begin(0);
        let mut resets = 0;
        for t in 1..=200u64 {
            let ev = s.feed(60.0, 0.0, t);
            if ev.iter().any(|e| e.code == ABS_MT_TRACKING_ID && e.value == TRACKING_ID_RELEASE) {
                resets += 1;
            }
        }
        assert!(resets > 0);
        assert!(s.last_reset_gap <= 2, "slot reset took {} ticks", s.last_reset_gap);
    }
}
