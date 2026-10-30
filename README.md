# GorillaLuxe

A low-disk, CI-driven Android virtualization engine for Windows 10/11 x64 — Luxe Dark UI, DirectX 12 compositor with AMD FidelityFX CAS sharpening, WHPX acceleration, and the official open-source Google emulator engine (`gfxstream` / `ANGLE` / `qemu-whpx`) running a real Android 13 x86_64 guest under the hood.

Легковесный движок виртуализации Android для Windows 10/11 x64 — тёмный интерфейс Luxe Dark, композитор DirectX 12 с резкостью AMD FidelityFX CAS, аппаратное ускорение WHPX и официальный открытый движок эмулятора Google (`gfxstream`/`ANGLE`/`qemu-whpx`), запускающий настоящий Android 13 x86_64.

---

## Install / Установка

1. Download `GorillaLuxe-Win64.zip` from GitHub Actions artifacts, extract anywhere (e.g. `D:\GorillaLuxe\` — paths with spaces are supported).
   Скачайте `GorillaLuxe-Win64.zip`, распакуйте в любую папку — пути с пробелами поддерживаются.
2. Run `gorillaluxe.exe`. Запустите `gorillaluxe.exe`.
3. If WHPX is off, click **⚡ Enable WHPX Acceleration in 1 Click** (UAC prompt → enables `HypervisorPlatform` + `VirtualMachinePlatform` + `bcdedit hypervisorlaunchtype auto`), then restart the PC.
   Если WHPX выключен — нажмите **⚡ Включить ускорение WHPX в 1 клик** и перезагрузите ПК.
4. On first boot the Zone D card offers **⬇ Download Android Runtime** — it natively downloads (with resume + SHA-256 checks) Google's platform-tools, the headless emulator engine, and the Android 13 x86_64 system image (~1.4 GB) into `%LOCALAPPDATA%\GorillaLuxe\runtime\`, then auto-boots.
   При первом запуске нажмите **⬇ Скачать среду Android** — встроенный загрузчик Rust (с докачкой и проверкой SHA-256) установит platform-tools, эмулятор и образ Android 13 x86_64, затем автоматически запустит гостя.

Verify binaries with `SHA256SUMS.txt`. Проверьте бинарники по `SHA256SUMS.txt`.

## Controls / Управление

| Key | Action |
|---|---|
| F1 | Keymapper canvas (Zone C) / Раскладка |
| F2 | Quick settings — CAS sharpness, resolution, FPS cap / Быстрые настройки |
| F3 | HUD toggle / HUD |
| F4 | RU ⇄ EN (syncs `persist.sys.locale` in the guest) |
| F5 | WHPX auto-setup / Настройка WHPX |
| F6 | FPS aim mode — Infinite Swipe / Режим прицела |
| ESC | Quit / Выход |
| Drop `.apk`/`.xapk` | Install into the running guest / Установка в гостя |

## Diagnostics / Диагностика

- Runtime log: `%LOCALAPPDATA%\GorillaLuxe\logs\gorillaluxe.log` (leveled, 5 MB rotation).
- CLI: `gorillaluxe-cli --self-test` (Phase 1 WHPX), `--self-test-gfx` (Phase 2 DX12/CAS), `--self-test-input` (Phase 3), `--self-test-phase4` (Phase 4 manifests/AVD/downloader), `setup-android` (headless runtime install), `--auto-setup` (elevated WHPX).
- Any download/boot failure shows the exact error in the UI card — never a silent reset.

## Uninstall / Удаление

Delete the install folder + `%LOCALAPPDATA%\GorillaLuxe\`. The emulator engine is bound to a Job Object with `KILL_ON_JOB_CLOSE` — no orphan `emulator.exe`/`qemu` processes survive exit.

## Layout / Структура

See `ARCHITECTURE.md` (EN+RU) and `docs/audit/AUDIT_REPORT.md` for the full module map, the WHPX-VMM vs Google-engine ADR, and the render-path honesty notes (zero-copy shared texture vs CPU frame bridge).
