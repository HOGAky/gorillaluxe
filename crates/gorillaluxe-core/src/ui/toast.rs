//! Animated Luxe Dark toasts — used by the `WM_DROPFILES` APK
//! installer (drag a .apk/.xapk onto the window) and env-doctor
//! notifications. Progress animates on the render clock.

use std::time::Instant;

pub struct Toast {
    pub title: String,
    /// 0.0..1.0 — animates automatically over `duration_ms`.
    pub progress: f32,
    pub created: Instant,
    pub duration_ms: u64,
    /// Extra hold time at 100 % before the toast is dropped.
    pub hold_ms: u64,
    pub done: bool,
}

impl Toast {
    pub fn new(title: String, duration_ms: u64) -> Self {
        Toast {
            title,
            progress: 0.0,
            created: Instant::now(),
            duration_ms: duration_ms.max(200),
            hold_ms: 1500,
            done: false,
        }
    }

    /// Advance animation; call once per frame.
    pub fn tick(&mut self) {
        let el = self.created.elapsed().as_millis() as f32;
        self.progress = (el / self.duration_ms as f32).clamp(0.0, 1.0);
        if el > self.duration_ms as f32 + self.hold_ms as f32 {
            self.done = true;
        }
    }
}

#[derive(Default)]
pub struct Toasts {
    pub items: Vec<Toast>,
}

impl Toasts {
    pub fn push(&mut self, title: String, duration_ms: u64) {
        self.items.push(Toast::new(title, duration_ms));
        if self.items.len() > 4 {
            self.items.remove(0);
        }
    }

    pub fn tick(&mut self) {
        for t in &mut self.items {
            t.tick();
        }
        self.items.retain(|t| !t.done);
    }
}
