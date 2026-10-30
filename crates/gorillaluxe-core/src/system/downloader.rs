//! Native resumable HTTP downloader + ZIP extractor.
//!
//! Replaces the PowerShell bootstrapper: downloads Google repository
//! packages on a background thread with `.part` resume, throttled
//! progress events (bytes/total/speed), streamed ZIP extraction, and
//! automatic cleanup — all progress surfaced through `DlEvent`s the
//! UI renders directly in the Zone D onboarding card.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// One artifact to fetch + extract.
#[derive(Debug, Clone)]
pub struct ArtifactTarget {
    /// 1-based step index shown in the UI ("step X of N").
    pub step: usize,
    /// Human label, e.g. "Android Platform-Tools (adb.exe)".
    pub label: String,
    pub url: String,
    /// Temporary zip destination (removed after extraction).
    pub zip: PathBuf,
    /// Extraction destination directory.
    pub dest: PathBuf,
    /// Expected SHA-256 hex from the manifest (empty = skip verify).
    pub sha256: String,
    /// Expected SHA-1 hex fallback when no sha256 is published.
    pub sha1: String,
}

/// Events emitted by the download worker thread.
#[derive(Debug)]
pub enum DlEvent {
    /// New step starting.
    Step {
        step: usize,
        of: usize,
        label: String,
    },
    /// Throttled byte progress for the CURRENT file only.
    Progress {
        done: u64,
        total: u64,
        speed_bps: f64,
    },
    /// Checksum verification running (sha256/sha1).
    Verifying { label: String },
    /// ZIP extraction started.
    Extracting { label: String },
    /// Free-form log line (also written to gorillaluxe.log by caller).
    Log(String),
    /// Terminal result.
    Finished(Result<(), String>),
}

/// Spawn the worker thread; events arrive on the returned channel.
pub fn start(targets: Vec<ArtifactTarget>) -> mpsc::Receiver<DlEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let r = run(&tx, &targets);
        let _ = tx.send(DlEvent::Finished(r));
    });
    rx
}

fn emit(tx: &mpsc::Sender<DlEvent>, e: DlEvent) -> Result<(), String> {
    tx.send(e)
        .map_err(|_| "UI dropped download channel".to_string())
}

fn run(tx: &mpsc::Sender<DlEvent>, targets: &[ArtifactTarget]) -> Result<(), String> {
    let of = targets.len();
    for t in targets {
        emit(
            tx,
            DlEvent::Step {
                step: t.step,
                of,
                label: t.label.clone(),
            },
        )?;
        if !t.zip.is_file() {
            download_resume(tx, t)?;
        } else {
            emit(tx, DlEvent::Log(format!("cached {}", t.label)))?;
        }
        // Integrity check against the manifest checksum — never trust
        // an archive that fails it.
        if !t.sha256.is_empty() || !t.sha1.is_empty() {
            emit(
                tx,
                DlEvent::Verifying {
                    label: t.label.clone(),
                },
            )?;
            verify_checksum(&t.zip, &t.sha256, &t.sha1)
                .map_err(|e| format!("checksum {}: {e}", t.label))?;
        }
        emit(
            tx,
            DlEvent::Extracting {
                label: t.label.clone(),
            },
        )?;
        extract_zip(&t.zip, &t.dest).map_err(|e| format!("extract {}: {e}", t.label))?;
        // Low-disk: don't keep the archive around.
        let _ = fs::remove_file(&t.zip);
        emit(tx, DlEvent::Log(format!("installed {}", t.label)))?;
    }
    Ok(())
}

/// HEAD content-length (0 when unreachable).
fn remote_len(url: &str) -> u64 {
    ureq::head(url)
        .timeout(Duration::from_secs(20))
        .call()
        .ok()
        .and_then(|r| r.header("Content-Length").and_then(|s| s.parse().ok()))
        .or_else(|| curl_len(url))
        .unwrap_or(0)
}

