//! Cyrillic + Latin font atlas rasterized via GDI (`GetGlyphOutlineW`
//! GGO_GRAY8_BITMAP on Segoe UI — spec §3.5). Output is an R8 coverage
//! texture consumed by the DX12 UI renderer.

use std::collections::HashMap;
use std::ffi::c_void;

use windows::Win32::Graphics::Gdi::*;

use super::theme::{FONT_HEADER, FONT_HUD, FONT_UI};

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

/// Extra font size packed into the same atlas texture.
pub struct AtlasSize {
    pub size_px: i32,
    pub ascent: i32,
    pub glyphs: HashMap<u16, Glyph>,
}

pub struct FontAtlas {
    /// Primary (body) size in pixels.
    pub size_px: i32,
    pub width: u32,
    pub height: u32,
    /// Ascent of the primary size.
    pub ascent: i32,
    /// R8 coverage data, row-major — shared by every size.
    pub pixels: Vec<u8>,
    pub glyphs: HashMap<u16, Glyph>,
    /// Additional sizes (header, HUD) packed in the same texture.
    pub extra: Vec<AtlasSize>,
}

impl FontAtlas {
    /// Glyph map for `px` (falls back to the primary size).
    pub fn glyphs_at(&self, px: i32) -> &HashMap<u16, Glyph> {
        self.extra
            .iter()
            .find(|e| e.size_px == px)
            .map(|e| &e.glyphs)
            .unwrap_or(&self.glyphs)
    }

    pub fn ascent_at(&self, px: i32) -> i32 {
        self.extra
            .iter()
            .find(|e| e.size_px == px)
            .map(|e| e.ascent)
            .unwrap_or(self.ascent)
    }
}

/// Glyph ranges: ASCII printable + Cyrillic U+0400..U+052F (spec §3.5).
pub const GLYPH_RANGES: &[(u16, u16)] = &[(0x0020, 0x007E), (0x0400, 0x052F)];

const FONT_FACE: &str = "Segoe UI\0";

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Rasterize all requested glyphs at `px` pixels (single size).
pub fn build(px: i32) -> Result<FontAtlas, String> {
    build_multi(&[px])
}

