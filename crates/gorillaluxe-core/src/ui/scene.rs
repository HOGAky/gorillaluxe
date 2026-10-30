//! Declarative Luxe Dark scene graph -> draw-instance list consumed by
//! `renderer::UiRenderer`. Phase 3 adds mouse interactivity: every
//! visual control emits a `Region` for hit-testing.
//!
//! Zones (spec §3.6): A = right dock, B = HUD pill, C = keymapper
//! canvas, plus Quick Settings panel, APK toasts, WHPX banner.

use super::atlas::FontAtlas;
use super::keymapper::{Keymapper, WidgetKind};
use super::settings_panel::{SettingsPanel, FPS_PRESETS, RES_PRESETS};
use super::theme::*;
use super::toast::Toasts;
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

/// Interactive region emitted for hit-testing.
#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub id: RegionId,
    /// x, y, w, h in pixels.
    pub r: [f32; 4],
}

/// Identifiers for clickable/draggable regions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RegionId {
    DockKeymapper,
    DockSettings,
    DockApk,
    DockShot,
    DockLang,
    CasSlider,
    ResPreset(usize),
    FpsPreset(usize),
    Palette(usize),
    Widget(usize),
    WhpxEnable,
    WhpxRestart,
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

/// WHPX setup banner state (spec: env-doctor card).
#[derive(Clone, Debug)]
pub enum WhpxBanner {
    Hidden,
    /// WHPX unavailable — show golden "Enable in 1 Click" button.
    Missing(String),
    /// Elevated setup finished — show "Restart PC" button.
    RestartReady,
}

/// Full UI frame state (owned by the app, read by `build_frame`).
pub struct UiState {
    pub lang: Lang,
    pub keymap: Keymapper,
    pub settings: SettingsPanel,
    pub toasts: Toasts,
    pub banner: WhpxBanner,
    pub hover: Option<RegionId>,
    /// Cursor position in pixels (for hover + drag ghosts).
    pub mouse: (f32, f32),
    /// CAS sharpness 0.0..1.0 (live -> constant buffer).
    pub sharpness: f32,
    pub res_idx: usize,
    pub fps_idx: usize,
    pub hud_visible: bool,
    pub fps_mode: bool,
    /// Widget kind currently dragged from the Zone C palette.
    pub palette_drag: Option<WidgetKind>,
}

impl Default for UiState {
    fn default() -> Self {
        UiState {
            lang: Lang::En,
            keymap: Keymapper::default(),
            settings: SettingsPanel::default(),
            toasts: Toasts::default(),
            banner: WhpxBanner::Hidden,
            hover: None,
            mouse: (0.0, 0.0),
            sharpness: 0.5,
            res_idx: 0,
            fps_idx: 1,
            hud_visible: true,
            fps_mode: false,
            palette_drag: None,
        }
    }
}

pub struct Frame {
    pub instances: Vec<Instance>,
    pub regions: Vec<Region>,
}

/// Hit-test a point against regions (topmost = last wins).
pub fn hit(regions: &[Region], x: f32, y: f32) -> Option<RegionId> {
    regions.iter().rev().find_map(|r| {
        (x >= r.r[0] && x <= r.r[0] + r.r[2] && y >= r.r[1] && y <= r.r[1] + r.r[3])
            .then_some(r.id)
    })
}

// ---------------------------------------------------------------------------

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

pub fn text_width(atlas: &FontAtlas, text: &str) -> f32 {
    let mut w = 0.0f32;
    for ch in text.chars() {
        if let Some(g) = atlas.glyphs.get(&(ch as u16)) {
            w += g.advance.max(1.0);
        }
    }
    w
}

fn centered_text(
    atlas: &FontAtlas,
    text: &str,
    cx: f32,
    y: f32,
    color: [f32; 4],
    out: &mut Vec<Instance>,
) {
    layout_text(atlas, text, cx - text_width(atlas, text) / 2.0, y, color, out);
}

fn vk_label(vk: u16) -> String {
    match vk {
        0x30..=0x39 | 0x41..=0x5A => ((vk as u8) as char).to_string(),
        0x70..=0x7B => format!("F{}", vk - 0x6F),
        _ => format!("VK{:02X}", vk),
    }
}

const DOCK_BTNS: usize = 5;

