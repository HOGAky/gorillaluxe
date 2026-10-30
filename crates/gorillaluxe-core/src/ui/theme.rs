//! "Luxe Dark" design tokens, spec §3.6 — Deep Obsidian + Luxe Amber Gold.

const fn rgba(r: u8, g: u8, b: u8, a: u8) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a as f32 / 255.0]
}

/// Background Primary — Deep Obsidian `#0D0E12` (alpha 242/255).
pub const BG_PRIMARY: [f32; 4] = rgba(13, 14, 18, 242);
/// Background Surface / Cards — Elevated Charcoal `#161821`.
pub const BG_SURFACE: [f32; 4] = rgba(22, 24, 33, 255);
/// Primary Accent — Luxe Amber Gold `#F5A623`.
pub const ACCENT: [f32; 4] = rgba(245, 166, 35, 255);
pub const ACCENT_HOVER: [f32; 4] = rgba(255, 184, 77, 255);
pub const ACCENT_ACTIVE: [f32; 4] = rgba(212, 136, 6, 255);
/// Status indicators.
pub const STATUS_OK: [f32; 4] = rgba(46, 204, 113, 255);
pub const STATUS_WARN: [f32; 4] = rgba(231, 76, 60, 255);
/// Text.
pub const TEXT_PRIMARY: [f32; 4] = rgba(243, 244, 246, 255);
pub const TEXT_MUTED: [f32; 4] = rgba(156, 163, 175, 255);
/// Subtle border `#262936`.
pub const BORDER: [f32; 4] = rgba(38, 41, 54, 255);

// Rounded geometry (px).
pub const R_WINDOW: f32 = 12.0;
pub const R_CHILD: f32 = 10.0;
pub const R_POPUP: f32 = 10.0;
pub const R_FRAME: f32 = 8.0;
pub const R_GRAB: f32 = 6.0;
pub const R_TAB: f32 = 8.0;
pub const R_PILL: f32 = 16.0;

// Spacing (px).
pub const PAD_WINDOW: f32 = 16.0;
pub const PAD_FRAME_X: f32 = 12.0;
pub const PAD_FRAME_Y: f32 = 8.0;
pub const ITEM_SPACING: f32 = 10.0;

// Zone A dock.
pub const DOCK_WIDTH: f32 = 48.0;
pub const DOCK_ICON: f32 = 32.0;
pub const DOCK_MARGIN: f32 = 8.0;

// Zone B HUD pill.
pub const HUD_HEIGHT: f32 = 30.0;

// Font sizes (px): 16 UI, 22 headers (spec §3.5).
pub const FONT_UI: i32 = 16;
pub const FONT_HEADER: i32 = 22;
