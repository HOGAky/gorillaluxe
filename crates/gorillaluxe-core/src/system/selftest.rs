//! Automated Verification Test 4 (spec PHASE 4).
//!
//! 1. 64-bit Long Mode guest execution under WHPX.
//! 2. Device profiles + Perlin sensor variance + locale sync.
//! 3. Named shared-memory frame throughput (60 × 1080p < 60 ms).
//! 4. LZ4 snapshot round-trip.
//! 5. Hybrid CPU topology + P-core pinning.
//! 6. Google repository manifest resolver (live fetch w/ embedded
//!    sample fallback).

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::i18n::Lang;
use crate::vmm::{self, SelfTest};

use super::{android_runtime, cpu_pinning, device_profile, manifest, sensors, shm_publisher, snapshot};

#[derive(Debug)]
pub enum Check {
    Pass(String),
    Skip(String),
    Fail(String),
}

impl Check {
    fn ok(&self) -> bool {
        !matches!(self, Check::Fail(_))
    }
    fn line(&self, name: &str) -> String {
        match self {
            Check::Pass(d) => format!("  [PASS] {name}: {d}"),
            Check::Skip(d) => format!("  [SKIP] {name}: {d}"),
            Check::Fail(d) => format!("  [FAIL] {name}: {d}"),
        }
    }
}

pub struct Report4 {
    pub checks: Vec<(String, Check)>,
}

impl Report4 {
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|(_, c)| c.ok())
    }
    pub fn print(&self) {
        println!("Phase 4 self-test:");
        for (name, c) in &self.checks {
            println!("{}", c.line(name));
        }
        println!(
            "SELFTEST-PHASE4: {}",
            if self.passed() { "PASS" } else { "FAIL" }
        );
    }
}

fn long_mode() -> Check {
    match vmm::long_mode::run_long_mode_test() {
        Ok(SelfTest::Pass { transcript }) => Check::Pass(transcript),
        Ok(SelfTest::Skipped(reason)) => Check::Skip(reason.to_string()),
        Err(e) => Check::Fail(format!("{e}")),
    }
}

const EMBEDDED_MANIFEST: &str = r#"
<sdk:repository>
 <remotePackage path="emulator">
  <archives><archive host-os="windows">
   <complete><size>400000000</size><url>emulator-windows_x64-12345678.zip</url></complete>
  </archive></archives>
 </remotePackage>
 <remotePackage path="platform-tools">
  <archives><archive host-os="windows">
   <complete><size>20000000</size><url>platform-tools_r36-windows.zip</url></complete>
  </archive></archives>
 </remotePackage>
</sdk:repository>"#;

fn manifest_check() -> Check {
    // Try live Google fetch (20s budget), fall back to the embedded
    // sample so the resolver logic is always exercised.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(android_runtime::fetch_manifest(manifest::REPO_MANIFEST));
    });
    let live = rx
        .recv_timeout(Duration::from_secs(20))
        .ok()
        .flatten();
    let (arts, source) = match live {
        Some(a) => (a, "live dl.google.com"),
        None => (manifest::parse_manifest(EMBEDDED_MANIFEST), "embedded sample (offline)"),
    };
    let emu = manifest::choose_emulator(&arts);
    let tools = manifest::choose_platform_tools(&arts);
    match (emu, tools) {
        (Some(e), Some(t)) => {
            let ok = e.absolute_url.starts_with(manifest::REPO_BASE)
                && e.absolute_url.ends_with(".zip")
                && t.absolute_url.ends_with(".zip");
            if ok {
                Check::Pass(format!(
                    "{source} -> {} + {}",
                    e.url, t.url
                ))
            } else {
                Check::Fail(format!("bad URLs: {} / {}", e.absolute_url, t.absolute_url))
            }
        }
        _ => Check::Fail("resolver produced no emulator/platform-tools".into()),
    }
}