/// Dock slot metrics shared by drawing + region emission.
fn dock_layout(sw: f32, sh: f32) -> (f32, f32, f32) {
    let dock_x = sw - DOCK_WIDTH - DOCK_MARGIN;
    let dock_h = DOCK_BTNS as f32 * (DOCK_ICON + ITEM_SPACING) + 16.0;
    let dock_y = sh * 0.5 - dock_h * 0.5;
    (dock_x, dock_y, dock_h)
}

fn slot_rect(dock_x: f32, dock_y: f32, i: usize) -> [f32; 4] {
    [
        dock_x + (DOCK_WIDTH - DOCK_ICON) / 2.0,
        dock_y + 8.0 + i as f32 * (DOCK_ICON + ITEM_SPACING),
        DOCK_ICON,
        DOCK_ICON,
    ]
}

/// Build the whole Luxe Dark overlay for frame `sw`x`sh`.
pub fn build_frame(
    sw: f32,
    sh: f32,
    st: &UiState,
    atlas: &FontAtlas,
    stats: &HudStats,
) -> Frame {
    let mut v: Vec<Instance> = Vec::with_capacity(512);
    let mut rg: Vec<Region> = Vec::with_capacity(64);
    let lang = st.lang;

    // ---- Zone A: right dock ------------------------------------------------
    let (dock_x, dock_y, dock_h) = dock_layout(sw, sh);
    v.push(Instance::rect([dock_x, dock_y], [DOCK_WIDTH, dock_h], BG_SURFACE, R_CHILD));
    let ids = [
        RegionId::DockKeymapper,
        RegionId::DockSettings,
        RegionId::DockApk,
        RegionId::DockShot,
        RegionId::DockLang,
    ];
    let labels = ["F1", "F2", "APK", "SCR", if lang == Lang::Ru { "RU" } else { "EN" }];
    for (i, (id, label)) in ids.iter().zip(labels.iter()).enumerate() {
        let r = slot_rect(dock_x, dock_y, i);
        let hovered = st.hover == Some(*id);
        let accent_slot = *id == RegionId::DockLang;
        v.push(Instance::rect(
            [r[0], r[1]],
            [r[2], r[3]],
            if accent_slot {
                if hovered { ACCENT_HOVER } else { ACCENT }
            } else if hovered {
                ACCENT_ACTIVE
            } else {
                BORDER
            },
            R_FRAME,
        ));
        rg.push(Region { id: *id, r });
        centered_text(
            atlas,
            label,
            r[0] + r[2] / 2.0,
            r[1] + (r[3] - atlas.size_px as f32) / 2.0,
            if accent_slot { BG_PRIMARY } else { TEXT_PRIMARY },
            &mut v,
        );
    }

    // ---- Zone B: HUD performance pill --------------------------------------
    if st.hud_visible {
        let mut hud_text =
            format!("{} FPS | {:.1} ms | {}", stats.fps, stats.frame_ms, stats.backend);
        if st.fps_mode {
            hud_text.push_str(&format!(" | {}", tr(lang, "ui.fps_aim")));
        }
        let tw = text_width(atlas, &hud_text);
        let pill_w = tw + PAD_FRAME_X * 2.0;
        v.push(Instance::rect([16.0, 16.0], [pill_w, HUD_HEIGHT], BG_PRIMARY, R_PILL));
        layout_text(
            atlas,
            &hud_text,
            16.0 + PAD_FRAME_X,
            16.0 + (HUD_HEIGHT - atlas.size_px as f32) / 2.0,
            if stats.fps >= 100 { STATUS_OK } else { TEXT_PRIMARY },
            &mut v,
        );
        let status = tr(lang, "ui.phase2");
        let stw = text_width(atlas, status);
        let sty = 16.0 + HUD_HEIGHT + ITEM_SPACING;
        v.push(Instance::rect(
            [16.0, sty - 2.0],
            [stw + PAD_FRAME_X * 2.0, atlas.size_px as f32 + 8.0],
            BG_SURFACE,
            R_FRAME,
        ));
        layout_text(atlas, status, 16.0 + PAD_FRAME_X, sty, TEXT_MUTED, &mut v);
    }

    // ---- Zone C: keymapper canvas -------------------------------------------
    if st.keymap.open {
        // 40 % dim over the game.
        v.push(Instance::rect(
            [0.0, 0.0],
            [sw, sh],
            [BG_PRIMARY[0], BG_PRIMARY[1], BG_PRIMARY[2], 0.4 * BG_PRIMARY[3]],
            0.0,
        ));
        // Placed widgets — animated golden rings.
        let wr = 28.0f32;
        for (i, w) in st.keymap.widgets.iter().enumerate() {
            let cx = w.x * sw;
            let cy = w.y * sh;
            let sel = st.keymap.selected == Some(i);
            v.push(Instance::rect(
                [cx - wr, cy - wr],
                [wr * 2.0, wr * 2.0],
                if sel { ACCENT_ACTIVE } else { ACCENT },
                wr,
            ));
            v.push(Instance::rect(
                [cx - wr + 3.0, cy - wr + 3.0],
                [wr * 2.0 - 6.0, wr * 2.0 - 6.0],
                BG_SURFACE,
                wr - 3.0,
            ));
            let label = w
                .key
                .map(vk_label)
                .unwrap_or_else(|| w.kind.code().chars().next().unwrap().to_uppercase().to_string());
            centered_text(atlas, &label, cx, cy - atlas.size_px as f32 / 2.0, TEXT_PRIMARY, &mut v);
            rg.push(Region {
                id: RegionId::Widget(i),
                r: [cx - wr, cy - wr, wr * 2.0, wr * 2.0],
            });
        }
        // Palette strip (bottom center).
        let pw = 88.0f32;
        let ph = 44.0f32;
        let gap = ITEM_SPACING;
        let total = WidgetKind::PALETTE.len() as f32 * (pw + gap) - gap;
        let px0 = (sw - total) / 2.0;
        let py = sh - ph - 24.0;
        for (i, kind) in WidgetKind::PALETTE.iter().enumerate() {
            let r = [px0 + i as f32 * (pw + gap), py, pw, ph];
            let hovered = st.hover == Some(RegionId::Palette(i));
            v.push(Instance::rect([r[0], r[1]], [r[2], r[3]], if hovered { ACCENT_ACTIVE } else { BORDER }, R_FRAME));
            centered_text(
                atlas,
                kind.code().to_uppercase().as_str(),
                r[0] + r[2] / 2.0,
                r[1] + (r[3] - atlas.size_px as f32) / 2.0,
                if hovered { BG_PRIMARY } else { TEXT_PRIMARY },
                &mut v,
            );
            rg.push(Region { id: RegionId::Palette(i), r });
        }
        // Drag ghost at cursor.
        if st.palette_drag.is_some() {
            let (mx, my) = st.mouse;
            v.push(Instance::rect([mx - wr, my - wr], [wr * 2.0, wr * 2.0], ACCENT, wr));
            v.push(Instance::rect([mx - wr + 3.0, my - wr + 3.0], [wr * 2.0 - 6.0, wr * 2.0 - 6.0], BG_PRIMARY, wr - 3.0));
        }
    }

    // ---- Quick Settings panel ----------------------------------------------
    if st.settings.open {
        let pw = 240.0f32;
        let ph = 208.0f32;
        let px = dock_x - pw - DOCK_MARGIN;
        let py = dock_y;
        v.push(Instance::rect([px, py], [pw, ph], BG_SURFACE, R_CHILD));
        let mut cy = py + 10.0;
        centered_text(atlas, tr(lang, "ui.quick_settings"), px + pw / 2.0, cy, TEXT_PRIMARY, &mut v);
        cy += 30.0;

        // CAS sharpness slider.
        let label = format!("CAS {:.0}%", st.sharpness * 100.0);
        layout_text(atlas, &label, px + 12.0, cy, TEXT_MUTED, &mut v);
        cy += 20.0;
        let track = [px + 12.0, cy, pw - 24.0, 10.0];
        v.push(Instance::rect([track[0], track[1]], [track[2], track[3]], BORDER, 5.0));
        v.push(Instance::rect(
            [track[0], track[1]],
            [track[2] * st.sharpness, track[3]],
            ACCENT,
            5.0,
        ));
        rg.push(Region { id: RegionId::CasSlider, r: track });
        cy += 22.0;

        // Resolution presets.
        layout_text(atlas, tr(lang, "ui.resolution"), px + 12.0, cy, TEXT_MUTED, &mut v);
        cy += 20.0;
        for (i, &(_rw, rh)) in RES_PRESETS.iter().enumerate() {
            let bw = (pw - 24.0 - 2.0 * gap_px()) / 3.0;
            let r = [px + 12.0 + i as f32 * (bw + gap_px()), cy, bw, 24.0];
            let active = st.res_idx == i;
            let hovered = st.hover == Some(RegionId::ResPreset(i));
            v.push(Instance::rect(
                [r[0], r[1]],
                [r[2], r[3]],
                if active { ACCENT } else if hovered { ACCENT_ACTIVE } else { BORDER },
                R_GRAB,
            ));
            centered_text(
                atlas,
                &format!("{}p", rh),
                r[0] + r[2] / 2.0,
                r[1] + (r[3] - atlas.size_px as f32) / 2.0,
                if active { BG_PRIMARY } else { TEXT_PRIMARY },
                &mut v,
            );
            rg.push(Region { id: RegionId::ResPreset(i), r });
        }
        cy += 32.0;

        // FPS caps.
        layout_text(atlas, tr(lang, "ui.fps_cap"), px + 12.0, cy, TEXT_MUTED, &mut v);
        cy += 20.0;
        for (i, &cap) in FPS_PRESETS.iter().enumerate() {
            let bw = (pw - 24.0 - 2.0 * gap_px()) / 3.0;
            let r = [px + 12.0 + i as f32 * (bw + gap_px()), cy, bw, 24.0];
            let active = st.fps_idx == i;
            let hovered = st.hover == Some(RegionId::FpsPreset(i));
            v.push(Instance::rect(
                [r[0], r[1]],
                [r[2], r[3]],
                if active { ACCENT } else if hovered { ACCENT_ACTIVE } else { BORDER },
                R_GRAB,
            ));
            centered_text(
                atlas,
                &format!("{}", cap),
                r[0] + r[2] / 2.0,
                r[1] + (r[3] - atlas.size_px as f32) / 2.0,
                if active { BG_PRIMARY } else { TEXT_PRIMARY },
                &mut v,
            );
            rg.push(Region { id: RegionId::FpsPreset(i), r });
        }
    }

    // ---- WHPX env-doctor banner ---------------------------------------------
    match &st.banner {
        WhpxBanner::Hidden => {}
        WhpxBanner::Missing(_) | WhpxBanner::RestartReady => {
            let bw = 400.0f32;
            let bh = 150.0f32;
            let bx = (sw - bw) / 2.0;
            let by = sh * 0.18;
            v.push(Instance::rect([bx, by], [bw, bh], BG_SURFACE, R_WINDOW));
            let (msg, btn_text, btn_id) = match &st.banner {
                WhpxBanner::Missing(d) => (
                    format!("{}: {}", tr(lang, "ui.whpx.missing"), d),
                    tr(lang, "ui.whpx.enable"),
                    RegionId::WhpxEnable,
                ),
                WhpxBanner::RestartReady => (
                    tr(lang, "ui.whpx.done").to_string(),
                    tr(lang, "ui.whpx.restart"),
                    RegionId::WhpxRestart,
                ),
                WhpxBanner::Hidden => unreachable!(),
            };
            centered_text(atlas, &msg, bx + bw / 2.0, by + 18.0, TEXT_PRIMARY, &mut v);
            let br = [bx + (bw - 240.0) / 2.0, by + bh - 56.0, 240.0, 36.0];
            let hovered = st.hover == Some(btn_id);
            v.push(Instance::rect(
                [br[0], br[1]],
                [br[2], br[3]],
                if hovered { ACCENT_HOVER } else { ACCENT },
                R_FRAME,
            ));
            centered_text(
                atlas,
                btn_text,
                br[0] + br[2] / 2.0,
                br[1] + (br[3] - atlas.size_px as f32) / 2.0,
                BG_PRIMARY,
                &mut v,
            );
            rg.push(Region { id: btn_id, r: br });
        }
    }

    // ---- Toasts (bottom center, above palette) -------------------------------
    let mut ty = sh - 24.0 - 40.0;
    if st.keymap.open {
        ty -= 56.0;
    }
    for t in st.toasts.items.iter().rev() {
        let tw2 = text_width(atlas, &t.title) + 32.0;
        let tx = (sw - tw2) / 2.0;
        let th = 36.0f32;
        ty -= th + 8.0;
        v.push(Instance::rect([tx, ty], [tw2, th], BG_SURFACE, R_FRAME));
        layout_text(atlas, &t.title, tx + 12.0, ty + (th - atlas.size_px as f32) / 2.0 - 4.0, TEXT_PRIMARY, &mut v);
        // Golden progress bar at the card's bottom edge.
        v.push(Instance::rect(
            [tx + 4.0, ty + th - 6.0],
            [(tw2 - 8.0) * t.progress, 3.0],
            ACCENT,
            1.5,
        ));
    }

    Frame { instances: v, regions: rg }
}

const fn gap_px() -> f32 {
    ITEM_SPACING
}
