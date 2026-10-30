//! Zone — Quick Settings panel (spec §3.6, toggled via F2/dock):
//! CAS sharpness slider (live), resolution presets, FPS cap.

/// Resolution presets shown in the panel.
pub const RES_PRESETS: [(u32, u32); 3] = [(1920, 1080), (2560, 1440), (3840, 2160)];
/// FPS caps shown in the panel.
pub const FPS_PRESETS: [u32; 3] = [60, 120, 240];

#[derive(Default)]
pub struct SettingsPanel {
    pub open: bool,
    /// `true` while the pointer is scrubbing the CAS slider.
    pub sliding: bool,
}

impl SettingsPanel {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if !self.open {
            self.sliding = false;
        }
    }
}
