//! Cyrillic + Latin font atlas rasterized via GDI (`GetGlyphOutlineW`
//! GGO_GRAY8_BITMAP on Segoe UI — spec §3.5). Output is an R8 coverage
//! texture consumed by the DX12 UI renderer.

use std::collections::HashMap;
use std::ffi::c_void;

use windows::Win32::Graphics::Gdi::*;

use super::theme::{FONT_HEADER, FONT_UI};

/// Glyph layout info (positions in pixels, uv in atlas units).
#[derive(Clone, Debug, Default)]
pub struct Glyph {
    pub uv: [f32; 4],
    pub advance: f32,
    pub origin: [f32; 2],
    pub size: [f32; 2],
    /// Sum of coverage samples (>0 => non-empty render — Phase 2 check).
    pub coverage: u64,
}

pub struct FontAtlas {
    pub size_px: i32,
    pub width: u32,
    pub height: u32,
    pub ascent: i32,
    /// R8 coverage data, row-major.
    pub pixels: Vec<u8>,
    pub glyphs: HashMap<u16, Glyph>,
}

/// Glyph ranges: ASCII printable + Cyrillic U+0400..U+052F (spec §3.5).
pub const GLYPH_RANGES: &[(u16, u16)] = &[(0x0020, 0x007E), (0x0400, 0x052F)];

const FONT_FACE: &str = "Segoe UI\0";

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Rasterize all requested glyphs at `px` pixels. Returns atlas with
/// every glyph's coverage recorded for verification.
pub fn build(px: i32) -> Result<FontAtlas, String> {
    unsafe {
        let screen = GetDC(None);
        let memdc = CreateCompatibleDC(if screen.0.is_null() { None } else { Some(screen) });
        if memdc.0.is_null() {
            return Err("CreateCompatibleDC failed".into());
        }
        let font_name = utf16(FONT_FACE);
        let font = CreateFontW(
            -px,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_TT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            (DEFAULT_PITCH.0 as u32) | (FF_SWISS.0 as u32),
            windows::core::PCWSTR(font_name.as_ptr()),
        );
        if font.0.is_null() {
            let _ = DeleteDC(memdc);
            return Err("CreateFontW(Segoe UI) failed".into());
        }
        let _ = SelectObject(memdc, HGDIOBJ(font.0));

        let mut tm = TEXTMETRICW::default();
        let _ = GetTextMetricsW(memdc, &mut tm);
        let ascent = tm.tmAscent;

        let mat = MAT2 {
            eM11: FIXED { value: 1, fract: 0 },
            eM12: FIXED { value: 0, fract: 0 },
            eM21: FIXED { value: 0, fract: 0 },
            eM22: FIXED { value: 1, fract: 0 },
        };

        // First pass: measure atlas size via simple shelf packing.
        struct Raw {
            ch: u16,
            bmp: Vec<u8>,
            pitch: usize,
            gm: GLYPHMETRICS,
        }
        let mut raws: Vec<Raw> = Vec::new();
        for &(lo, hi) in GLYPH_RANGES {
            for ch in lo..=hi {
                let mut gm = GLYPHMETRICS::default();
                let need = GetGlyphOutlineW(
                    memdc,
                    ch as u32,
                    GGO_GRAY8_BITMAP,
                    &mut gm,
                    0,
                    None,
                    &mat,
                );
                let (bmp, pitch) = if need == 0 || need == u32::MAX {
                    (Vec::new(), 0usize)
                } else {
                    let mut b = vec![0u8; need as usize];
                    let got = GetGlyphOutlineW(
                        memdc,
                        ch as u32,
                        GGO_GRAY8_BITMAP,
                        &mut gm,
                        need,
                        Some(b.as_mut_ptr() as *mut c_void),
                        &mat,
                    );
                    if got == u32::MAX {
                        (Vec::new(), 0usize)
                    } else {
                        b.truncate(got as usize);
                        (b, ((gm.gmBlackBoxX as usize) + 3) & !3)
                    }
                };
                raws.push(Raw { ch, bmp, pitch, gm });
            }
        }

        // Shelf-pack into an atlas (1 px gutters).
        let atlas_w = 512u32;
        let mut atlas_h = 1u32;
        let mut pen_x = 1u32;
        let mut pen_y = 1u32;
        let mut shelf_h = 0u32;
        let mut placements: Vec<(u32, u32)> = Vec::with_capacity(raws.len());
        for r in &raws {
            let gw = r.gm.gmBlackBoxX.max(1);
            let gh = r.gm.gmBlackBoxY.max(1);
            if pen_x + gw + 1 > atlas_w {
                pen_x = 1;
                pen_y += shelf_h + 1;
                shelf_h = 0;
            }
            placements.push((pen_x, pen_y));
            pen_x += gw + 1;
            shelf_h = shelf_h.max(gh);
            atlas_h = atlas_h.max(pen_y + gh + 1);
        }

        let mut pixels = vec![0u8; (atlas_w * atlas_h) as usize];
        let mut glyphs = HashMap::new();
        for (r, &(px_, py_)) in raws.iter().zip(placements.iter()) {
            let gw = r.gm.gmBlackBoxX as usize;
            let gh = r.gm.gmBlackBoxY as usize;
            let mut coverage = 0u64;
            for y in 0..gh {
                for x in 0..gw {
                    let v = if r.bmp.is_empty() {
                        0u8
                    } else {
                        let idx = y * r.pitch + x;
                        let raw = *r.bmp.get(idx).unwrap_or(&0);
                        // GGO_GRAY8_BITMAP yields 0..64 coverage.
                        ((raw.min(64) as u32) * 255 / 64) as u8
                    };
                    pixels[((py_ + y as u32) * atlas_w + px_ + x as u32) as usize] = v;
                    coverage += v as u64;
                }
            }
            glyphs.insert(
                r.ch,
                Glyph {
                    uv: [
                        px_ as f32 / atlas_w as f32,
                        py_ as f32 / atlas_h as f32,
                        gw as f32 / atlas_w as f32,
                        gh as f32 / atlas_h as f32,
                    ],
                    advance: r.gm.gmCellIncX as f32,
                    origin: [r.gm.gmptGlyphOrigin.x as f32, r.gm.gmptGlyphOrigin.y as f32],
                    size: [gw as f32, gh as f32],
                    coverage,
                },
            );
        }

        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = DeleteDC(memdc);
        if !screen.0.is_null() {
            ReleaseDC(None, screen);
        }

        Ok(FontAtlas {
            size_px: px,
            width: atlas_w,
            height: atlas_h,
            ascent,
            pixels,
            glyphs,
        })
    }
}

