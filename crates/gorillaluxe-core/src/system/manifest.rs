//! Google Android SDK repository manifest parser.
//!
//! Parses `repository2-1.xml` (emulator + platform-tools packages) and
//! `sys-img2-1.xml` (Android system images) served from
//! `https://dl.google.com/android/repository/` and resolves concrete
//! downloadable `.zip` URLs — no XML library needed, the schema is
//! simple enough for targeted token scanning.

pub const REPO_BASE: &str = "https://dl.google.com/android/repository/";
pub const REPO_MANIFEST: &str = "repository2-1.xml";

/// System-image manifests live under flavor subdirectories (the
/// flat `sys-img2-1.xml` path at the repository root is a 404).
/// Ordered by preference: Play Store > Google APIs > AOSP default.
pub const SYSIMG_MANIFESTS: &[(&str, &str)] = &[
    (
        "google_apis_playstore",
        "sys-img/google_apis_playstore/sys-img2-1.xml",
    ),
    ("google_apis", "sys-img/google_apis/sys-img2-1.xml"),
    ("android", "sys-img/android/sys-img2-1.xml"),
];

/// Android 13 = API 33 — the Phase-4 target.
pub const TARGET_API: u32 = 33;

/// One downloadable archive resolved from a manifest.
#[derive(Debug, Clone)]
pub struct Artifact {
    /// `remotePackage path` attribute, e.g. `emulator` or
    /// `system-images;android-34;google_apis_playstore;x86_64`.
    pub package_path: String,
    /// `host-os` filter on the enclosing `<archive>` (empty = all OS).
    pub host_os: String,
    /// `archives/archive/complete/url` raw value.
    pub url: String,
    /// Absolute download URL (`REPO_BASE` + url).
    pub absolute_url: String,
    /// Declared `<size>` in bytes (0 when absent).
    pub size: u64,
    /// `api-level` attribute when present (system images only).
    pub api_level: u32,
    /// `<checksum type="sha256">` hex (empty when absent).
    pub sha256: String,
    /// `<checksum type="sha1">` hex fallback.
    pub sha1: String,
}

/// `<checksum type="sha256">hex</checksum>` inside one archive block —
/// Google emits sha256 for newer packages, sha1 for legacy.
fn checksum_of(arch: &str, kind: &str) -> Option<String> {
    let open = format!("<checksum type=\"{kind}\">");
    arch.split(&open)
        .nth(1)
        .and_then(|s| s.split("</checksum>").next())
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
}

fn attr(tag: &str, name: &str) -> String {
    let key = format!("{name}=\"");
    tag.find(&key)
        .map(|i| {
            let rest = &tag[i + key.len()..];
            rest.split('\"').next().unwrap_or("").to_string()
        })
        .unwrap_or_default()
}

/// Extract every downloadable artifact from a manifest body.
pub fn parse_manifest(xml: &str) -> Vec<Artifact> {
    let mut out = Vec::new();
    // Split on `<remotePackage` blocks.
    for block in xml.split("<remotePackage").skip(1) {
        let path = block
            .find("path=\"")
            .map(|i| {
                let rest = &block[i + 6..];
                rest.split('\"').next().unwrap_or("").to_string()
            })
            .unwrap_or_default();
        // API level: `api-level` attr if present, else the
        // `<api-level>N</api-level>` element used by sys-img manifests.
        let api: u32 = attr(block.split('<').next().unwrap_or(""), "api-level")
            .parse()
            .unwrap_or_else(|_| {
                block
                    .split("<api-level>")
                    .nth(1)
                    .and_then(|s| s.split("</").next())
                    .and_then(|n| n.trim().parse().ok())
                    .unwrap_or(0)
            });
        // Each `<archive ...>` element carries its own url + size.
        for arch in block.split("<archive").skip(1) {
            let tag_end = arch.find('>').unwrap_or(0);
            let tag = &arch[..tag_end];
            // Real manifests carry <host-os> as a child element; some
            // schemas also emit it as an archive attribute.
            let host_os = {
                let a = attr(tag, "host-os");
                if a.is_empty() {
                    arch.split("<host-os>")
                        .nth(1)
                        .and_then(|s| s.split("</").next())
                        .unwrap_or("")
                        .trim()
                        .to_string()
                } else {
                    a
                }
            };
            let url = arch
                .split("<url>")
                .nth(1)
                .and_then(|r| r.split("</url>").next())
                .unwrap_or("")
                .trim()
                .to_string();
            if url.is_empty() {
                continue;
            }
            let size: u64 = arch
                .split("<size>")
                .nth(1)
                .and_then(|r| r.split("</size>").next())
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            out.push(Artifact {
                package_path: path.clone(),
                host_os,
                absolute_url: format!("{REPO_BASE}{url}"),
                url,
                size,
                api_level: api,
                sha256: checksum_of(arch, "sha256").unwrap_or_default(),
                sha1: checksum_of(arch, "sha1").unwrap_or_default(),
            });
        }
    }
    out
}

/// Windows-targeted archive: `<host-os>windows` (the authoritative
/// filter) with URL-level fallbacks (`-win.zip` for platform-tools,
/// `windows_x64` for the emulator).
fn is_windows(a: &Artifact) -> bool {
    a.host_os == "windows" || a.url.contains("windows") || a.url.ends_with("-win.zip")
}

