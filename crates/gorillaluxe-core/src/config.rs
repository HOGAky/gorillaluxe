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

pub fn data_dir() -> PathBuf {
    if let Ok(p) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(p).join("GorillaLuxe");
    }
    if let Ok(p) = std::env::var("USERPROFILE") {
        return PathBuf::from(p).join("AppData").join("Local").join("GorillaLuxe");
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
        // Autodetect first; the saved file overrides below.
        let mut cfg = Config { lang: detect_os_lang(), ..Default::default() };
        let Ok(text) = std::fs::read_to_string(Self::path()) else {
            return cfg;
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match (k.trim(), v.trim()) {
                ("lang", v) => {
                    if let Some(l) = Lang::from_code(v) {
                        cfg.lang = l;
                    }
                }
                ("ci_software_gpu", v) => cfg.ci_software_gpu = v == "1" || v.eq_ignore_ascii_case("true"),
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
        std::fs::write(Self::path(), text)
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
}
