//! gorillaluxe-cli.exe — headless entry point.
//!
//! Usage:
//!   gorillaluxe-cli --self-test       [--lang en|ru] [--allow-no-whpx]   (Phase 1 WHPX)
//!   gorillaluxe-cli --self-test-gfx   [--lang en|ru] [--ci-software-gpu] (Phase 2 DX12/CAS/UI)
//!   gorillaluxe-cli --self-test-input [--lang en|ru]                     (Phase 3 swipe/WASAPI/keymap)
//!   gorillaluxe-cli --self-test-phase4 [--lang en|ru]                    (Phase 4 long-mode/runtime)
//!   gorillaluxe-cli setup-android     resolve + download the Google Android runtime
//!   gorillaluxe-cli --auto-setup      UAC-elevated WHPX feature enablement
//!
//! Exit codes: 0 = PASS (or SKIP with --allow-no-whpx), 1 = FAIL, 2 = SKIP.

use gorillaluxe_core::config::Config;
use gorillaluxe_core::gfx::selftest;
use gorillaluxe_core::i18n::{tr, Lang};
use gorillaluxe_core::input::selftest as input_selftest;
use gorillaluxe_core::system::{android_runtime, env_doctor, selftest as sys_selftest};
use gorillaluxe_core::vmm::{run_smoke_test, SelfTest};
use gorillaluxe_core::{APP_NAME, APP_VERSION};

fn main() {
    std::process::exit(real_main());
}

fn real_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut cfg = Config::load();
    let mut self_test = false;
    let mut gfx_test = false;
    let mut input_test = false;
    let mut phase4_test = false;
    let mut setup_android = false;
    let mut auto_setup = false;
    let mut elevated_setup = false;
    let mut allow_no_whpx = false;
    let mut show_help = false;
    let mut show_version = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--self-test" | "selftest" => self_test = true,
            "--self-test-gfx" | "gfxtest" => gfx_test = true,
            "--self-test-input" | "inputtest" => input_test = true,
            "--self-test-phase4" | "phase4test" => phase4_test = true,
            "setup-android" => setup_android = true,
            "--auto-setup" => auto_setup = true,
            "--elevated-enable-whpx" => elevated_setup = true,
            "--ci-software-gpu" => cfg.ci_software_gpu = true,
            "--allow-no-whpx" => allow_no_whpx = true,
            "--version" | "-V" => show_version = true,
            "--help" | "-h" | "/?" => show_help = true,
            "--lang" => {
                if let Some(v) = args.get(i + 1) {
                    match Lang::from_code(v) {
                        Some(l) => cfg.lang = l,
                        None => {
                            eprintln!("unknown language: {}", v);
                            return 1;
                        }
                    }
                    i += 1;
                } else {
                    eprintln!("--lang requires a value");
                    return 1;
                }
            }
            "--guest" => {
                // Accepted now; micro-guest boot path lands in Phase 2/3.
                if args.get(i + 1).is_some() {
                    i += 1;
                } else {
                    eprintln!("--guest requires a path");
                    return 1;
                }
            }
            "config" => return config_cmd(&args[i + 1..], &mut cfg),
            other => {
                eprintln!("unknown argument: {}", other);
                return 1;
            }
        }
        i += 1;
    }

    if show_version {
        println!("{} {} ({})", APP_NAME, APP_VERSION, cfg.lang.code());
        return 0;
    }
    if elevated_setup {
        return match env_doctor::run_elevated_setup() {
            Ok(_) => {
                println!("{}", tr(cfg.lang, "cli.setup_done"));
                0
            }
            Err(e) => {
                eprintln!("WHPX setup failed: {e}");
                1
            }
        };
    }
    if auto_setup {
        println!("{}", tr(cfg.lang, "cli.auto_setup"));
        if env_doctor::is_elevated() {
            return match env_doctor::run_elevated_setup() {
                Ok(_) => {
                    println!("{}", tr(cfg.lang, "cli.setup_done"));
                    0
                }
                Err(e) => {
                    eprintln!("WHPX setup failed: {e}");
                    1
                }
            };
        }
        return match env_doctor::launch_elevated_setup() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{}: {e}", tr(cfg.lang, "ui.whpx.uac_denied"));
                1
            }
        };
    }
    if show_help || (!self_test && !gfx_test && !input_test && !phase4_test && !setup_android) {
        println!("{}", tr(cfg.lang, "cli.help"));
        return 0;
    }

    if gfx_test {
        return run_gfx_test(cfg.lang, cfg.ci_software_gpu);
    }
    if input_test {
        return run_input_test(cfg.lang);
    }
    if phase4_test {
        return run_phase4_test(cfg.lang);
    }
    if setup_android {
        return run_setup_android(cfg.lang);
    }

    let lang = cfg.lang;
    println!("{}", tr(lang, "selftest.start"));
    match run_smoke_test() {
        Ok(SelfTest::Pass { transcript }) => {
            println!("{}", tr(lang, "selftest.whpx.found"));
            println!("{}", tr(lang, "selftest.partition.ok"));
            println!("{}: {:?}", tr(lang, "selftest.guest.ok"), transcript);
            println!("{}", tr(lang, "selftest.pass"));
            0
        }
        Ok(SelfTest::Skipped(reason)) => {
            eprintln!("{} ({})", tr(lang, "selftest.whpx.missing"), reason);
            println!("{}", tr(lang, "selftest.skip"));
            if allow_no_whpx {
                0
            } else {
                2
            }
        }
        Err(e) => {
            eprintln!("{}: {}", tr(lang, "selftest.guest.bad"), e);
            println!("{}", tr(lang, "selftest.fail"));
            1
        }
    }
}