/// Pick the Windows x64 headless emulator package.
pub fn choose_emulator(arts: &[Artifact]) -> Option<Artifact> {
    arts.iter()
        .filter(|a| a.package_path == "emulator" && is_windows(a) && a.url.ends_with(".zip"))
        .max_by_key(|a| a.size)
        .cloned()
}

/// Pick the best Android x86_64 system image with ARM64 translation
/// inside ONE flavor's manifest: prefer the target API (Android 13),
/// else the highest available.
pub fn choose_system_image(arts: &[Artifact]) -> Option<Artifact> {
    choose_image_api(arts, TARGET_API)
}

/// API-preferring variant — `pref==0` means "highest available".
pub fn choose_image_api(arts: &[Artifact], pref: u32) -> Option<Artifact> {
    let pool: Vec<&Artifact> = arts
        .iter()
        .filter(|a| {
            a.package_path.contains("system-images;android-")
                && a.package_path.contains(";x86_64")
                && a.url.ends_with(".zip")
        })
        .collect();
    if pref > 0 {
        if let Some(a) = pool
            .iter()
            .filter(|a| a.api_level == pref)
            .max_by_key(|a| a.size)
        {
            return Some((*a).clone());
        }
    }
    pool.into_iter().max_by_key(|a| a.api_level).cloned()
}

/// Pick the Windows platform-tools package (adb.exe) — Google names
/// it `platform-tools_r*-win.zip`, not `windows`.
pub fn choose_platform_tools(arts: &[Artifact]) -> Option<Artifact> {
    arts.iter()
        .find(|a| a.package_path == "platform-tools" && is_windows(a) && a.url.ends_with(".zip"))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
<sdk:repository>
 <remotePackage path="emulator">
  <revision><major>36</major><minor>1</minor></revision>
  <archives>
   <archive host-os="windows">
    <complete><size>400000000</size><url>emulator-windows_x64-12345678.zip</url></complete>
   </archive>
  </archives>
 </remotePackage>
 <remotePackage path="platform-tools">
  <archives><archive host-os="windows">
   <complete><size>20000000</size><url>platform-tools_r36-windows.zip</url></complete>
  </archive></archives>
 </remotePackage>
</sdk:repository>"#;

    const IMG: &str = r#"
<sdk:sdk-sys-img>
 <remotePackage path="system-images;android-34;google_apis_playstore;x86_64" api-level="34">
  <archives><archive>
   <complete><size>900000000</size><url>x86_64-34_r12.zip</url></complete>
  </archive></archives>
 </remotePackage>
 <remotePackage path="system-images;android-33;google_apis;x86_64" api-level="33">
  <archives><archive>
   <complete><size>800000000</size><url>x86_64-33_r04.zip</url></complete>
  </archive></archives>
 </remotePackage>
</sdk:sdk-sys-img>"#;

    #[test]
    fn resolves_artifacts() {
        let a = parse_manifest(SAMPLE);
        let emu = choose_emulator(&a).unwrap();
        assert_eq!(
            emu.absolute_url,
            "https://dl.google.com/android/repository/emulator-windows_x64-12345678.zip"
        );
        let pt = choose_platform_tools(&a).unwrap();
        assert!(pt.absolute_url.ends_with(".zip"));
    }

    /// Regression: real Google manifests use <host-os> elements and
    /// `*-win.zip` (not `*windows*`) platform-tools URLs.
    #[test]
    fn real_manifest_shapes() {
        const REAL: &str = r#"
<sdk:repository>
 <remotePackage path="emulator">
  <archives>
   <archive><complete><size>1</size><url>emulator-linux_x64-1.zip</url></complete><host-os>linux</host-os></archive>
   <archive><complete><size>460366285</size><url>emulator-windows_x64-16696583.zip</url></complete><host-os>windows</host-os></archive>
  </archives>
 </remotePackage>
 <remotePackage path="platform-tools">
  <archives>
   <archive><complete><size>2</size><url>platform-tools_r37.0.1-darwin.zip</url></complete><host-os>macosx</host-os></archive>
   <archive><complete><size>3</size><url>platform-tools_r37.0.1-win.zip</url></complete><host-os>windows</host-os></archive>
  </archives>
 </remotePackage>
</sdk:repository>"#;
        let a = parse_manifest(REAL);
        let emu = choose_emulator(&a).unwrap();
        assert_eq!(emu.host_os, "windows");
        assert!(emu.url.contains("windows_x64"));
        let tools = choose_platform_tools(&a).unwrap();
        assert!(tools.url.ends_with("-win.zip"), "{}", tools.url);
    }

    #[test]
    fn prefers_target_api_then_highest() {
        let a = parse_manifest(IMG);
        // TARGET_API (33) present → wins over the newer 34 image.
        let img = choose_system_image(&a).unwrap();
        assert_eq!(img.api_level, TARGET_API);
        assert!(img.package_path.contains("x86_64"));
        // Ask for an absent API → falls back to highest.
        let img2 = choose_image_api(&a, 99).unwrap();
        assert_eq!(img2.api_level, 34);
    }
}
