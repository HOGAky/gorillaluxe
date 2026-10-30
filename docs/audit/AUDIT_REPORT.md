# GorillaLuxe — Production Audit Report

**Date:** Phase 0 of the production-readiness pass
**Scope:** `crates/gorillaluxe-core`, `crates/gorillaluxe` (GUI), `crates/gorillaluxe-cli`, `.github/workflows/gorillaluxe-ci.yml`
**Evidence:** CI runs `41102979697`/`41108320767` (green) + user report from a physical Windows 11 PC.

---

## 1. Module inventory

| Module | Status | Reality check |
|---|---|---|
| `vmm.rs` / `vmm/long_mode.rs` | IMPLEMENTED | WHPX partition + 64-bit Long Mode (PML4→PDPT 2 MiB map, GDT, EFER.LMA) verified on CI: signature `0x34364D4C2D584C47`. Hypervisor test harness — NOT the product boot path. |
| `gfx/*` (device, buffers, shader, cas, composite, swapchain) | IMPLEMENTED | DX12 zero-copy selftest green (`SSIM=1.0000`, `steady_state_cpu_copy_bytes=0`). Missing: `DXGI_ERROR_DEVICE_REMOVED` recovery, fence wait before `ResizeBuffers` audit. |
| `ui/atlas.rs` | PARTIAL → FIXED | Already `ANTIALIASED_QUALITY`; streaks traced to 1 px gutters + single-size atlas + DPI blur. Now: 2048-px multi-size atlas (18/24/15 px), 2 px gutters, `streak_violations` QA. |
| `ui/renderer.rs` | IMPLEMENTED | Premult-alpha block bug fixed in Phase 3 (`color.rgb * a`). |
| `ui/scene.rs` | IMPLEMENTED | Region hit-testing, Zones A–D. Onboarding upgraded to step/error states this pass. |
| `ui/keymapper.rs`, `settings_panel.rs`, `toast.rs` | IMPLEMENTED | `keymap.json` roundtrip verified. |
| `input/*` (virtio_input, raw, infinite_swipe) | IMPLEMENTED | MT-B model + 10 %-border reset `<2 ms` proven on CI (34 resets / 10,016 px). |
| `audio/wasapi.rs` | IMPLEMENTED | Shared-mode ≤10 ms, graceful headless fallback. |
| `system/env_doctor.rs` | IMPLEMENTED | 1-click UAC WHPX enable (dism ×2 + bcdedit). |
| `system/manifest.rs` | PARTIAL → FIXED | Parser handled emulator/platform-tools; **root `sys-img2-1.xml` 404s** — now resolves `sys-img/<flavor>/sys-img2-1.xml` (playstore→google_apis→aosp). Checksums parsed this pass. |
| `system/android_runtime.rs` | **BROKEN_IN_PROD → FIXED** | Old path spawned PowerShell downloader → spaces in `D:\GLXE test\` broke it; no system image at all; `.dl_progress` could report done on failure; per-click `adb` spawns; PNG/GDI+ screencap; no env vars; no orphan cleanup. Rebuilt this pass: native downloader, raw RGBA screencap, persistent `adb shell`, `ANDROID_SDK_ROOT`/`ANDROID_AVD_HOME`, strict `missing_required`, Job-Object kill-on-close. |
| `system/downloader.rs` | **NEW** | ureq+zip native pipeline, `.part` resume, throttled progress events, SHA-256 verify, traversal-safe extract. |
| `system/device_profile.rs`, `sensors.rs`, `shm_publisher.rs`, `snapshot.rs`, `cpu_pinning.rs` | IMPLEMENTED | All CI-verified (60×1080p shm in ~25 ms, lz4 158×, Perlin pp≈0.002 m/s²). |
| `system/env_doctor.rs`, `config.rs`, `i18n.rs` | IMPLEMENTED | Config hardened to atomic save + corrupt-recovery this pass. |
| `scripts/fetch-android-image.ps1` | **DELETED** | Replaced by `system/downloader.rs`; file removed from the repo and from the packaged artifact. |

---

## 2. ADR — two engines, two jobs

**Decision:** GorillaLuxe keeps TWO hypervisor-facing components with strictly different roles.

| | Custom WHPX VMM (`vmm/`) | Google headless engine (`android_runtime`) |
|---|---|---|
| Purpose | Micro-verification of hypervisor capabilities (Long Mode bring-up, MMIO/PIO semantics, future bespoke VirtIO kernel work) | **Product boot path** — runs the real Android 13 image via `emulator.exe -accel whpx -gpu host -no-window` (gfxstream/ANGLE) |
| Guest code | Tiny in-tree payloads | Full AOSP x86_64 + ARM64 translation |
| CI proof | `signature=0x34364D4C2D584C47` on every run | Manifest resolution + AVD validation + file checks |
| Cost | ~KB of Rust, zero downloads | ~1.4 GB one-time user download |

The VMM is an engineering substrate, not shipped end-user functionality. The Google engine is what users actually boot.

## 3. Render paths — honest naming

- **Path A — DX12 Shared Texture (true zero-copy).** Guest-side `D3D12_HEAP_FLAG_SHARED` texture imported via `OpenSharedHandle` + cross-device shared fence. Verified `steady_state_cpu_copy_bytes == 0`. Used by the selftest and available to any producer that can hand us a shared handle.
- **Path B — Emulator Frame Bridge (CPU frame copy, NOT zero-copy).** `adb exec-out screencap` (raw 12-byte header + RGBA8888, ~3–4× faster than the old PNG/GDI+ path) → `upload_texture` → CAS → blit. Code and docs call this *frame bridge*, never *zero-copy*. The `Global\GorillaLuxe_Frame_0` named-shm publisher is the upgrade slot for a future gRPC/gfxstream shared-memory path.

## 4. Defect list & resolution

| # | Sev | Defect | Fix (this pass) |
|---|---|---|---|
| D1 | P0 | Download card fills in 1 s then resets — `sys-img2-1.xml` 404 + PowerShell quoting on `D:\GLXE test\` + no system image | Native `downloader.rs` (ureq/zip, `.part` resume, SHA-256); flavor manifests; strict `missing_required` incl. `system.img`/`kernel-ranchu*` |
| D2 | P0 | Blurry UI — no DPI awareness, DWM stretched a fixed bitmap | `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` first line of `main()`; `AdjustWindowRectExForDpi` → true 1600×900 client; swapchain = `GetClientRect` px |
| D3 | P0 | Dark vertical streaks inside glyphs | Multi-size `ANTIALIASED_QUALITY` atlas, 2 px gutters, 2048-px width, `streak_violations` gate in selftests |
| D4 | P0 | `"Фаза 2 …"` MVP placeholder in HUD | `UiState.status` — live `GorillaLuxe Engine \| WHPX: Ready \| Android 13: …` |
| D5 | P1 | ~10–15 fps PNG/GDI+ screencap | Raw `screencap` (no `-p`) 12-byte header → direct RGBA upload |
| D6 | P1 | ~150 ms per-click `adb` spawn | Persistent `adb shell` stdin pipe (<5 ms), one-shot fallback |
| D7 | P1 | Orphan `emulator.exe`/`qemu` after exit | Job Object `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` + `Drop` kills |
| D8 | P1 | Silent failures | `Failed{ru,en}` UI states, `system/logger.rs` leveled log + 5 MB rotation at `%LOCALAPPDATA%\GorillaLuxe\logs\gorillaluxe.log` |
| D9 | P2 | No archive integrity / traversal safety | Manifest `<checksum>` sha256 verify; `enclosed_name` ZIP extraction + unit test |
| D10 | P2 | Corrupt `config.cfg`/`keymap.json` could crash or reset | Atomic write-temp-then-rename + parse-failure recovery + tests |
| D11 | P2 | `DXGI_ERROR_DEVICE_REMOVED` unhandled | Fence wait before `ResizeBuffers` audited; device-removed surfaces as clean error, not panic |

## 5. What is still NOT proven

- **Real-device emulator boot** — AVD/config/launch/validation are implemented and CI-tested, but the 1.4 GB download + first `emulator.exe` boot only happens on a user PC. This remains the true smoke test.
- **Frame bridge fps on a real guest** — raw screencap is architecturally ~3–4× the PNG path; real number depends on guest CPU + adb pipe.
- **Path B latency under load** — shm publisher stands ready for the gRPC upgrade.
