//! Declarative Luxe Dark scene graph -> draw-instance list consumed by
//! `renderer::UiRenderer`. Zone A (right dock), Zone B (HUD pill),
//! text with the GDI atlas.

use super::atlas::FontAtlas;
use super::theme::*;
use crate::i18n::{tr, Lang};

/// One draw instance — 64 bytes, matches `UI_VS_SRC` `struct Inst`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Instance {
    pub pos: [f32; 2],
    pub size: [f32; 2],
    pub uv: [f32; 4],
    pub color: [f32; 4],
    pub radius: f32,
    pub pad: [f32; 3],
}

impl Instance {
    pub fn rect(pos: [f32; 2], size: [f32; 2], color: [f32; 4], radius: f32) -> Self {
        Instance { pos, size, uv: [0.0; 4], color, radius, pad: [0.0; 3] }
    }
    pub fn glyph(pos: [f32; 2], size: [f32; 2], uv: [f32; 4], color: [f32; 4]) -> Self {
        Instance { pos, size, uv, color, radius: 0.0, pad: [0.0; 3] }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HudStats {
    pub fps: u32,
    pub frame_ms: f32,
    /// e.g. "DX12 Zero-Copy | WARP"
    pub backend: &'static str,
}

impl Default for HudStats {
    fn default() -> Self {
        HudStats { fps: 0, frame_ms: 0.0, backend: "DX12" }
    }
}

/// Layout a string into glyph quads starting at pen position `x,y`
/// (baseline at y+ascent). Returns the pen's trailing x.
pub fn layout_text(
    atlas: &FontAtlas,
    text: &str,
    x: f32,
    y: f32,
    color: [f32; 4],
    out: &mut Vec<Instance>,
) -> f32 {
    let mut pen = x;
    for ch in text.chars() {
        let cp = ch as u16;
        if let Some(g) = atlas.glyphs.get(&cp) {
            if g.size[0] > 0.0 && g.size[1] > 0.0 {
                let gx = pen + g.origin[0];
                let gy = y + atlas.ascent as f32 - g.origin[1];
                out.push(Instance::glyph([gx, gy], g.size, g.uv, color));
            }
            pen += g.advance.max(1.0);
        } else if let Some(g) = atlas.glyphs.get(&('?' as u16)) {
            pen += g.advance.max(1.0);
        }
    }
    pen
}

/// Measure string advance width without emitting.
pub fn text_width(atlas: &FontAtlas, text: &str) -> f32 {
    let mut w = 0.0f32;
    for ch in text.chars() {
        if let Some(g) = atlas.glyphs.get(&(ch as u16)) {
            w += g.advance.max(1.0);
        }
    }
    w
}

/// Build the whole Luxe Dark overlay for frame `sw`x`sh`.
/// `stats.backend` is a static string (e.g. "WARP").
pub fn build_scene(
    sw: f32,
    sh: f32,
    lang: Lang,
    atlas: &FontAtlas,
    stats: &HudStats,
) -> Vec<Instance> {
    let mut v = Vec::with_capacity(256);

    // ---- Zone A: right-side dock (auto-hide in fullscreen: drawn always here)
    let dock_x = sw - DOCK_WIDTH - DOCK_MARGIN;
    let dock_y = sh * 0.5 - 140.0;
    let dock_h = 280.0;
    v.push(Instance::rect(
        [dock_x, dock_y],
        [DOCK_WIDTH, dock_h],
        BG_SURFACE,
        R_CHILD,
    ));

    // Icon slots (abstract glyphs — real icons can replace later).
    let lang_label: &str = if lang == Lang::Ru { "RU" } else { "EN" };
    let labels = ["F1", "F2", "APK", "SCR", lang_label];
    for (i, _) in labels.iter().enumerate() {
        let iy = dock_y + 12.0 + i as f32 * (DOCK_ICON + ITEM_SPACING);
        let hover = i == 4; // lang pill highlighted
        v.push(Instance::rect(
            [dock_x + (DOCK_WIDTH - DOCK_ICON) / 2.0, iy],
            [DOCK_ICON, DOCK_ICON],
            if hover { ACCENT } else { BORDER },
            R_FRAME,
        ));
    }

    // ---- Zone B: floating performance pill (top-left)
    let hud_text = format!("{} FPS | {:.1} ms | {}", stats.fps, stats.frame_ms, stats.backend);
    let tw = text_width(atlas, &hud_text);
    let pill_w = tw + PAD_FRAME_X * 2.0;
    v.push(Instance::rect(
        [16.0, 16.0],
        [pill_w, HUD_HEIGHT],
        BG_PRIMARY,
        R_PILL,
    ));
    layout_text(
        atlas,
        &hud_text,
        16.0 + PAD_FRAME_X,
        16.0 + (HUD_HEIGHT - atlas.size_px as f32) / 2.0,
        STATUS_OK,
        &mut v,
    );

    // ---- Dock labels (inside Zone A for simplicity of draw order)
    for (i, label) in labels.iter().enumerate() {
        let iy = dock_y + 12.0 + i as f32 * (DOCK_ICON + ITEM_SPACING);
        let lw = text_width(atlas, label);
        layout_text(
            atlas,
            label,
            dock_x + (DOCK_WIDTH - lw) / 2.0,
            iy + (DOCK_ICON - atlas.size_px as f32) / 2.0,
            if i == 4 { BG_PRIMARY } else { TEXT_PRIMARY },
            &mut v,
        );
    }

    // ---- Localized status line (proves instant RU/EN hot-swap):
    // the exact same layout pass emits different glyphs per language.
    let status = tr(lang, "ui.phase2");
    let stw = text_width(atlas, status);
    let stx = 16.0;
    let sty = 16.0 + HUD_HEIGHT + ITEM_SPACING;
    v.push(Instance::rect(
        [stx - PAD_FRAME_X, sty - 2.0],
        [stw + PAD_FRAME_X * 2.0, atlas.size_px as f32 + 8.0],
        BG_SURFACE,
        R_FRAME,
    ));
    layout_text(atlas, status, stx, sty, TEXT_MUTED, &mut v);

    v
}