/// Rasterize `GLYPH_RANGES` at EVERY size in `pxs` (e.g. 18/24/15 px)
/// and pack them all into ONE R8 atlas — `glyphs`/`ascent`/`size_px`
/// describe `pxs[0]`, the rest live in `extra`. ANTIALIASED_QUALITY
/// grayscale (never ClearType — no subpixel RGB streaks), accurate
/// `gmCellIncX` advances, `tmAscent` baselines, 2 px gutters.
pub fn build_multi(pxs: &[i32]) -> Result<FontAtlas, String> {
    struct Raw {
        size_idx: usize,
        ch: u16,
        bmp: Vec<u8>,
        pitch: usize,
        gm: GLYPHMETRICS,
    }
    unsafe {
        let screen = GetDC(None);
        let memdc = CreateCompatibleDC(if screen.0.is_null() {
            None
        } else {
            Some(screen)
        });
        if memdc.0.is_null() {
            return Err("CreateCompatibleDC failed".into());
        }
        let font_name = utf16(FONT_FACE);
        let mat = MAT2 {
            eM11: FIXED { value: 1, fract: 0 },
            eM12: FIXED { value: 0, fract: 0 },
            eM21: FIXED { value: 0, fract: 0 },
            eM22: FIXED { value: 1, fract: 0 },
        };

        let mut raws: Vec<Raw> = Vec::new();
        let mut ascents: Vec<i32> = Vec::with_capacity(pxs.len());
        for (size_idx, &px) in pxs.iter().enumerate() {
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
                if !screen.0.is_null() {
                    ReleaseDC(None, screen);
                }
                return Err("CreateFontW(Segoe UI) failed".into());
            }
            let _ = SelectObject(memdc, HGDIOBJ(font.0));
            let mut tm = TEXTMETRICW::default();
            let _ = GetTextMetricsW(memdc, &mut tm);
            ascents.push(tm.tmAscent);

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
                    raws.push(Raw {
                        size_idx,
                        ch,
                        bmp,
                        pitch,
                        gm,
                    });
                }
            }
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
        let _ = DeleteDC(memdc);
        if !screen.0.is_null() {
            ReleaseDC(None, screen);
        }

        // Shelf-pack every size into one 2048px-wide atlas (2 px
        // gutters so bilinear filtering never bleeds the next glyph).
        const GUTTER: u32 = 2;
        let atlas_w = 2048u32;
        let mut atlas_h = 1u32;
        let mut pen_x = GUTTER;
        let mut pen_y = GUTTER;
        let mut shelf_h = 0u32;
        let mut placements: Vec<(u32, u32)> = Vec::with_capacity(raws.len());
        for r in &raws {
            let gw = r.gm.gmBlackBoxX.max(1);
            let gh = r.gm.gmBlackBoxY.max(1);
            if pen_x + gw + GUTTER > atlas_w {
                pen_x = GUTTER;
                pen_y += shelf_h + GUTTER;
                shelf_h = 0;
            }
            placements.push((pen_x, pen_y));
            pen_x += gw + GUTTER;
            shelf_h = shelf_h.max(gh);
            atlas_h = atlas_h.max(pen_y + gh + GUTTER);
        }
        if atlas_h > 4096 {
            return Err(format!("font atlas overflow: {}px", atlas_h));
        }

        let mut pixels = vec![0u8; (atlas_w * atlas_h) as usize];
        let mut per_size: Vec<HashMap<u16, Glyph>> =
            (0..pxs.len()).map(|_| HashMap::new()).collect();
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
            per_size[r.size_idx].insert(
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

        let mut it = per_size.into_iter();
        let glyphs = it.next().unwrap_or_default();
        let mut asc = ascents.into_iter();
        Ok(FontAtlas {
            size_px: pxs[0],
            width: atlas_w,
            height: atlas_h,
            ascent: asc.next().unwrap_or(pxs[0]),
            pixels,
            glyphs,
            extra: pxs[1..]
                .iter()
                .copied()
                .zip(asc)
                .zip(it)
                .map(|((px, ascent), glyphs)| AtlasSize {
                    size_px: px,
                    ascent,
                    glyphs,
                })
                .collect(),
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
    for (&ch, g) in atlas
        .glyphs
        .iter()
        .chain(atlas.extra.iter().flat_map(|e| e.glyphs.iter()))
    {
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

/// ClearType-streak detector (production QA): with grayscale
/// ANTIALIASED_QUALITY no glyph cell may contain a fully-transparent
/// *interior* column flanked by well-inked columns on BOTH sides —
/// that's the signature of a subpixel channel dropout appearing as a
/// dark vertical streak inside a stem. Returns offending codepoints.
pub fn streak_violations(atlas: &FontAtlas) -> Vec<u16> {
    let mut bad = Vec::new();
    for (&ch, g) in atlas
        .glyphs
        .iter()
        .chain(atlas.extra.iter().flat_map(|e| e.glyphs.iter()))
    {
        let px0 = (g.uv[0] * atlas.width as f32) as u32;
        let py0 = (g.uv[1] * atlas.height as f32) as u32;
        let gw = (g.uv[2] * atlas.width as f32) as usize;
        let gh = (g.uv[3] * atlas.height as f32) as usize;
        if gw < 4 || gh < 9 {
            continue; // too narrow/short to exhibit streaks
        }
        let col_ink = |x: usize| -> f32 {
            (0..gh)
                .map(|y| atlas.pixels[(py0 as usize + y) * atlas.width as usize + px0 as usize + x])
                .map(|v| v as f32 / 255.0)
                .sum::<f32>()
                / gh as f32
        };
        for x in 1..gw - 1 {
            if col_ink(x) == 0.0 && col_ink(x - 1) > 0.4 && col_ink(x + 1) > 0.4 {
                bad.push(ch);
                break;
            }
        }
    }
    bad.sort_unstable();
    bad.dedup();
    bad
}

/// Convenience: one atlas containing every UI font size
/// (body `FONT_UI`, header `FONT_HEADER`, HUD `FONT_HUD`).
pub fn build_ui_atlases() -> Result<FontAtlas, String> {
    build_multi(&[FONT_UI, FONT_HEADER, FONT_HUD])
}
