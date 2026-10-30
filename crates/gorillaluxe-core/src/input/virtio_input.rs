//! Linux Multi-Touch Protocol Type B model (spec §3.2).
//!
//! The guest-facing virtio-input device exposes MT slots; host code
//! pushes `EvDevEvent`s which a future virtio-input transport forwards
//! to the guest kernel. The slot machine is pure logic — identical
//! semantics to `drivers/input` MT-B handling.

/// evdev event types (linux/input-event-codes.h).
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;

/// evdev codes used by the emulator.
pub const SYN_REPORT: u16 = 0x00;
pub const BTN_TOUCH: u16 = 0x14a;
pub const BTN_MOUSE: u16 = 0x110;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_PRESSURE: u16 = 0x18;

/// Release value for ABS_MT_TRACKING_ID.
pub const TRACKING_ID_RELEASE: i32 = -1;

/// One evdev event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvDevEvent {
    pub ev_type: u16,
    pub code: u16,
    pub value: i32,
}

impl EvDevEvent {
    pub const fn new(ev_type: u16, code: u16, value: i32) -> Self {
        EvDevEvent { ev_type, code, value }
    }
    pub const fn syn() -> Self {
        EvDevEvent::new(EV_SYN, SYN_REPORT, 0)
    }
    pub const fn slot(s: u32) -> Self {
        EvDevEvent::new(EV_ABS, ABS_MT_SLOT, s as i32)
    }
    pub const fn tracking(id: i32) -> Self {
        EvDevEvent::new(EV_ABS, ABS_MT_TRACKING_ID, id)
    }
    pub const fn pos_x(x: i32) -> Self {
        EvDevEvent::new(EV_ABS, ABS_MT_POSITION_X, x)
    }
    pub const fn pos_y(y: i32) -> Self {
        EvDevEvent::new(EV_ABS, ABS_MT_POSITION_Y, y)
    }
    pub const fn touch(on: bool) -> Self {
        EvDevEvent::new(EV_KEY, BTN_TOUCH, on as i32)
    }
}

/// One MT-B contact slot.
#[derive(Clone, Copy, Debug)]
pub struct MtSlot {
    /// `None` = free; `Some(id)` = active tracking ID.
    pub tracking_id: Option<i32>,
    pub x: i32,
    pub y: i32,
}

impl Default for MtSlot {
    fn default() -> Self {
        MtSlot { tracking_id: None, x: 0, y: 0 }
    }
}

/// Multi-touch slot tracker (Type B semantics).
/// The caller appends the produced events onto the virtio-input
/// queue; `drain()` returns the next batch.
pub struct TouchScreen {
    pub slots: Vec<MtSlot>,
    active: usize,
    next_id: i32,
    events: Vec<EvDevEvent>,
    /// Diagnostics for the Phase 3 harness: timestamps (in caller
    /// ticks) of the last TRACKING_ID transitions per slot.
    pub last_release_tick: u64,
    pub last_assign_tick: u64,
}

impl TouchScreen {
    pub fn new(max_slots: usize) -> Self {
        TouchScreen {
            slots: vec![MtSlot::default(); max_slots.max(1)],
            active: 0,
            next_id: 1,
            events: Vec::with_capacity(64),
            last_release_tick: 0,
            last_assign_tick: 0,
        }
    }

    fn select(&mut self, slot: usize) {
        if self.active != slot {
            self.active = slot;
            self.events.push(EvDevEvent::slot(slot as u32));
        }
    }

    /// Begin a contact at `(x, y)` on `slot`. Emits
    /// `ABS_MT_SLOT`, `ABS_MT_TRACKING_ID`, X/Y, `BTN_TOUCH`, `SYN_REPORT`.
    pub fn touch_down(&mut self, slot: usize, x: i32, y: i32, tick: u64) {
        self.select(slot);
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.slots[slot].tracking_id = Some(id);
        self.slots[slot].x = x;
        self.slots[slot].y = y;
        self.last_assign_tick = tick;
        self.events.push(EvDevEvent::tracking(id));
        self.events.push(EvDevEvent::pos_x(x));
        self.events.push(EvDevEvent::pos_y(y));
        self.events.push(EvDevEvent::touch(true));
        self.events.push(EvDevEvent::syn());
    }

    /// Move the contact on `slot` (no-op when the slot is free).
    pub fn touch_move(&mut self, slot: usize, x: i32, y: i32) {
        if self.slots[slot].tracking_id.is_none() {
            return;
        }
        self.select(slot);
        self.slots[slot].x = x;
        self.slots[slot].y = y;
        self.events.push(EvDevEvent::pos_x(x));
        self.events.push(EvDevEvent::pos_y(y));
        self.events.push(EvDevEvent::syn());
    }

    /// Release the contact on `slot` (`TRACKING_ID = -1`).
    pub fn touch_up(&mut self, slot: usize, tick: u64) {
        if self.slots[slot].tracking_id.is_none() {
            return;
        }
        self.select(slot);
        self.slots[slot].tracking_id = None;
        self.last_release_tick = tick;
        self.events.push(EvDevEvent::tracking(TRACKING_ID_RELEASE));
        self.events.push(EvDevEvent::touch(false));
        self.events.push(EvDevEvent::syn());
    }

    /// Take the pending event batch.
    pub fn drain(&mut self) -> Vec<EvDevEvent> {
        std::mem::take(&mut self.events)
    }

    /// Total events ever produced (diagnostics).
    pub fn tracking_id_of(&self, slot: usize) -> Option<i32> {
        self.slots[slot].tracking_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mt_b_sequence() {
        let mut t = TouchScreen::new(4);
        t.touch_down(0, 100, 200, 0);
        t.touch_move(0, 150, 200);
        t.touch_up(0, 1);
        let ev = t.drain();
        assert!(ev.contains(&EvDevEvent::tracking(TRACKING_ID_RELEASE)));
        assert!(ev.iter().any(|e| e.code == ABS_MT_TRACKING_ID && e.value > 0));
        assert_eq!(t.tracking_id_of(0), None);
    }
}