/// Coverage check for the Cyrillic block (Phase 2 acceptance).
pub fn cyrillic_coverage(atlas: &FontAtlas) -> u64 {
    (0x0410u16..=0x044F)
        .map(|c| atlas.glyphs.get(&c).map(|g| g.coverage).unwrap_or(0))
        .sum()
}

/// Phase-3 visual QA: verify every rasterized glyph cell is a real
/// letter shape, not a solid block — saturated (255) pixels must stay
/// <= 75% of the cell and letter cells must contain >= 5% ink.
/// Returns (cells_checked, offending_codepoints).
pub fn glyph_fill_violations(atlas: &FontAtlas) -> (usize, Vec<u16>) {
    let mut checked = 0usize;
    let mut bad: Vec<u16> = Vec::new();
    for (&ch, g) in &atlas.glyphs {
        let px0 = (g.uv[0] * atlas.width as f32) as u32;
        let py0 = (g.uv[1] * atlas.height as f32) as u32;
        let gw = (g.uv[2] * atlas.width as f32) as u32;
        let gh = (g.uv[3] * atlas.height as f32) as u32;
        let area = (gw * gh) as usize;
        if area < 9 {
            continue; // dots/diacritics too small to judge
        }
        checked += 1;
        let mut sat = 0usize;
        let mut nz = 0usize;
        for y in 0..gh {
            for x in 0..gw {
                let v = atlas.pixels[((py0 + y) * atlas.width + px0 + x) as usize];
                if v == 255 {
                    sat += 1;
                }
                if v > 0 {
                    nz += 1;
                }
            }
        }
        // A correct letter can reach ~100% ink but never ~all 255 —
        // a solid 0xFF cell is the GDI-fallback rectangle symptom.
        let solid = sat * 100 > 75 * area;
        let letter = matches!(ch, 0x30..=0x39 | 0x41..=0x5A | 0x61..=0x7A | 0x0400..=0x052F);
        let empty = letter && nz * 100 < 5 * area;
        if solid || empty {
            bad.push(ch);
        }
    }
    (checked, bad)
}

/// Convenience: UI + header atlases used by the compositor.
pub fn build_ui_atlases() -> Result<(FontAtlas, FontAtlas), String> {
    Ok((build(FONT_UI)?, build(FONT_HEADER)?))
}
