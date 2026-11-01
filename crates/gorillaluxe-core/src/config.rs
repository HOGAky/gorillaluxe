//! Persistent config stored at %LOCALAPPDATA%\GorillaLuxe\config.cfg
//! Flat `key=value` lines, no external deps (disk budget is strict).

use crate::i18n::Lang;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub lang: Lang,
    /// `--ci-software-gpu`: allow DXGI_ADAPTER_FLAG_SOFTWARE (WARP) fallback.
    pub ci_software_gpu: bool,
    /// FidelityFX CAS sharpness 0.0..=1.0 (Phase 2+).
    pub cas_sharpness: f32,
    /// Compositor resolution preset (Phase 3 settings panel).
    pub res_w: u32,
    pub res_h: u32,
    /// Frame cap (60/120/240).
    pub fps_cap: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            lang: Lang::En,
            ci_software_gpu: false,
            cas_sharpness: 0.5,
            res_w: 1920,
            res_h: 1080,
            fps_cap: 120,
        }
    }
}

/// Adaptive resource profile — weak laptops (≤4 GiB) get a small
/// guest so the host never swaps; capable hosts get the gaming tablet.
pub struct SpecProfile {
    /// `emulator.exe -memory` / `hw.ramSize` (MiB).
    pub guest_mem_mb: u32,
    /// `emulator.exe -cores` / `hw.cpu.ncore`.
    pub guest_cores: u32,
    /// AVD `hw.lcd.*` (landscape).
    pub lcd_w: u32,
    pub lcd_h: u32,
    pub lcd_dpi: u32,
    /// Default frame cap when the user hasn't picked one.
    pub fps_cap: u32,
}

/// `true` for hosts where the full guest profile would cause OOM/swap.
pub fn is_low_spec(host_ram_mb: u64) -> bool {
    host_ram_mb > 0 && host_ram_mb <= 4096
}

/// Pick the profile from host RAM (0 = probe failed → capable path).
pub fn spec_profile(host_ram_mb: u64) -> SpecProfile {
    if is_low_spec(host_ram_mb) {
        SpecProfile {
            guest_mem_mb: 1024,
            guest_cores: 2,
            lcd_w: 960,
            lcd_h: 540,
            lcd_dpi: 160,
            fps_cap: 30,
        }
    } else {
        SpecProfile {
            guest_mem_mb: 2048,
            guest_cores: 4,
            lcd_w: 1280,
            lcd_h: 720,
            lcd_dpi: 240,
            fps_cap: 60,
        }
    }
}

pub fn data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(p).join("GorillaLuxe");
    }
    if let Ok(p) = std::env::var("USERPROFILE") {
        return PathBuf::from(p)
            .join("AppData")
            .join("Local")
            .join("GorillaLuxe");
    }
    PathBuf::from(".").join("GorillaLuxe")
}