/// Phase 2 — Automated Verification Test 2 (DX12 zero-copy, CAS,
/// Luxe Dark UI, Cyrillic atlas, RU/EN hot-swap).
fn run_gfx_test(lang: Lang, allow_software: bool) -> i32 {
    println!("{}", tr(lang, "gfx.init"));
    match selftest::run(allow_software) {
        Ok(r) => {
            println!(
                "{}: {} | {}",
                tr(lang, "gfx.adapter"),
                r.host_adapter,
                if r.is_warp { "WARP" } else { "HW" }
            );
            println!(
                "{}: {}",
                tr(lang, "gfx.zerocopy.ok"),
                tr(lang, "gfx.steady.zero")
            );
            println!(
                "{}: SSIM={:.4} non-black={} presented={}",
                tr(lang, "gfx.cas.ok"),
                r.ssim,
                r.nonblack,
                r.presented
            );
            println!(
                "{}: cyr={} glyphs, coverage={}",
                tr(lang, "gfx.atlas.ok"),
                r.cyr_glyphs,
                r.cyr_coverage
            );
            println!(
                "glyph cells={} block_bad={} streak_bad={}",
                r.glyph_cells, r.glyph_fill_bad, r.glyph_streak_bad
            );
            println!("{}: {:.1} us", tr(lang, "gfx.hotswap.ok"), r.hotswap_us);
            if r.passed() {
                println!("{}", tr(lang, "selftest.pass"));
                0
            } else {
                println!("{}", tr(lang, "selftest.fail"));
                1
            }
        }
        Err(e) => {
            eprintln!("gfx self-test error: {}", e);
            println!("{}", tr(lang, "selftest.fail"));
            1
        }
    }
}

/// Phase 3 — Automated Verification Test 3 (Infinite Swipe 10,000px
/// motion, MT-B slot reset < 2 ms, WASAPI init/fallback, keymap.json
/// roundtrip).
fn run_input_test(lang: Lang) -> i32 {
    println!("{}", tr(lang, "input.selftest"));
    let r = input_selftest::run();
    println!(
        "swipe: {:.0}px over {} ticks, resets={}, max tracking-id gap={} tick(s)",
        r.total_dx_px, r.ticks, r.resets, r.max_reset_gap_ticks
    );
    println!(
        "events={}, distinct tracking ids={}, stalled ticks={}",
        r.events, r.distinct_ids, r.stalled_ticks
    );
    println!("keymap.json roundtrip: {}", r.keymap_roundtrip);
    if r.wasapi_ready {
        println!(
            "WASAPI shared mode: ready ({} | period {:.1} ms)",
            r.wasapi_detail, r.wasapi_period_ms
        );
    } else {
        println!("WASAPI shared mode: fallback ({})", r.wasapi_detail);
    }
    if r.passed() {
        println!("{}", tr(lang, "selftest.pass"));
        0
    } else {
        println!("{}", tr(lang, "selftest.fail"));
        1
    }
}

