# GorillaLuxe — Architecture / Архитектура

Low-disk Windows Android virtualization engine: WHPX VMM, DX12
zero-copy compositor, Luxe Dark bilingual UI.
Низкодисковый движок виртуализации Android для Windows: VMM на WHPX,
композитор DX12 zero-copy, двуязычный интерфейс Luxe Dark.

## Workspace layout / Структура воркспейса

| Crate | Purpose | Назначение |
|---|---|---|
| `gorillaluxe` | GUI shell `gorillaluxe.exe` — window, wnd_proc, render loop | GUI-оболочка — окно, wnd_proc, цикл рендера |
| `gorillaluxe-cli` | Headless entry `gorillaluxe-cli.exe` — self-tests, `--auto-setup` | Консольная утилита — самотесты, `--auto-setup` |
| `gorillaluxe-core` | All engine logic (below) | Вся логика движка (ниже) |

## `gorillaluxe-core` modules / Модули

### `vmm` + `whpx` + `guest` (Phase 1 / Фаза 1)
- **`whpx.rs`** — runtime-loaded `WinHvPlatform.dll` bindings
  (`WHv*Api` via `GetProcAddress`), capability query, partition/vCPU.
  Загрузка WHPX без link-time зависимости.
- **`vmm.rs`** — partition lifecycle, GPA memory map, vCPU registers,
  smoke test (memory-mailbox completion at GPA `0x2000`).
  Жизненный цикл раздела, память, регистры, смоук-тест.
- **`guest.rs`** — micro-payload executor used by the WHPX smoke test.

### `gfx` — DX12 compositor (Phase 2 / Фаза 2)
Zero CPU pixel copies in steady state
(`stats().steady_state_cpu_copy_bytes == 0`).
Ноль копий пикселей через CPU в стационарном режиме.

| File | Role | Роль |
|---|---|---|
| `device.rs` | Adapter pick (HW first, WARP with `--ci-software-gpu`), device/queue/fence, `submit_wait`, `wait_idle` | Выбор адаптера, устройство/очередь/фенс |
| `buffers.rs` | Committed resources, barriers, shared NT handles (`CreateSharedHandle`/`OpenSharedHandle` for resources **and fences**), upload/readback | Ресурсы, барьеры, общие NT-хэндлы, upload/readback |
| `shader.rs` | Runtime HLSL via `d3dcompiler_47.dll`; sources: `PATTERN_SRC` (selftest checker), `AMBIENT_SRC` (Luxe Dark backdrop), `CAS_SRC`, `BLIT_*`, `UI_*` | Компиляция HLSL в рантайме; все шейдеры |
| `pattern.rs` | Procedural guest frame (compute pass); CPU twin `pattern_image` for verification; ambient pass | Процедурный кадр гостя + ambient-фон |
| `cas.rs` | AMD FidelityFX CAS compute (sharpness `0.0..1.0`), Rust reference + SSIM | CAS-шарпинг, эталон и SSIM |
| `composite.rs` | Pipeline glue: guest/ambient → CAS → blit → UI | Склейка конвейера |
| `swapchain.rs` | DXGI 1.6 flip swapchain (`FLIP_DISCARD`, latency waitable), `resize()` for `WM_SIZE` | Flip-свопчейн, `resize()` |
| `selftest.rs` | Automated Verification Test 2 (zero-copy proof, CAS SSIM, Cyrillic atlas, glyph-fill QA, hot-swap, present) | Приёмочный тест фазы 2 |

### `ui` — Luxe Dark overlay (Phase 2+3 / Фазы 2–3)
| File | Role | Роль |
|---|---|---|
| `theme.rs` | Palette (`#0D0E12`, `#161821`, `#F5A623`), radii, spacing tokens | Токены палитры и геометрии |
| `atlas.rs` | GDI `Segoe UI` R8 font atlas — Latin + Cyrillic `U+0400..052F`; `glyph_fill_violations` QA | Шрифтовой атлас + проверка «сплошных блоков» |
| `scene.rs` | Scene graph → `Frame{instances, regions}`: dock (Zone A), HUD (B), keymapper canvas (C), settings panel, toasts, WHPX banner; `hit()` region test | Сцена → инстансы + хит-регионы для мыши |
| `renderer.rs` | `UiRenderer` (SDF rounded rects + glyph quads, premultiplied alpha) and `BlitPass` | SDF-рендер UI + fullscreen blit |
| `keymapper.rs` | Zone C state: widget kinds (Tap/WASD/FpsAim/Macro), drag/bind, `keymap.json` save/load | Состояние кеймаппера, сериализация |
| `settings_panel.rs` | Quick Settings state: `RES_PRESETS` (1080p/1440p/4K), `FPS_PRESETS` (60/120/240) | Пресеты панели настроек |
| `toast.rs` | Animated Luxe Dark toasts (APK drop progress, notifications) | Анимированные тосты |

