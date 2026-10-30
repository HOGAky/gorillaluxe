//! Google Android SDK repository manifest parser.
//!
//! Parses `repository2-1.xml` (emulator + platform-tools packages) and
//! `sys-img2-1.xml` (Android system images) served from
//! `https://dl.google.com/android/repository/` and resolves concrete
//! downloadable `.zip` URLs — no XML library needed, the schema is
//! simple enough for targeted token scanning.

pub const REPO_BASE: &str = "https://dl.google.com/android/repository/";
pub const REPO_MANIFEST: &str = "repository2-1.xml";
pub const SYSIMG_MANIFEST: &str = "sys-img2-1.xml";

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
            let host_os = attr(tag, "host-os");
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
            });
        }
    }
    out
}

/// Pick the Windows x64 headless emulator package.
pub fn choose_emulator(arts: &[Artifact]) -> Option<Artifact> {
    arts.iter()
        .filter(|a| {
            a.package_path == "emulator"
                && a.url.contains("windows")
                && a.url.ends_with(".zip")
        })
        .max_by_key(|a| a.size)
        .cloned()
}

/// Pick the best Android x86_64 system image with ARM64 translation:
/// prefer the highest API level offering an x86_64 flavor.
pub fn choose_system_image(arts: &[Artifact]) -> Option<Artifact> {
    arts.iter()
        .filter(|a| {
            a.package_path.contains("system-images;android-")
                && a.package_path.contains(";x86_64")
                && a.url.ends_with(".zip")
        })
        .max_by_key(|a| a.api_level)
        .cloned()
}

/// Pick the Windows platform-tools package (adb.exe).
pub fn choose_platform_tools(arts: &[Artifact]) -> Option<Artifact> {
    arts.iter()
        .filter(|a| {
            a.package_path == "platform-tools"
                && a.url.contains("windows")
                && a.url.ends_with(".zip")
        })
        .cloned()
        .into_iter()
        .next()
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

    #[test]
    fn prefers_highest_api() {
        let a = parse_manifest(IMG);
        let img = choose_system_image(&a).unwrap();
        assert_eq!(img.api_level, 34);
        assert!(img.package_path.contains("x86_64"));
    }
}