/// Phase 4 — Automated Verification Test 4 (long mode, profiles,
/// sensors, shm frames, snapshots, pinning, manifest resolver).
fn run_phase4_test(lang: Lang) -> i32 {
    let _ = lang;
    let r = sys_selftest::run();
    r.print();
    if r.passed() {
        0
    } else {
        1
    }
}

/// `setup-android`: resolve Google repository URLs and run the
/// resumable downloader synchronously (CLI mode).
fn run_setup_android(lang: Lang) -> i32 {
    println!("{}", tr(lang, "cli.setup_android"));
    if let android_runtime::InstallState::Ready {
        emulator, sysimg, ..
    } = android_runtime::install_state()
    {
        println!(
            "already installed: {} | {}",
            emulator.display(),
            sysimg.display()
        );
        return 0;
    }
    match android_runtime::resolve_runtime_urls() {
        Ok(urls) => {
            for a in &urls {
                println!("  -> {}", a.absolute_url);
            }
            match android_runtime::DownloadJob::start(&urls) {
                Ok(job) => {
                    use gorillaluxe_core::system::downloader::DlEvent;
                    let mut done_all: Option<Result<(), String>> = None;
                    while done_all.is_none() {
                        for ev in job.poll() {
                            match ev {
                                DlEvent::Step { step, of, label } => {
                                    println!("\n[{step}/{of}] {label}");
                                }
                                DlEvent::Progress {
                                    done,
                                    total,
                                    speed_bps,
                                } => {
                                    print!(
                                        "\r  {:.1} / {:.1} MB ({:.1} MB/s)   ",
                                        done as f64 / 1048576.0,
                                        total as f64 / 1048576.0,
                                        speed_bps / 1048576.0
                                    );
                                }
                                DlEvent::Verifying { label } => {
                                    println!("\n  verifying {label}...");
                                }
                                DlEvent::Extracting { label } => {
                                    println!("\n  extracting {label}...");
                                }
                                DlEvent::Log(l) => println!("  {l}"),
                                DlEvent::Finished(r) => done_all = Some(r),
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_millis(120));
                    }
                    println!();
                    match (done_all, android_runtime::install_state()) {
                        (Some(Ok(())), android_runtime::InstallState::Ready { .. }) => {
                            println!("{}", tr(lang, "ui.onboard.ready"));
                            0
                        }
                        (Some(Err(e)), _) => {
                            eprintln!("download failed: {e}");
                            1
                        }
                        (_, st) => {
                            let m = match st {
                                android_runtime::InstallState::Partial(m) => m,
                                _ => "unknown".into(),
                            };
                            eprintln!("download finished but runtime incomplete: {m}");
                            1
                        }
                    }
                }
                Err(e) => {
                    eprintln!("downloader spawn failed: {e}");
                    1
                }
            }
        }
        Err(e) => {
            eprintln!("manifest resolve failed: {e}");
            1
        }
    }
}

fn config_cmd(args: &[String], cfg: &mut Config) -> i32 {
    match args {
        [cmd, key] if cmd == "get" => {
            let v = match key.as_str() {
                "lang" => cfg.lang.code().to_string(),
                "ci_software_gpu" => cfg.ci_software_gpu.to_string(),
                "cas_sharpness" => cfg.cas_sharpness.to_string(),
                _ => return 1,
            };
            println!("{}", v);
            0
        }
        [cmd, key, value] if cmd == "set" => {
            match key.as_str() {
                "lang" => match Lang::from_code(value) {
                    Some(l) => cfg.lang = l,
                    None => {
                        eprintln!("unknown language: {}", value);
                        return 1;
                    }
                },
                "ci_software_gpu" => {
                    cfg.ci_software_gpu = value == "1" || value.eq_ignore_ascii_case("true")
                }
                "cas_sharpness" => {
                    let Ok(f) = value.parse::<f32>() else {
                        eprintln!("invalid float: {}", value);
                        return 1;
                    };
                    cfg.cas_sharpness = f.clamp(0.0, 1.0);
                }
                _ => {
                    eprintln!("unknown key: {}", key);
                    return 1;
                }
            }
            match cfg.save() {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("config save failed: {}", e);
                    1
                }
            }
        }
        _ => {
            eprintln!("usage: config get <key> | config set <key> <value>");
            1
        }
    }
}