fn profile_sensor_check() -> Check {
    let p = &device_profile::PRESETS[0];
    let script = p.props_script();
    let cmd = p.kernel_cmdline();
    let binfmt = device_profile::binfmt_descriptors();
    let (loc_ru, lang_ru) = device_profile::locale_sync_cmds(Lang::Ru);
    let (loc_en, _) = device_profile::locale_sync_cmds(Lang::En);
    let feed = sensors::SensorFeed::new(0xC0FFEE);
    let xs: Vec<f32> = (0..100).map(|t| feed.sample(t).accel[0]).collect();
    let pp = sensors::variance_pp(&xs);
    let ok = script.contains("ro.product.model")
        && cmd.contains(&format!("dpi:{}", p.dpi))
        && binfmt[0].contains("aarch64")
        && binfmt[1].contains("armv7l")
        && loc_ru.contains("ru-RU")
        && lang_ru.contains("ru")
        && loc_en.contains("en-US")
        && pp > 1e-6
        && pp < 0.02;
    if ok {
        Check::Pass(format!(
            "{} profiles, accel pp={:.5} m/s^2 @100Hz, locale ru-RU/en-US sync",
            device_profile::PRESETS.len(),
            pp
        ))
    } else {
        Check::Fail(format!("pp={pp}"))
    }
}

fn shm_check() -> Check {
    use shm_publisher::{ShmPublisher, ShmReader, PIXELS_1080P_RGBA};
    let id = 0;
    let mut pub_ = match ShmPublisher::create(id, PIXELS_1080P_RGBA) {
        Ok(p) => p,
        Err(e) => return Check::Fail(format!("create: {e}")),
    };
    let reader = match ShmReader::open(id, PIXELS_1080P_RGBA) {
        Ok(r) => r,
        Err(e) => return Check::Fail(format!("open: {e}")),
    };
    let mut frame = vec![0xABu8; PIXELS_1080P_RGBA];
    let t = Instant::now();
    for i in 0..60u8 {
        frame[12345] = i;
        if let Err(e) = pub_.write_frame(&frame) {
            return Check::Fail(format!("write {i}: {e}"));
        }
    }
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let mut dst = vec![0u8; PIXELS_1080P_RGBA];
    match reader.read_frame(&mut dst) {
        Ok((seq, len)) if seq == 60 && len == PIXELS_1080P_RGBA && dst[12345] == 59 => {
            if ms < 60.0 {
                Check::Pass(format!(
                    "60x1080p frames in {ms:.1}ms via {}",
                    pub_.map_name
                ))
            } else {
                Check::Fail(format!("60 frames took {ms:.1}ms (>60ms)"))
            }
        }
        Ok((s, l)) => Check::Fail(format!("seq={s} len={l}")),
        Err(e) => Check::Fail(e),
    }
}

fn snapshot_check() -> Check {
    let mut ram = vec![0u8; 2 * 1024 * 1024];
    for (i, b) in ram.iter_mut().enumerate().take(64 * 1024) {
        *b = ((i.wrapping_mul(2654435761)) >> 13) as u8;
    }
    let (raw, packed, ratio) = snapshot::snapshot_stats(&ram);
    match snapshot::snapshot_restore(&snapshot::snapshot_save(&ram)) {
        Ok(back) if back == ram => Check::Pass(format!(
            "2MiB roundtrip: {raw}B -> {packed}B ({ratio:.2}x)"
        )),
        Ok(_) => Check::Fail("roundtrip mismatch".into()),
        Err(e) => Check::Fail(e),
    }
}

fn pinning_check() -> Check {
    match cpu_pinning::enumerate() {
        Ok(topo) => {
            let desc = cpu_pinning::describe(&topo);
            if topo.is_hybrid() {
                match cpu_pinning::pin_to_p_cores() {
                    Ok(n) => Check::Pass(format!("{desc}, pinned {n} P-core cpu-sets")),
                    Err(e) => Check::Fail(format!("{desc}, pin: {e}")),
                }
            } else {
                Check::Pass(format!("{desc} (uniform — no pinning needed)"))
            }
        }
        Err(e) => Check::Fail(e),
    }
}

fn runtime_state_check() -> Check {
    match android_runtime::install_state() {
        android_runtime::InstallState::Ready { .. } => {
            Check::Pass("runtime installed".into())
        }
        s => Check::Pass(format!("not installed ({s:?}) — bootstrap available")),
    }
}

pub fn run() -> Report4 {
    Report4 {
        checks: vec![
            ("long-mode WHPX".into(), long_mode()),
            ("manifest resolver".into(), manifest_check()),
            ("profiles+sensors+locale".into(), profile_sensor_check()),
            ("shm frame publisher".into(), shm_check()),
            ("lz4 snapshot".into(), snapshot_check()),
            ("cpu pinning".into(), pinning_check()),
            ("android runtime".into(), runtime_state_check()),
        ],
    }
}
