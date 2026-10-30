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
}

impl Default for Config {
    fn default() -> Self {
        Config { lang: Lang::En, ci_software_gpu: false, cas_sharpness: 0.5 }
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
                _ => {}
            }
        }
        cfg
    }

    pub fn save(&self) -> std::io::Result<()> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir)?;
        let text = format!(
            "lang={}\nci_software_gpu={}\ncas_sharpness={}\n",
            self.lang.code(),
            self.ci_software_gpu as u8,
            self.cas_sharpness
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
