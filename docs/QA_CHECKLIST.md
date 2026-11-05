# GorillaLuxe QA Checklist / Чек-лист QA

Run before every release tag. Mark each item PASS/FAIL with notes.
Прогонять перед каждым релизным тегом.

## 1. Hypervisor / Гипервизор

- [ ] Clean boot on WHPX-enabled host → `gorillaluxe.log` shows
      `WindowsHypervisorPlatform=on`, `CPU Acceleration: working`,
      QEMU gets `-enable-whpx`.
- [ ] WHPX-disabled/VirtualBox-conflict host → launch fails with a
      CLEAR fatal error (no silent TCG fallback); `perf_debug.log`
      records `host.whpx_ok=false`.
- [ ] One-click WHPX setup (F5) → UAC elevation → dism succeeds →
      reboot marker written.

## 2. GPU paths / Пути GPU

- [ ] Hardware GPU host: log shows `gpu_mode_requested: host` and
      guest GLES reports a real GPU (NOT SwiftShader/llvmpipe);
      `guest.gles_software=false` in perf_debug.log.
- [ ] `--ci-software-gpu` (or WARP-only host): D3D12 falls back to
      WARP for the UI compositor; guest uses `swangle`; still boots.
- [ ] Hybrid laptop: D3D12 picks the high-performance adapter
      (`host.adapter.chosen` in perf_debug.log); NvOptimus/Amd
      symbols exported (`dumpbin /exports gorillaluxe.exe`).
- [ ] Chrome/WebView stability: open Chrome 10 min → no
      `UNSUPPORTED_VK_APP`, no compositor crash (Vulkan is `-feature
      -Vulkan` — verify it stays off).

## 3. Guest boot / Загрузка

- [ ] Cold boot (fresh install, no snapshot) completes in ≤ 3 min on
      the minimum target (i3-1115G4/8 GB) — measure and record.
- [ ] QuickBoot resume (2nd launch) restores the desktop in seconds.
- [ ] `FocusedWindows` non-empty after boot; no `[Gesture Monitor]`
      windows (`navigation_mode=0` applied).
- [ ] Zero ANR dialogs after boot settles; watchdog refocuses HOME if
      the window system wedges.

## 4. Apps / Приложения

- [ ] Chrome: opens, renders a page, scrolling works, omnibox text
      entry works (physical keyboard → `event text`).
- [ ] Play Store (google_apis_playstore image): opens, sign-in screen
      reachable (document account requirement — installs need sign-in).
- [ ] APK install: drop `.apk` → install toast → app launches.
- [ ] Back/Home dock buttons drive `KEYCODE_BACK` / HOME.

## 5. Input / Ввод

- [ ] Taps land on launcher icons (FocusedWindows populated).
- [ ] Scroll/swipe continuous.
- [ ] Text entry incl. Cyrillic + surrogate-pair chars.
- [ ] ESC → Back; F6 infinite-swipe aim; WASD keymapper hold.

## 6. Frames / Кадры

- [ ] Capture mode = `grpc` in gorillaluxe.log; delivered FPS and
      capture latency in perf_debug.log — record numbers vs target
      (≥30 FPS @720p on i3-1115G4).
- [ ] HUD overlay shows FPS/frame-time/host-GPU/backend live.
- [ ] adb fallback works with gRPC disabled (kill the stream →
      `screencap` takes over).
- [ ] SCR dock button saves a correct-orientation screenshot.

## 7. Stability / Стабильность

- [ ] Force a panic (test hook) → `crash\panic-*.dmp` written +
      backtrace in gorillaluxe.log; app doesn't silently vanish.
- [ ] Kill emulator mid-session → UI detects death, records gpu
      crash, offers/auto-restarts on fallback renderer.
- [ ] App exit saves QuickBoot snapshot (log: "QuickBoot snapshot
      saved") → next launch is warm.

## 8. Security / Безопасность

- [ ] `netstat -ano | findstr "LISTENING"` while running → only
      127.0.0.1 binds for 5554/5555/8554; NO `:4444` listener.
- [ ] gRPC without `authorization` header is rejected
      (`-grpc-use-token` armed).
- [ ] A second/foreign emulator → attach refused or ownership
      logged (no blind adopt).
- [ ] No secrets in `gorillaluxe.log`/`perf_debug.log` (grep the
      console token — must NOT appear).

## 9. Packaging / Упаковка

- [ ] CI run produces `GorillaLuxe-Win64.zip` < 35 MB with
      `SHA256SUMS.txt`; hashes verify on a clean machine.
- [ ] Binaries launch standalone on a machine without Rust installed.