/// Spawn `curl.exe` hidden (built into Windows 10/11 — reliable where
/// ureq's TLS/DNS path fails on some machines).
fn curl(args: &[&str]) -> Result<std::process::Output, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("curl.exe")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("spawn curl.exe: {e}"))
}

/// content-length via `curl -sI` (transport fallback).
fn curl_len(url: &str) -> Option<u64> {
    let out = curl(&["-sIL", "--max-time", "20", url]).ok()?;
    if !out.status.success() {
        return None;
    }
    let head = String::from_utf8_lossy(&out.stdout);
    head.lines()
        .filter_map(|l| {
            l.strip_prefix("Content-Length:")
                .or_else(|| l.strip_prefix("content-length:"))
        })
        .next_back()
        .and_then(|v| v.trim().parse().ok())
}

/// GET a small body (manifest XML etc.) — ureq first, `curl.exe -fL`
/// when ureq's transport fails. Errors always carry the URL.
pub fn http_get(url: &str) -> Result<Vec<u8>, String> {
    match ureq::get(url).timeout(Duration::from_secs(30)).call() {
        Ok(resp) => {
            let mut b = Vec::new();
            resp.into_reader()
                .read_to_end(&mut b)
                .map_err(|e| format!("{url}: read: {e}"))?;
            Ok(b)
        }
        Err(e) => {
            let out = curl(&["-fL", "-sS", "--retry", "3", "--max-time", "60", url])
                .map_err(|e2| format!("{url}: ureq: {e}; curl spawn: {e2}"))?;
            if !out.status.success() {
                return Err(format!(
                    "{url}: ureq: {e}; curl exit {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Ok(out.stdout)
        }
    }
}

/// Probe `url` with `Range: bytes=0-15` — returns the server-reported
/// total size when the server answers 200/206. CI gates on this for
/// every resolved package URL.
pub fn probe_url(url: &str) -> Result<u64, String> {
    let resp = ureq::get(url)
        .set("Range", "bytes=0-15")
        .timeout(Duration::from_secs(30))
        .call();
    match resp {
        Ok(r) => {
            // 206 → parse Content-Range "bytes 0-15/TOTAL"; 200 →
            // the whole object was returned, use Content-Length.
            if let Some(cr) = r.header("Content-Range") {
                if let Some(total) = cr.rsplit('/').next().and_then(|s| s.trim().parse().ok()) {
                    return Ok(total);
                }
            }
            r.header("Content-Length")
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| format!("{url}: no Content-Range/Content-Length"))
        }
        Err(e) => {
            // curl fallback: download 16 bytes with -r, read size.
            let out = curl(&[
                "-fL",
                "-sS",
                "-r",
                "0-15",
                "-o",
                "NUL",
                "-w",
                "%{size_download} %{size_header}",
                "--max-time",
                "30",
                url,
            ])
            .map_err(|e2| format!("{url}: ureq: {e}; curl spawn: {e2}"))?;
            if !out.status.success() {
                return Err(format!(
                    "{url}: ureq: {e}; curl exit {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Ok(0) // reachable, total unknown via this path
        }
    }
}

/// Download `t.url` → `t.zip` with `.part` resume and 3 attempts.
fn download_resume(tx: &mpsc::Sender<DlEvent>, t: &ArtifactTarget) -> Result<(), String> {
    let part = t.zip.with_extension("part");
    let total = remote_len(&t.url);
    let mut last_err = String::new();
    for attempt in 1..=3 {
        match download_attempt(tx, t, &part, total) {
            Ok(()) => {
                fs::rename(&part, &t.zip).map_err(|e| format!("rename: {e}"))?;
                return Ok(());
            }
            Err(e) => {
                last_err = e;
                emit(
                    tx,
                    DlEvent::Log(format!("attempt {attempt}/3 failed: {last_err}")),
                )?;
                thread::sleep(Duration::from_millis(800));
            }
        }
    }
    Err(format!("{} download failed: {last_err}", t.label))
}

fn download_attempt(
    tx: &mpsc::Sender<DlEvent>,
    t: &ArtifactTarget,
    part: &Path,
    total: u64,
) -> Result<(), String> {
    let mut have = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if total > 0 && have == total {
        return Ok(()); // fully downloaded previously
    }
    let mut req = ureq::get(&t.url).timeout(Duration::from_secs(60));
    if have > 0 && total > have {
        req = req.set("Range", &format!("bytes={have}-"));
    }
    let resp = match req.call() {
        Ok(r) => r,
        Err(e) => {
            // Transport fallback: curl.exe handles its own resume via
            // `-C -` against the existing .part file.
            emit(
                tx,
                DlEvent::Log(format!(
                    "ureq failed on {} ({e}); falling back to curl.exe",
                    t.url
                )),
            )?;
            let part_s = part.to_string_lossy().to_string();
            let out = curl(&["-fL", "--retry", "3", "-C", "-", "-o", &part_s, &t.url])
                .map_err(|e2| format!("{}: ureq: {e}; curl spawn: {e2}", t.url))?;
            if !out.status.success() {
                return Err(format!(
                    "{}: ureq: {e}; curl exit {}: {}",
                    t.url,
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            let done = fs::metadata(part).map(|m| m.len()).unwrap_or(0);
            emit(
                tx,
                DlEvent::Progress {
                    done,
                    total,
                    speed_bps: 0.0,
                },
            )?;
            if total > 0 && done < total {
                return Err(format!("truncated at {done}/{total}"));
            }
            return Ok(());
        }
    };
    let mut reader = resp.into_reader();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(part)
        .map_err(|e| format!("open {}: {e}", part.display()))?;

    let mut buf = vec![0u8; 256 * 1024];
    let mut last_emit = Instant::now();
    let mut window = Instant::now();
    let mut window_bytes = 0u64;
    let mut speed = 0.0f64;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| format!("write: {e}"))?;
        have += n as u64;
        window_bytes += n as u64;
        let we = window.elapsed().as_secs_f64();
        if we >= 0.5 {
            speed = window_bytes as f64 / we;
            window = Instant::now();
            window_bytes = 0;
        }
        if last_emit.elapsed() > Duration::from_millis(120) {
            last_emit = Instant::now();
            emit(
                tx,
                DlEvent::Progress {
                    done: have,
                    total,
                    speed_bps: speed,
                },
            )?;
        }
        if total > 0 && have >= total {
            break;
        }
    }
    file.flush().map_err(|e| e.to_string())?;
    if total > 0 && have < total {
        return Err(format!("truncated at {have}/{total}"));
    }
    emit(
        tx,
        DlEvent::Progress {
            done: have,
            total,
            speed_bps: speed,
        },
    )
}

/// Hex digest of a file — `algo` is "sha256" or "sha1". Streams in
/// 1 MiB chunks so multi-GB images don't hit RAM.
pub fn digest_hex(path: &Path, algo: &str) -> Result<String, String> {
    use sha2::Digest;
    let mut f = File::open(path).map_err(|e| format!("open: {e}"))?;
    let mut buf = vec![0u8; 1024 * 1024];
    macro_rules! feed {
        ($h:ident, $ctor:expr) => {{
            let mut $h = $ctor;
            loop {
                let n = f.read(&mut buf).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                Digest::update(&mut $h, &buf[..n]);
            }
            format!("{:x}", Digest::finalize($h))
        }};
    }
    Ok(match algo {
        "sha256" => feed!(h, sha2::Sha256::new()),
        "sha1" => feed!(h, sha1::Sha1::new()),
        _ => return Err(format!("unknown digest {algo}")),
    })
}

/// Compare the file's digest against the manifest value.
fn verify_checksum(path: &Path, sha256: &str, sha1: &str) -> Result<(), String> {
    let (algo, want) = if !sha256.is_empty() {
        ("sha256", sha256)
    } else {
        ("sha1", sha1)
    };
    let got = digest_hex(path, algo)?;
    if got.eq_ignore_ascii_case(want) {
        Ok(())
    } else {
        Err(format!("{algo} mismatch: got {got} want {want}"))
    }
}

/// Streamed ZIP extraction with EXPLICIT path-traversal protection —
/// `enclosed_name()` rejects `..`, absolute paths, and drive roots so
/// a hostile archive can never write outside `dest`.
fn extract_zip(zip_path: &Path, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    let f = File::open(zip_path).map_err(|e| format!("open: {e}"))?;
    let mut za = zip::ZipArchive::new(f).map_err(|e| format!("bad zip: {e}"))?;
    for i in 0..za.len() {
        let mut entry = za.by_index(i).map_err(|e| format!("entry {i}: {e}"))?;
        let Some(rel) = entry.enclosed_name() else {
            continue; // traversal entry — skipped by design
        };
        let out = dest.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(p) = out.parent() {
            fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        let mut w = File::create(&out).map_err(|e| format!("create {rel:?}: {e}"))?;
        std::io::copy(&mut entry, &mut w).map_err(|e| format!("write {rel:?}: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal store-only ZIP builder (zip 2.x refuses `..` names at
    /// write time, so a hostile entry must be hand-rolled).
    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        fn crc32(data: &[u8]) -> u32 {
            let mut crc = !0u32;
            for &b in data {
                crc ^= b as u32;
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xEDB88320 & (0u32.wrapping_sub(crc & 1)));
                }
            }
            !crc
        }
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for (name, data) in entries {
            let off = out.len() as u32;
            let crc = crc32(data);
            let (n, d) = (name.len() as u16, data.len() as u32);
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&d.to_le_bytes());
            out.extend_from_slice(&d.to_le_bytes());
            out.extend_from_slice(&n.to_le_bytes());
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
            cd.extend_from_slice(b"PK\x01\x02");
            cd.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            cd.extend_from_slice(&crc.to_le_bytes());
            cd.extend_from_slice(&d.to_le_bytes());
            cd.extend_from_slice(&d.to_le_bytes());
            cd.extend_from_slice(&n.to_le_bytes());
            cd.extend_from_slice(&[0; 8]); // extra+comment+disk+intattr
            cd.extend_from_slice(&[0; 4]); // ext attr
            cd.extend_from_slice(&off.to_le_bytes());
            cd.extend_from_slice(name.as_bytes());
        }
        let (cd_off, cd_len) = (out.len() as u32, cd.len() as u32);
        out.extend_from_slice(&cd);
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_len.to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    #[test]
    fn zip_traversal_is_rejected() {
        let tmp = std::env::temp_dir().join("gorillaluxe-ziptest");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        // Hostile archive: `../evil.txt` traversal + a legit nested entry.
        let blob = make_zip(&[("../evil.txt", b"pwned"), ("dir/ok.txt", b"fine")]);
        let zpath = tmp.join("evil.zip");
        fs::write(&zpath, &blob).unwrap();
        let dest = tmp.join("out");
        extract_zip(&zpath, &dest).unwrap();
        assert!(dest.join(r"dir\ok.txt").is_file() || dest.join("dir/ok.txt").is_file());
        // The traversal entry must NOT have escaped `dest`.
        assert!(!tmp.join("evil.txt").exists());
        assert!(!dest.join("..").join("evil.txt").exists());
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn digest_known_vector() {
        let tmp = std::env::temp_dir().join("gorillaluxe-sha.txt");
        fs::write(&tmp, b"abc").unwrap();
        assert_eq!(
            digest_hex(&tmp, "sha256").unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            digest_hex(&tmp, "sha1").unwrap(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        let _ = fs::remove_file(&tmp);
    }
}