### `input` (Phase 3 / Фаза 3)
- **`virtio_input.rs`** — Linux Multi-Touch Protocol Type B event model
  (`ABS_MT_SLOT`, `ABS_MT_TRACKING_ID`, `ABS_MT_POSITION_X/Y`,
  `SYN_REPORT`); event batches destined for the virtio-input queue.
  Модель MT-B — события для будущего virtio-input.
- **`raw.rs`** — `WM_INPUT` plumbing: `RegisterRawInputDevices`
  (`RIDEV_INPUTSINK`) + `GetRawInputData` delta extraction.
- **`infinite_swipe.rs`** — FPS aiming state machine: relative mouse
  deltas drive touch slot 0 from anchor `(0.75W, 0.50H)`; at the 10 %
  border the contact is released and re-touched on the next 1 ms tick
  (spec §3.2) — no camera stall at screen edges.
  Сброс слота на границе за <2 мс — камера не упирается в край.
- **`selftest.rs`** — Automated Verification Test 3: 10,000 px swipe,
  tracking-ID gap ≤ 2 ticks, stall detector, keymap roundtrip,
  WASAPI probe.

### `audio` (Phase 3 / Фаза 3)
- **`wasapi.rs`** — WASAPI shared-mode output for `virtio-snd`:
  `MMDeviceEnumerator → IMMDevice → IAudioClient`, ≤ 10 ms engine
  period (falls back to device minimum), graceful `Err` on headless
  hosts without endpoints.
  WASAPI shared-mode ≤10 мс с мягким фолбэком без аудиовыхода.

### `system` (Phase 3+4 / Фазы 3–4)
- **`env_doctor.rs`** — WHPX diagnosis (`diagnose_host`), UAC
  `ShellExecuteW("runas")` → `gorillaluxe.exe --elevated-enable-whpx`
  → `dism` enables `HypervisorPlatform` + `VirtualMachinePlatform` +
  `bcdedit hypervisorlaunchtype auto`, marker + log in
  `%LOCALAPPDATA%\GorillaLuxe`, `restart_pc()`.
  Диагностика WHPX и «Включить в 1 клик» через UAC.
- **`manifest.rs`** — `dl.google.com/android/repository` XML resolver:
  `repository2-1.xml` (emulator + platform-tools) and `sys-img2-1.xml`
  (x86_64 system images) → concrete `.zip` URLs.
  Парсер манифестов репозитория Google → прямые ссылки на zip.
