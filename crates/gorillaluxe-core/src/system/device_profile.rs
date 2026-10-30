//! Device profile manager (spec §3.3).
//!
//! Ships hardware-accurate presets and emits the artifacts the guest
//! consumes: an early-boot `setprop` script, a kernel-cmdline blob,
//! and the `binfmt_misc` registration descriptor that routes AArch64 /
//! ARMv7 binaries through `libndk_translation`.

use crate::i18n::Lang;

/// One Android device identity.
#[derive(Debug, Clone)]
pub struct DeviceProfile {
    pub name: &'static str,
    pub model: &'static str,
    pub brand: &'static str,
    pub device: &'static str,
    pub board: &'static str,
    pub hardware: &'static str,
    pub fingerprint: &'static str,
    /// Logical density in DPI (maps to `ro.sf.lcd_density`).
    pub dpi: u32,
    /// Stable MAC for `ro.boot.wifimacaddr` / `ifconfig` fakes.
    pub mac_address: &'static str,
    /// SoC codename used for sensor/thermal naming.
    pub soc: &'static str,
}

pub const S23_ULTRA: DeviceProfile = DeviceProfile {
    name: "Samsung S23 Ultra",
    model: "SM-S918B",
    brand: "samsung",
    device: "dm3q",
    board: "kalama",
    hardware: "qcom",
    fingerprint: "samsung/dm3qxeea/dm3q:13/TP1A.220624.014/S918BXXS7CWL8:user/release-keys",
    dpi: 600,
    mac_address: "8C:1F:57:10:B1:3C",
    soc: "Snapdragon 8 Gen 2",
};

pub const ROG7: DeviceProfile = DeviceProfile {
    name: "ASUS ROG Phone 7",
    model: "ASUS_AI2205_A",
    brand: "asus",
    device: "AI2205",
    board: "kalama",
    hardware: "qcom",
    fingerprint:
        "asus/WW_AI2205/ASUS_AI2205_A:13/TKQ1.221013.002/33.0820.0810.17:user/release-keys",
    dpi: 480,
    mac_address: "04:92:26:A5:22:0A",
    soc: "Snapdragon 8 Gen 2",
};

pub const PIXEL8: DeviceProfile = DeviceProfile {
    name: "Pixel 8 Pro",
    model: "Pixel 8 Pro",
    brand: "google",
    device: "husky",
    board: "husky",
    hardware: "husky",
    fingerprint: "google/husky/husky:14/UQ1A.240105.002/11206848:user/release-keys",
    dpi: 489,
    mac_address: "F4:F5:E8:12:34:50",
    soc: "Tensor G3",
};

/// All built-in presets, in dock order.
pub const PRESETS: &[DeviceProfile] = &[S23_ULTRA, ROG7, PIXEL8];

/// Normalized MAC (colon-separated hex pairs); falls back to a
/// deterministic derived value if the preset string is malformed.
fn valid_mac(p: &DeviceProfile) -> String {
    let m = p.mac_address;
    if m.len() == 17 && m.chars().all(|c| c.is_ascii_hexdigit() || c == ':') {
        m.to_string()
    } else {
        // Deterministic fallback from the device name.
        let h: u64 = m
            .bytes()
            .fold(0xC0FFEE, |a, b| a.wrapping_mul(33).wrapping_add(b as u64));
        format!(
            "02:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            (h >> 32) & 0xFF,
            (h >> 24) & 0xFF,
            (h >> 16) & 0xFF,
            (h >> 8) & 0xFF,
            h & 0xFF
        )
    }
}

impl DeviceProfile {
    /// `setprop` script consumed by the guest early-boot daemon.
    pub fn props_script(&self) -> String {
        format!(
            "setprop ro.product.model {m}\n\
             setprop ro.product.brand {b}\n\
             setprop ro.product.device {d}\n\
             setprop ro.product.board {bo}\n\
             setprop ro.product.hardware {h}\n\
             setprop ro.build.fingerprint {f}\n\
             setprop ro.sf.lcd_density {dpi}\n\
             setprop ro.boot.wifimacaddr {mac}\n\
             setprop ro.soc.model {soc}\n",
            m = self.model,
            b = self.brand,
            d = self.device,
            bo = self.board,
            h = self.hardware,
            f = self.fingerprint,
            dpi = self.dpi,
            mac = valid_mac(self),
            soc = self.soc,
        )
    }

    /// Kernel-cmdline fragment the VMM appends for the early-boot
    /// profile daemon (`gorillaluxe.profile=<json-ish>`).
    pub fn kernel_cmdline(&self) -> String {
        format!(
            "gorillaluxe.profile=model:{},brand:{},board:{},dpi:{},mac:{}",
            self.model,
            self.brand,
            self.board,
            self.dpi,
            valid_mac(self)
        )
    }
}

/// `binfmt_misc` registration strings for ARM64 + ARMv7 translation
/// through the NDK bridge (spec §3.3.3).
pub fn binfmt_descriptors() -> [&'static str; 2] {
    [
        // AArch64 ELF: magic + machine type 0xB7 at offset 18.
        ":aarch64:M:0:\\x7fELF\\x02\\x01\\x01\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x02\\x00\\xb7\\x00:\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\x00\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xfe\\xff\\xff\\xff:/system/bin/houdini64:",
        // ARMv7 (32-bit, EABI5).
        ":armv7l:M:0:\\x7fELF\\x01\\x01\\x01\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x00\\x02\\x00\\x28\\x00:\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\x00\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xff\\xfe\\xff\\xff\\xff:/system/bin/houdini:",
    ]
}

/// Guest locale sync commands for a UI language (spec §3.5).
/// Returned as `(setprop persist.sys.locale …, setprop persist.sys.language …)`.
pub fn locale_sync_cmds(lang: Lang) -> (String, String) {
    let (loc, l) = match lang {
        Lang::Ru => ("ru-RU", "ru"),
        Lang::En => ("en-US", "en"),
    };
    (
        format!("setprop persist.sys.locale {loc}"),
        format!("setprop persist.sys.language {l}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_have_required_fields() {
        for p in PRESETS {
            assert!(!p.model.is_empty() && !p.brand.is_empty() && !p.board.is_empty());
            assert!(p.dpi >= 400);
            let s = p.props_script();
            assert!(s.contains("ro.product.model") && s.contains("ro.sf.lcd_density"));
        }
        let [a, b] = binfmt_descriptors();
        assert!(a.contains("aarch64") && b.contains("armv7l"));
        let (loc_ru, _) = locale_sync_cmds(Lang::Ru);
        assert!(loc_ru.contains("ru-RU"));
        let (loc_en, _) = locale_sync_cmds(Lang::En);
        assert!(loc_en.contains("en-US"));
    }
}
