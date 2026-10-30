//! gorillaluxe-cli.exe — headless entry point.
//!
//! Phase 1 usage:
//!   gorillaluxe-cli --self-test [--lang en|ru] [--allow-no-whpx]
//!
//! Exit codes: 0 = PASS (or SKIP with --allow-no-whpx), 1 = FAIL, 2 = SKIP.

use gorillaluxe_core::config::Config;
use gorillaluxe_core::i18n::{tr, Lang};
use gorillaluxe_core::vmm::{run_smoke_test, SelfTest};
use gorillaluxe_core::{APP_NAME, APP_VERSION};

fn main() {
    std::process::exit(real_main());
}

fn real_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut cfg = Config::load();
    let mut self_test = false;
    let mut allow_no_whpx = false;
    let mut show_help = false;
    let mut show_version = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--self-test" | "selftest" => self_test = true,
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
    if show_help || !self_test {
        println!("{}", tr(cfg.lang, "cli.help"));
        return 0;
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