- **`android_runtime.rs`** — real Android bootstrapper/engine bridge:
  install-state probe under `%LOCALAPPDATA%\GorillaLuxe\runtime\`,
  resumable `DownloadJob` (PowerShell bootstrapper), AVD writer,
  headless `emulator.exe -no-window -accel whpx -gpu host|swiftshader_indirect`
  spawn (gfxstream/ANGLE, no Qt window), adb bridge (`install -r`,
  `input tap/swipe`, `setprop persist.sys.locale`), and `exec-out
  screencap` → GDI+ PNG → RGBA frame capture feeding the DX12/CAS
  pipeline. Загрузчик и мост к headless-движку Google.
- **`device_profile.rs`** — S23 Ultra / ROG Phone 7 / Pixel 8 Pro
  presets (`model/brand/board/dpi/mac`), `setprop` script +
  kernel-cmdline fragment, `binfmt_misc` AArch64/ARMv7 descriptors for
  `libndk_translation`, `locale_sync_cmds` (ru-RU/en-US).
  Профили устройств и ARM64-трансляция.
- **`sensors.rs`** — virtual sensor HAL twin: deterministic 1D Perlin
  micro-noise (±0.002 m/s² accel, 100 Hz) + gyro + lux; identical
  generator to `sensors.gorillaluxe.so`. Перлин-шум сенсоров.
- **`shm_publisher.rs`** — named shared memory
  `Global\GorillaLuxe_Frame_<ID>` + named event
  `Global\GorillaLuxe_Event_<ID>`; `[seq][len][payload]` layout,
  publisher + reader. Именованная память кадров + событие.
- **`snapshot.rs`** — LZ4 VM snapshot save/restore (`lz4_flex`,
  size-prepended frames). LZ4-снимки ВМ.
- **`cpu_pinning.rs`** — hybrid P/E topology via
  `GetLogicalProcessorInformationEx` + `GetSystemCpuSetInformation`,
  `SetThreadSelectedCpuSets` P-core pinning.
  Гибридная топология P/E-ядер и закрепление потоков.
- **`downloader.rs`** — native resumable downloader (ureq+Schannel,
  `.part` resume + HTTP Range, 3 attempts, manifest SHA-256/SHA-1
  verify, traversal-safe ZIP extract via `enclosed_name`, throttled
  `DlEvent` progress). Нативный загрузчик с докачкой и SHA-256.
- **`logger.rs`** — leveled runtime log at
  `%LOCALAPPDATA%\GorillaLuxe\logs\gorillaluxe.log`, 5 MiB rotation.
  Логирование с ротацией.
- **`selftest.rs`** — Automated Verification Test 4 runner/report.

### `vmm/long_mode.rs` (Phase 4 / Фаза 4)
- PML4→PDPT→PD identity map (2 MiB pages), 64-bit GDT, `EFER.LME|LMA`,
  `CR4.PAE`, `CR0.PG`, `GDTR`, CS L-attribute — the same vCPU state the
  Linux x86_64 64-bit boot protocol expects after `startup_64`.
  64-битное состояние vCPU для загрузки Linux.
- 64-bit payload proof: `mov r64,imm64` + qword mailbox + HLT,
  verified `RAX` signature + CS L-bit under WHPX.

### Android runtime state machine / Машина состояний
`CheckingEnvironment → MissingRuntime → Downloading{step,MB,MB/s} →
VerifyingArchive → Extracting → CreatingAvd → StartingEmulator →
WaitingForBoot{elapsed_s} → Running → Stopped`, `Failed{ru,en}` + Retry.
Downloads are native Rust (`system/downloader.rs`) — the PowerShell
bootstrapper was removed; `emulator.exe` runs inside a Job Object with
`KILL_ON_JOB_CLOSE` (no orphan qemu), input flows through a persistent
`adb shell` stdin pipe (<5 ms), and frames arrive as raw `screencap`
(12-byte header + RGBA8888 — no PNG/GDI+).

### `i18n` + `config`
- **`i18n.rs`** — atomic `set_lang`/`current_lang`, `tr()` table for
  all UI/CLI strings (EN/RU hot-swap < 1 ms).
  Атомарное переключение языка без рестарта.
- **`config.rs`** — `%LOCALAPPDATA%\GorillaLuxe\config.cfg`
  (lang, ci_software_gpu, cas_sharpness, res, fps_cap) +
  `GetUserDefaultUILanguage` autodetect.

## `gorillaluxe.exe` interactive flow / Интерактивный поток

```
wnd_proc ──► App (Mutex) ──► build_frame(state) ──► Frame{instances, regions}
   ▲              ▲                                        │
   └── hit-test ──┴── regions stored back into App ◄────────┘
```

- `WM_LBUTTONDOWN/UP`, `WM_MOUSEMOVE` → dock clicks, hover, widget
  drags, slider scrub.
- `WM_INPUT` → raw mouse deltas → `InfiniteSwipe` (F6 toggles FPS aim).
- `WM_DROPFILES` → `.apk`/`.xapk` → animated install toast +
  `adb install -r` into the running guest.
- `WM_SIZE` → `swapchain.resize()` + CAS/UI viewport update.
- Hotkeys: F1 keymapper, F2 settings, F3 HUD, F4 RU/EN (+ guest locale
  sync), F5 WHPX setup, F6 FPS aim (adb swipe forward), ESC quit.
- Zone D onboarding: `install_state()` → Download (native ureq+zip
  with SHA-256 + step labels) / Launch Android → Job-bound headless
  emulator → raw `exec-out screencap` (12B header + RGBA8888) frames →
  `android_tex` → CAS → swapchain. Clicks forward through the
  persistent `adb shell` pipe (<5 ms).

## CI pipeline / Конвейер CI

`windows-latest` (MSVC): `cargo fmt --check` + `cargo clippy -D
warnings` → `cargo build/test --release --locked` → Phase 1
`--self-test` → Phase 2 `--self-test-gfx --ci-software-gpu` → Phase 3
`--self-test-input` → Phase 4 `--self-test-phase4` (EN + RU; live
manifest resolve + real-archive download + AVD validation + raw
screencap + atlas streak checks) → `GorillaLuxe-Win64.zip` +
`SHA256SUMS.txt` (< 35 MB budget).