/// Host UI language autodetect (spec §3.5): `GetUserDefaultUILanguage`
/// primary ID `LANG_RUSSIAN` -> RU, otherwise EN. Only used when the
/// user has no saved preference.
pub fn detect_os_lang() -> Lang {
    const LANG_RUSSIAN: u16 = 0x19;
    unsafe {
        let id = windows::Win32::Globalization::GetUserDefaultUILanguage();
        if (id & 0x3FF) == LANG_RUSSIAN {
            Lang::Ru
        } else {
            Lang::En
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        data_dir().join("config.cfg")
    }

    pub fn load() -> Config {
        let path = Self::path();
        let saved = std::fs::read_to_string(&path).unwrap_or_default();
        let mut cfg = Self::load_from(&path);
        // Weak-PC adaptive defaults — applied only when the user has
        // never explicitly chosen a value in config.cfg.
        let ram = crate::system::env_doctor::host_ram_mb();
        if is_low_spec(ram) {
            let spec = spec_profile(ram);
            let has = |k: &str| {
                saved
                    .lines()
                    .any(|l| l.trim_start().starts_with(&format!("{k}=")))
            };
            if !has("fps_cap") {
                cfg.fps_cap = spec.fps_cap;
            }
            if !has("res") {
                cfg.res_w = spec.lcd_w;
                cfg.res_h = spec.lcd_h;
            }
        }
        cfg
    }

    /// Load from an explicit path. Malformed/garbage lines are skipped
    /// line-by-line so a truncated write can never crash startup —
    /// worst case the user gets defaults (corrupt-file recovery).
    pub fn load_from(path: &std::path::Path) -> Config {
        // Autodetect first; the saved file overrides below.
        let mut cfg = Config {
            lang: detect_os_lang(),
            ..Default::default()
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return cfg;
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match (k.trim(), v.trim()) {
                ("lang", v) => {
                    if let Some(l) = Lang::from_code(v) {
                        cfg.lang = l;
                    }
                }
                ("ci_software_gpu", v) => {
                    cfg.ci_software_gpu = v == "1" || v.eq_ignore_ascii_case("true")
                }
                ("cas_sharpness", v) => {
                    if let Ok(f) = v.parse::<f32>() {
                        cfg.cas_sharpness = f.clamp(0.0, 1.0);
                    }
                }
                ("res", v) => {
                    if let Some((w, h)) = v.split_once('x') {
                        if let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) {
                            if w >= 320 && h >= 240 && w <= 8192 && h <= 8192 {
                                cfg.res_w = w;
                                cfg.res_h = h;
                            }
                        }
                    }
                }
                ("fps_cap", v) => {
                    if let Ok(f) = v.parse::<u32>() {
                        cfg.fps_cap = f.clamp(30, 1000);
                    }
                }
                _ => {}
            }
        }
        cfg
    }

    pub fn save(&self) -> std::io::Result<()> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir)?;
        let text = format!(
            "lang={}\nci_software_gpu={}\ncas_sharpness={}\nres={}x{}\nfps_cap={}\n",
            self.lang.code(),
            self.ci_software_gpu as u8,
            self.cas_sharpness,
            self.res_w,
            self.res_h,
            self.fps_cap
        );
        // Atomic-ish save: write a temp file, then rename over the
        // target — a crash mid-write can never leave a truncated
        // config.cfg. MoveFileEx-without-replace semantics means we
        // remove the destination first on retry (worst case one
        // lost save, never a corrupt one).
        let tmp = Self::path().with_extension("cfg.tmp");
        std::fs::write(&tmp, text)?;
        if std::fs::rename(&tmp, Self::path()).is_err() {
            let _ = std::fs::remove_file(Self::path());
            std::fs::rename(&tmp, Self::path())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let c = Config::default();
        assert_eq!(c.lang, Lang::En);
        assert!((c.cas_sharpness - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn corrupt_file_recovers_to_defaults() {
        let p = std::env::temp_dir().join("gorillaluxe-badcfg.cfg");
        std::fs::write(&p, b"\x00\x01\x02garbage\xFF\xFE===\nlang=zz\nres=-1x0\n").unwrap();
        let c = Config::load_from(&p);
        // Bad values rejected → defaults (lang falls back to OS detect).
        assert!((320..=8192).contains(&c.res_w) && c.res_w == 1920);
        assert!(c.fps_cap >= 30);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn roundtrip_load_from() {
        let p = std::env::temp_dir().join("gorillaluxe-goodcfg.cfg");
        std::fs::write(
            &p,
            "lang=en\nfps_cap=240\ncas_sharpness=0.9\nres=2560x1440\n",
        )
        .unwrap();
        let loaded = Config::load_from(&p);
        assert_eq!(loaded.fps_cap, 240);
        assert!((loaded.cas_sharpness - 0.9).abs() < 1e-4);
        assert_eq!((loaded.res_w, loaded.res_h), (2560, 1440));
        let _ = std::fs::remove_file(&p);
    }
}
