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

### `system` (Phase 3 / Фаза 3)
- **`env_doctor.rs`** — WHPX diagnosis (`diagnose_host`), UAC
  `ShellExecuteW("runas")` → `gorillaluxe.exe --elevated-enable-whpx`
  → `dism` enables `HypervisorPlatform` + `VirtualMachinePlatform` +
  `bcdedit hypervisorlaunchtype auto`, marker + log in
  `%LOCALAPPDATA%\GorillaLuxe`, `restart_pc()`.
  Диагностика WHPX и «Включить в 1 клик» через UAC.

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
- `WM_DROPFILES` → `.apk`/`.xapk` → animated install toast.
- `WM_SIZE` → `swapchain.resize()` + CAS/UI viewport update.
- Hotkeys: F1 keymapper, F2 settings, F3 HUD, F4 RU/EN, F5 WHPX
  setup, F6 FPS aim, ESC quit.

## CI pipeline / Конвейер CI

`windows-latest` (MSVC): `cargo test` → Phase 1 `--self-test` →
Phase 2 `--self-test-gfx --ci-software-gpu` → Phase 3
`--self-test-input` (EN + RU) → `GorillaLuxe-Win64.zip` artifact
(< 35 MB budget).
