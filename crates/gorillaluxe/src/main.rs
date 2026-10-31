//! gorillaluxe.exe — Luxe Dark player shell.
//!
//! Phase 2 delivered the DX12 zero-copy compositor (guest shared
//! texture -> CAS -> flip swapchain) with the bilingual Luxe Dark UI.
//! Phase 3 adds full mouse interactivity (dock buttons, hover,
//! sliders, keymapper canvas, WHPX one-click setup), WM_SIZE
//! swapchain resizing, raw-input Infinite Swipe, and the ambient
//! Luxe Dark backdrop.
#![windows_subsystem = "windows"]

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};

use gorillaluxe_core::config::Config;
use gorillaluxe_core::gfx::buffers::{barrier, create_texture_rgba, upload_texture};
use gorillaluxe_core::gfx::composite::Composite;
use gorillaluxe_core::gfx::device::Dx12;
use gorillaluxe_core::gfx::swapchain::SwapChain;
use gorillaluxe_core::i18n::{current_lang, set_lang, tr, Lang};
use gorillaluxe_core::input::infinite_swipe::InfiniteSwipe;
use gorillaluxe_core::input::raw;
use gorillaluxe_core::system::downloader::DlEvent;
use gorillaluxe_core::system::{android_runtime, env_doctor};
use gorillaluxe_core::ui::atlas;
use gorillaluxe_core::ui::keymapper::WidgetKind;
use gorillaluxe_core::ui::renderer::UiRenderer;
use gorillaluxe_core::ui::scene::{
    build_frame, hit, Frame, HudStats, Onboard, Region, RegionId, UiState, WhpxBanner,
};
use gorillaluxe_core::ui::settings_panel::{FPS_PRESETS, RES_PRESETS};
use gorillaluxe_core::APP_NAME;
use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COMMON,
    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
    D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_RENDER_TARGET,
};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, EndPaint, FillRect, SetBkMode, SetTextColor, TextOutW,
    UpdateWindow, HDC, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForSystem, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_ESCAPE, VK_F1, VK_F2, VK_F3, VK_F4, VK_F5, VK_F6,
};
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;

// Luxe Dark palette (GDI fallback path).
const LUXE_BG: u32 = 0x00141210;
const LUXE_ACCENT: COLORREF = COLORREF(0x0027A2C9);
const LUXE_TEXT: COLORREF = COLORREF(0x00E8E4DC);
const LUXE_DIM: COLORREF = COLORREF(0x00908C85);

const WM_INPUT_MSG: u32 = 0x00FF;
const WM_DROPFILES_MSG: u32 = 0x0233;

// DWM attributes (Windows 11 21H2+).
const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
const DWMWCP_ROUND: u32 = 2;

/// Active mouse drag operation.
#[derive(Clone, Copy, PartialEq)]
enum Drag {
    None,
    /// Moving a placed keymapper widget.
    Widget(usize),
    /// Dragging a new widget kind from the Zone C palette.
    Palette(WidgetKind),
    /// Scrubbing the CAS sharpness slider.
    Slider,
}

/// Shared interactive state — owned by `static APP`, mutated by
/// `wnd_proc` (same thread as the render loop).
struct App {
    ui: UiState,
    swipe: InfiniteSwipe,
    /// Regions emitted by the last `build_frame` (hit-test targets).
    regions: Vec<Region>,
    /// Window client size in pixels.
    win: (f32, f32),
    /// WM_SIZE request consumed by the render loop.
    pending_resize: Option<(u32, u32)>,
    /// Resolution preset request (compositor target).
    pending_res: Option<(u32, u32)>,
    fps_cap: u32,
    drag: Drag,
    /// Monotonic swipe tick (1 unit = 1 ms).
    tick: u64,
    /// Saved at exit.
    cfg: Config,
    // ---- Phase 4: real Android runtime -----------------------------------
    /// Active bootstrapper download (spawned by Zone D).
    dl: Option<android_runtime::DownloadJob>,
    /// Running headless emulator — spawned by us or attached to an
    /// already-running instance.
    emu: Option<android_runtime::EmulatorHandle>,
    /// Low-latency input injector (console :5554 or sendevent pipe).
    fast_input: Option<android_runtime::FastInput>,
    /// Frame channel fed by the screencap capture thread.
    frame_rx: Option<mpsc::Receiver<(Vec<u8>, u32, u32)>>,
    /// Live guest display dims (from the last captured frame).
    android_dims: Option<(u32, u32)>,
    /// Last Infinite-Swipe position forwarded via adb.
    last_swipe_pos: Option<(i32, i32)>,
    last_swipe_tick: u64,
    /// Persistent `adb shell` stdin pipe (<5 ms input forwarding).
    shell: Option<android_runtime::AdbShell>,
    /// WHPX readiness for the Zone B status pill.
    whpx_ok: bool,
}

fn app_cell() -> &'static OnceLock<Mutex<App>> {
    static APP: OnceLock<Mutex<App>> = OnceLock::new();
    &APP
}

fn app() -> &'static Mutex<App> {
    app_cell()
        .get()
        .expect("APP initialized before window creation")
}

/// Slot the resolver thread drops the spawned `DownloadJob` into —
/// the render loop picks it up on the next frame.
fn dl_slot() -> &'static Mutex<Option<Result<android_runtime::DownloadJob, String>>> {
    static SLOT: OnceLock<Mutex<Option<Result<android_runtime::DownloadJob, String>>>> =
        OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Slot the adb-ready thread drops the persistent shell into.
fn shell_slot() -> &'static Mutex<Option<android_runtime::AdbShell>> {
    static SLOT: OnceLock<Mutex<Option<android_runtime::AdbShell>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Slot the adb-ready thread drops the FastInput injector into.
fn input_slot() -> &'static Mutex<Option<android_runtime::FastInput>> {
    static SLOT: OnceLock<Mutex<Option<android_runtime::FastInput>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Zone D → resolve Google manifests, then start the native download.
fn start_android_download(a: &mut App) {
    // Debounce: exactly one resolve+download pipeline may run at a
    // time. A failed resolve surfaces as `Failed` within ms and rapid
    // clicks must never spawn a thread per click.
    if a.dl.is_some()
        || !matches!(
            a.ui.onboard,
            Onboard::MissingRuntime | Onboard::Failed { .. } | Onboard::Stopped | Onboard::Hidden
        )
    {
        return;
    }
    let slot = dl_slot();
    *slot.lock().unwrap() = None;
    a.ui.onboard = Onboard::CheckingEnvironment;
    std::thread::spawn(move || {
        let r = android_runtime::resolve_runtime_urls()
            .and_then(|urls| android_runtime::DownloadJob::start(&urls));
        *dl_slot().lock().unwrap() = Some(r);
    });
}

/// Slot the boot-completed poller drops `true` into once the guest
/// reports `sys.boot_completed=1` (clears the "Booting" HUD chip).
fn booted_slot() -> &'static Mutex<bool> {
    static SLOT: OnceLock<Mutex<bool>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(false))
}

/// Zone D → launch the headless emulator + start the frame pump.
fn start_android(a: &mut App) {
    a.ui.onboard = Onboard::StartingEmulator;
    let profile = &gorillaluxe_core::system::device_profile::PRESETS[0];
    // GPU-aware fallback chain (host → angle_indirect → swiftshader):
    // Intel iGPUs crash `-gpu host` at Vulkan ext-mem init, so the
    // chain auto-advances when the engine dies inside 12 s.
    // WHPX accel first, `-accel auto` as the outer fallback.
    let res =
        android_runtime::launch_resilient(profile, "whpx", a.cfg.ci_software_gpu).or_else(|e| {
            android_runtime::log_line(&format!("whpx chain exhausted ({e}); retry accel=auto"));
            android_runtime::launch_resilient(profile, "auto", a.cfg.ci_software_gpu)
        });
    match res {
        Ok(emu) => {
            a.emu = Some(emu);
            a.ui.booted = false;
            *booted_slot().lock().unwrap() = false;
            a.ui.onboard = Onboard::WaitingForBoot {
                elapsed_s: 0,
                log: "engine spawned — waiting for guest framebuffer".to_string(),
            };
            let (tx, rx) = mpsc::channel();
            a.frame_rx = Some(rx);
            // Frame bridge: ONE persistent `adb exec-out` screencap
            // pipe — zero process spawns per frame (60 FPS path).
            // Starts IMMEDIATELY so the boot animation streams live.
            // Per-frame `grab_frame` stays as the fallback.
            std::thread::spawn(move || {
                let mut stream = match android_runtime::FrameStream::open() {
                    Ok(s) => {
                        android_runtime::log_line("frame stream: persistent screencap pipe");
                        Some(s)
                    }
                    Err(e) => {
                        android_runtime::log_line(&format!(
                            "frame stream open failed ({e}) — per-frame fallback"
                        ));
                        None
                    }
                };
                loop {
                    let f = match &mut stream {
                        Some(s) => s.next_frame(),
                        None => android_runtime::grab_frame(),
                    };
                    match f {
                        Ok(frame) => {
                            if tx.send(frame).is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            if stream.is_some() {
                                android_runtime::log_line(&format!(
                                    "frame stream died ({e}) — per-frame fallback"
                                ));
                                stream = None;
                                continue;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(300));
                        }
                    }
                    if stream.is_none() {
                        std::thread::sleep(std::time::Duration::from_millis(33));
                    }
                }
            });
            // Persistent low-latency input shell + injector once the
            // guest is up.
            std::thread::spawn(move || {
                if android_runtime::wait_for_device() {
                    match android_runtime::AdbShell::open() {
                        Ok(sh) => {
                            android_runtime::log_line("persistent adb shell attached");
                            *shell_slot().lock().unwrap() = Some(sh);
                        }
                        Err(e) => android_runtime::log_line(&format!("adb shell: {e}")),
                    }
                    match android_runtime::FastInput::connect() {
                        Ok(fi) => *input_slot().lock().unwrap() = Some(fi),
                        Err(e) => android_runtime::log_line(&format!("fast input: {e}")),
                    }
                    // Boot-completion poll — clears the HUD chip; runs
                    // in parallel with the live frame stream.
                    if android_runtime::boot_completed(240) {
                        android_runtime::log_line("sys.boot_completed=1");
                        *booted_slot().lock().unwrap() = true;
                    }
                }
            });
        }
        Err(e) => {
            android_runtime::log_line(&format!("emulator launch failed: {e}"));
            a.ui.onboard = Onboard::failed(e);
        }
    }
}

/// Write one line into the persistent guest shell (<5 ms). Returns
/// `false` when no shell is attached so the caller can fall back to a
/// one-shot adb process.
fn shell_cmd(a: &mut App, cmd: &str) -> bool {
    if let Some(sh) = &mut a.shell {
        if sh.cmd(cmd).is_ok() {
            return true;
        }
        android_runtime::log_line("adb shell pipe lost — reopening");
        a.shell = None;
    }
    false
}

/// Tap through the low-latency injector (emulator console `event send`
/// ~1 ms, or `sendevent` batch ~3 ms). `false` when the fast path is
/// unavailable so the caller can degrade to a shell `input tap`.
fn fast_tap(a: &mut App, gx: u32, gy: u32) -> bool {
    if let (Some(fi), Some(sh)) = (a.fast_input.as_mut(), a.shell.as_mut()) {
        if fi.tap(sh, gx, gy).is_ok() {
            return true;
        }
        android_runtime::log_line("fast input lost — degraded to shell");
        a.fast_input = None;
    }
    false
}

/// Aim/touch move through the fast injector. `press` sends DOWN at the
/// point (first feed of a drag), otherwise MOVE.
fn fast_aim(a: &mut App, x: i32, y: i32, press: bool) -> bool {
    let (x, y) = (x.max(0) as u32, y.max(0) as u32);
    if let (Some(fi), Some(sh)) = (a.fast_input.as_mut(), a.shell.as_mut()) {
        let r = if press {
            fi.down(sh, x, y)
        } else {
            fi.mv(sh, x, y)
        };
        if r.is_ok() {
            return true;
        }
        a.fast_input = None;
    }
    false
}

/// Release a held touch (FPS aim off / drag end).
fn fast_up(a: &mut App) {
    if let (Some(fi), Some(sh)) = (a.fast_input.as_mut(), a.shell.as_mut()) {
        let _ = fi.up(sh);
    }
}

/// Whether a live Android guest is attached.
fn android_running(a: &mut App) -> bool {
    a.emu.as_mut().is_some_and(android_runtime::alive)
}

fn main() {
    // CRITICAL: declare Per-Monitor-V2 DPI awareness BEFORE any HWND,
    // font, or DXGI object exists — everything after this runs in
    // physical pixels with zero DWM bitmap stretching.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mut cfg = Config::load();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--lang" => {
                if let Some(v) = args.next() {
                    if let Some(l) = Lang::from_code(&v) {
                        cfg.lang = l;
                    }
                }
            }
            "--ci-software-gpu" => cfg.ci_software_gpu = true,
            // Elevated worker spawned by `env_doctor::launch_elevated_setup`
            // (UAC `runas` verb): enables Windows features and exits.
            "--elevated-enable-whpx" => {
                match env_doctor::run_elevated_setup() {
                    Ok(_) => eprintln!("{}", tr(cfg.lang, "cli.setup_done")),
                    Err(e) => eprintln!("WHPX setup failed: {e}"),
                }
                return;
            }
            "--auto-setup" => {
                if env_doctor::is_elevated() {
                    let _ = env_doctor::run_elevated_setup();
                } else {
                    let _ = env_doctor::launch_elevated_setup();
                }
                return;
            }
            _ => {}
        }
    }

    unsafe { run_app(cfg) };
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn run_app(cfg: Config) {
    set_lang(cfg.lang);

    // Shared app state must exist before WM_CREATE fires.
    let ui = UiState {
        lang: cfg.lang,
        sharpness: cfg.cas_sharpness,
        fps_idx: FPS_PRESETS
            .iter()
            .position(|&f| f == cfg.fps_cap)
            .unwrap_or(1),
        res_idx: RES_PRESETS
            .iter()
            .position(|&r| r == (cfg.res_w, cfg.res_h))
            .unwrap_or(0),
        ..Default::default()
    };
    let _ = app_cell().set(Mutex::new(App {
        ui,
        swipe: InfiniteSwipe::new(cfg.res_w, cfg.res_h),
        regions: Vec::new(),
        win: (1280.0, 720.0),
        pending_resize: None,
        pending_res: None,
        fps_cap: cfg.fps_cap,
        drag: Drag::None,
        tick: 0,
        cfg: cfg.clone(),
        dl: None,
        emu: None,
        fast_input: None,
        frame_rx: None,
        android_dims: None,
        last_swipe_pos: None,
        last_swipe_tick: 0,
        shell: None,
        whpx_ok: false,
    }));

    let hinst = HINSTANCE::from(GetModuleHandleW(None).unwrap_or_default());
    let class_name = w!("GorillaLuxeWindow");

    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: CreateSolidBrush(COLORREF(LUXE_BG)),
        lpszMenuName: windows::core::PCWSTR::null(),
        lpszClassName: class_name,
        hIconSm: Default::default(),
    };
    RegisterClassExW(&wc);

    // Physical-pixel window: with PMv2 awareness the values below are
    // raw device pixels. Adjust the window rect so the CLIENT area is
    // exactly 1600x900 physical px — the swapchain will match it 1:1.
    const CLIENT_W: i32 = 1600;
    const CLIENT_H: i32 = 900;
    let dpi = GetDpiForSystem();
    let mut wr = RECT {
        left: 0,
        top: 0,
        right: CLIENT_W,
        bottom: CLIENT_H,
    };
    let _ = AdjustWindowRectExForDpi(
        &mut wr,
        WS_OVERLAPPEDWINDOW,
        false,
        WS_EX_NOREDIRECTIONBITMAP | WS_EX_APPWINDOW,
        dpi,
    );
    let hwnd = CreateWindowExW(
        WS_EX_NOREDIRECTIONBITMAP | WS_EX_APPWINDOW,
        class_name,
        w!("GorillaLuxe"),
        WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        wr.right - wr.left,
        wr.bottom - wr.top,
        None,
        None,
        Some(hinst),
        Some(Box::into_raw(Box::new(cfg.lang)) as *const _),
    )
    .unwrap();

    // Windows 11 dark title bar + rounded corners (spec §1).
    let dark: i32 = 1;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWINDOWATTRIBUTE(DWMWA_USE_IMMERSIVE_DARK_MODE as i32),
        &dark as *const _ as *const _,
        std::mem::size_of::<i32>() as u32,
    );
    let corner: u32 = DWMWCP_ROUND;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWINDOWATTRIBUTE(DWMWA_WINDOW_CORNER_PREFERENCE as i32),
        &corner as *const _ as *const _,
        std::mem::size_of::<u32>() as u32,
    );

    // Phase 3: raw mouse input + OS drag-and-drop for APK/XAPK.
    if let Err(e) = raw::register_mouse(hwnd) {
        eprintln!("raw input: {e}");
    }
    DragAcceptFiles(hwnd, true);

    // env_doctor: non-blocking WHPX diagnosis -> banner in the UI.
    {
        let report = env_doctor::diagnose_host();
        let mut a = app().lock().unwrap();
        a.whpx_ok = report.whpx_ok;
        a.ui.banner = if report.whpx_ok {
            WhpxBanner::Hidden
        } else if env_doctor::setup_completed() {
            WhpxBanner::RestartReady
        } else {
            WhpxBanner::Missing(report.detail.unwrap_or_else(|| "unknown".into()))
        };
        // Restore persisted keymap.
        a.ui.keymap = gorillaluxe_core::ui::keymapper::Keymapper::load();
        // Zone D first-boot onboarding: card state from install state.
        a.ui.onboard = match android_runtime::install_state() {
            android_runtime::InstallState::Ready { .. } => Onboard::Stopped,
            android_runtime::InstallState::Partial(m) => {
                android_runtime::log_line(&format!("runtime incomplete: {m}"));
                Onboard::MissingRuntime
            }
            android_runtime::InstallState::Missing => Onboard::MissingRuntime,
        };
        a.ui.status = engine_status(a.whpx_ok, &a.ui.onboard);
    }

    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = UpdateWindow(hwnd);

    if let Err(e) = run_dx12(hwnd, &cfg) {
        eprintln!("DX12 compositor unavailable ({e}); falling back to GDI shell");
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Real production status line for the Zone B pill — replaces the
/// MVP "Phase 2" placeholder with live engine state.
fn engine_status(whpx_ok: bool, onboard: &Onboard) -> String {
    let a13 = match onboard {
        Onboard::Running | Onboard::Hidden => "Running",
        Onboard::Stopped => "Ready",
        Onboard::StartingEmulator | Onboard::WaitingForBoot { .. } => "Booting",
        Onboard::Failed { .. } => "Error",
        _ => "Missing",
    };
    format!(
        "GorillaLuxe Engine | WHPX: {} | Android 13: {a13}",
        if whpx_ok { "Ready" } else { "Off" }
    )
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    (
        (rc.right - rc.left).max(1) as u32,
        (rc.bottom - rc.top).max(1) as u32,
    )
}

fn run_dx12(hwnd: HWND, cfg: &Config) -> Result<(), String> {
    let dx = Dx12::new(cfg.ci_software_gpu)?;
    let (mut win_w, mut win_h) = client_size(hwnd);
    let mut swap = SwapChain::new(&dx, hwnd, win_w, win_h)?;
    let comp = Composite::new(&dx)?;
    let atlas = atlas::build_ui_atlases()?;
    let ui = UiRenderer::new(&dx, &atlas)?;

    // Guest framebuffer analog at the compositor resolution preset.
    let mut res = (cfg.res_w, cfg.res_h);
    let mut guest = create_texture_rgba(
        &dx,
        res.0,
        res.1,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_COMMON,
        true,
    )?;
    let mut sharpened = create_texture_rgba(
        &dx,
        win_w,
        win_h,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_COMMON,
        false,
    )?;

    let mut guest_state = D3D12_RESOURCE_STATE_COMMON;
    let mut out_state = D3D12_RESOURCE_STATE_COMMON;
    // Phase 4: live Android framebuffer texture (created on first frame).
    let mut android_tex: Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource> = None;
    let mut android_tex_dims: (u32, u32) = (0, 0);
    let mut frames = 0u64;
    let mut last = std::time::Instant::now();
    let mut fps = 0u32;
    let t0 = std::time::Instant::now();
    let mut frame_deadline = t0;

    unsafe {
        'outer: loop {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    break 'outer;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            // ---- Consume interactive state --------------------------------
            let (sw, sh);
            {
                let mut a = app().lock().unwrap();
                a.tick = t0.elapsed().as_millis() as u64;
                a.ui.toasts.tick();
                a.ui.lang = current_lang();

                if let Some((rw, rh)) = a.pending_res.take() {
                    res = (rw, rh);
                    a.swipe = InfiniteSwipe::new(rw, rh);
                    dx.wait_idle()?;
                    guest = create_texture_rgba(
                        &dx,
                        res.0,
                        res.1,
                        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                        D3D12_RESOURCE_STATE_COMMON,
                        true,
                    )?;
                    guest_state = D3D12_RESOURCE_STATE_COMMON;
                    a.cfg.res_w = rw;
                    a.cfg.res_h = rh;
                }
                if let Some((nw, nh)) = a.pending_resize.take() {
                    if nw > 0 && nh > 0 && (nw != win_w || nh != win_h) {
                        win_w = nw;
                        win_h = nh;
                        dx.wait_idle()?;
                        swap.resize(&dx, win_w, win_h)?;
                        sharpened = create_texture_rgba(
                            &dx,
                            win_w,
                            win_h,
                            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                            D3D12_RESOURCE_STATE_COMMON,
                            false,
                        )?;
                        out_state = D3D12_RESOURCE_STATE_COMMON;
                    }
                    a.win = (win_w as f32, win_h as f32);
                }
                // Re-check WHPX banner after an elevated setup run.
                if matches!(a.ui.banner, WhpxBanner::Missing(_)) && env_doctor::setup_completed() {
                    a.ui.banner = WhpxBanner::RestartReady;
                }
                // ---- Android runtime state machine (spec §5.1) ------
                if let Some(r) = dl_slot().lock().unwrap().take() {
                    match r {
                        Ok(job) => {
                            a.dl = Some(job);
                            a.ui.onboard = Onboard::Downloading {
                                step: "connecting dl.google.com".into(),
                                progress: 0.0,
                                mb_done: 0.0,
                                mb_total: 0.0,
                                mbps: 0.0,
                            };
                        }
                        Err(e) => {
                            android_runtime::log_line(&format!("resolve failed: {e}"));
                            a.ui.onboard = Onboard::failed(e);
                        }
                    }
                }
                if let Some(job) = &a.dl {
                    let events = job.poll();
                    let mut finished: Option<Result<(), String>> = None;
                    for ev in events {
                        match ev {
                            DlEvent::Step { step, of, label } => {
                                android_runtime::log_line(&format!("step {step}/{of}: {label}"));
                                a.ui.onboard = Onboard::Downloading {
                                    step: format!("{step}/{of}  {label}"),
                                    progress: 0.0,
                                    mb_done: 0.0,
                                    mb_total: 0.0,
                                    mbps: 0.0,
                                };
                            }
                            DlEvent::Progress {
                                done,
                                total,
                                speed_bps,
                            } => {
                                let step = match &a.ui.onboard {
                                    Onboard::Downloading { step, .. } => step.clone(),
                                    _ => String::new(),
                                };
                                a.ui.onboard = Onboard::Downloading {
                                    step,
                                    progress: if total > 0 {
                                        done as f32 / total as f32
                                    } else {
                                        0.0
                                    },
                                    mb_done: done as f32 / 1048576.0,
                                    mb_total: total as f32 / 1048576.0,
                                    mbps: speed_bps as f32 / 1048576.0,
                                };
                            }
                            DlEvent::Verifying { label } => {
                                a.ui.onboard = Onboard::VerifyingArchive { step: label };
                            }
                            DlEvent::Extracting { label } => {
                                a.ui.onboard = Onboard::Extracting {
                                    step: format!("4/4  {label}"),
                                };
                            }
                            DlEvent::Log(l) => android_runtime::log_line(&l),
                            DlEvent::Finished(r) => finished = Some(r),
                        }
                    }
                    if let Some(r) = finished {
                        a.dl = None;
                        match (r, android_runtime::install_state()) {
                            (Ok(()), android_runtime::InstallState::Ready { .. }) => {
                                android_runtime::log_line("runtime install complete");
                                a.ui.onboard = Onboard::CreatingAvd;
                                // Auto-transition straight into boot.
                                start_android(&mut a);
                            }
                            (Err(e), _) => {
                                android_runtime::log_line(&format!("download failed: {e}"));
                                a.ui.onboard = Onboard::failed(e);
                            }
                            (Ok(()), st) => {
                                let e = match st {
                                    android_runtime::InstallState::Partial(m) => {
                                        format!("missing files: {m}")
                                    }
                                    _ => "install validation failed".to_string(),
                                };
                                android_runtime::log_line(&format!("validation failed: {e}"));
                                a.ui.onboard = Onboard::failed(e);
                            }
                        }
                    }
                }
                // Boot-elapsed tick + emulator liveness.
                let boot_elapsed = a
                    .emu
                    .as_ref()
                    .map(|e| e.launched_at().elapsed().as_secs() as u32);
                if let (Some(secs), Onboard::WaitingForBoot { elapsed_s, .. }) =
                    (boot_elapsed, &mut a.ui.onboard)
                {
                    *elapsed_s = secs;
                }
                if a.emu.is_some() && !android_running(&mut a) {
                    let was_booting = matches!(
                        a.ui.onboard,
                        Onboard::WaitingForBoot { .. } | Onboard::StartingEmulator
                    );
                    a.emu = None;
                    a.frame_rx = None;
                    a.android_dims = None;
                    a.shell = None;
                    if was_booting {
                        a.ui.onboard =
                            Onboard::failed("emulator exited during boot — see gorillaluxe.log");
                    } else {
                        a.ui.onboard = Onboard::Stopped;
                        toast(&mut a, "Android exited".into());
                    }
                }
                // Attach the persistent shell when the waiter delivers it.
                if a.shell.is_none() {
                    if let Some(sh) = shell_slot().lock().unwrap().take() {
                        a.shell = Some(sh);
                    }
                }
                if a.fast_input.is_none() {
                    if let Some(fi) = input_slot().lock().unwrap().take() {
                        a.fast_input = Some(fi);
                    }
                }
                // Boot-completion poller cleared the "Booting" chip.
                if *booted_slot().lock().unwrap() {
                    a.ui.booted = true;
                }
                let status = engine_status(a.whpx_ok, &a.ui.onboard);
                a.ui.status = status;
                sw = a.win.0;
                sh = a.win.1;
            }

            // Latest live guest frame (drain to the newest).
            let mut latest: Option<(Vec<u8>, u32, u32)> = None;
            {
                let mut a = app().lock().unwrap();
                if let Some(rx) = &a.frame_rx {
                    while let Ok(f) = rx.try_recv() {
                        latest = Some(f);
                    }
                }
                if let Some((_, w, h)) = &latest {
                    a.android_dims = Some((*w, *h));
                    if matches!(a.ui.onboard, Onboard::WaitingForBoot { .. }) {
                        android_runtime::log_line("first guest frame received — boot OK");
                        a.ui.onboard = Onboard::Running;
                    }
                }
            }

            // FPS cap pacing (plus the swapchain latency waitable).
            let frame_target =
                std::time::Duration::from_secs_f64(1.0 / app().lock().unwrap().fps_cap as f64);
            let _ = WaitForSingleObject(swap.latency_event, 500);
            let now = std::time::Instant::now();
            if now < frame_deadline {
                std::thread::sleep(frame_deadline - now);
            }
            frame_deadline = std::time::Instant::now() + frame_target;

            let (_alloc, list) = dx.cmd()?;
            let sharpness = app().lock().unwrap().ui.sharpness;
            // CAS source: the real Android framebuffer when a guest is
            // attached, else the Luxe Dark ambient backdrop.
            let src_res;
            let src_tex;
            if let Some((rgba, w, h)) = latest {
                if android_tex.is_none() || android_tex_dims != (w, h) {
                    dx.wait_idle()?;
                    android_tex = Some(create_texture_rgba(
                        &dx,
                        w,
                        h,
                        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                        D3D12_RESOURCE_STATE_COMMON,
                        false,
                    )?);
                    android_tex_dims = (w, h);
                }
                let t = android_tex.as_ref().unwrap();
                upload_texture(&dx, t, w, h, DXGI_FORMAT_R8G8B8A8_UNORM, &rgba, 4)?;
                list.ResourceBarrier(&[barrier(
                    t,
                    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                )]);
                src_tex = t;
                src_res = (w, h);
            } else {
                comp.ambient_frame(&dx, &list, &guest, res.0, res.1, guest_state)?;
                guest_state = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
                src_tex = &guest;
                src_res = res;
            }
            comp.sharpen(
                &dx,
                &list,
                src_tex,
                &sharpened,
                src_res,
                (win_w, win_h),
                sharpness,
                out_state,
            )?;
            out_state = D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;

            let idx = swap.current_index();
            let rt = &swap.buffers[idx as usize];
            list.ResourceBarrier(&[barrier(
                rt,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
            )]);
            let stats = HudStats {
                fps,
                frame_ms: if fps > 0 { 1000.0 / fps as f32 } else { 0.0 },
                backend: if dx.is_warp {
                    "DX12 Zero-Copy | WARP"
                } else {
                    "DX12 Zero-Copy"
                },
            };
            let frame: Frame = {
                let a = app().lock().unwrap();
                build_frame(sw, sh, &a.ui, &atlas, &stats)
            };
            {
                let mut a = app().lock().unwrap();
                a.regions = frame.regions;
            }
            comp.present_frame(
                &dx,
                &ui,
                &list,
                &sharpened,
                &frame.instances,
                swap.rtv_for(&dx, idx),
                win_w,
                win_h,
            )?;
            list.ResourceBarrier(&[barrier(
                rt,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                D3D12_RESOURCE_STATE_PRESENT,
            )]);
            dx.submit_wait(&list)?;
            swap.present()?;

            frames += 1;
            if last.elapsed().as_secs_f64() >= 1.0 {
                fps = frames as u32;
                frames = 0;
                last = std::time::Instant::now();
            }
        }
    }

    // Graceful engine teardown + persist UI choices on clean exit.
    {
        let mut a = app().lock().unwrap();
        a.emu = None; // Drop → child.kill() + Job close (no orphans)
        a.shell = None;
        a.cfg.lang = current_lang();
        a.cfg.cas_sharpness = a.ui.sharpness;
        a.cfg.fps_cap = a.fps_cap;
        let _ = a.cfg.save();
        let _ = a.ui.keymap.save();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Input dispatch
// ---------------------------------------------------------------------------

fn lparam_xy(lparam: LPARAM) -> (f32, f32) {
    (
        (lparam.0 & 0xFFFF) as i16 as f32,
        ((lparam.0 >> 16) & 0xFFFF) as i16 as f32,
    )
}

fn toast(a: &mut App, title: String) {
    a.ui.toasts.push(title, 2200);
}

fn on_lbutton_down(a: &mut App, x: f32, y: f32) {
    match hit(&a.regions, x, y) {
        Some(RegionId::DockLang) => {
            let next = if current_lang() == Lang::Ru {
                Lang::En
            } else {
                Lang::Ru
            };
            set_lang(next);
            a.ui.lang = next;
            sync_guest_locale(a, next);
        }
        Some(RegionId::DockKeymapper) => a.ui.keymap.toggle(),
        Some(RegionId::DockSettings) => a.ui.settings.toggle(),
        Some(RegionId::DockApk) => toast(a, tr(a.ui.lang, "ui.apk.hint").to_string()),
        Some(RegionId::DockShot) => toast(a, tr(a.ui.lang, "ui.shot.saved").to_string()),
        Some(RegionId::CasSlider) => {
            a.drag = Drag::Slider;
            apply_slider(a, x);
        }
        Some(RegionId::ResPreset(i)) => {
            a.ui.res_idx = i;
            a.pending_res = Some(RES_PRESETS[i]);
        }
        Some(RegionId::FpsPreset(i)) => {
            a.ui.fps_idx = i;
            a.fps_cap = FPS_PRESETS[i];
        }
        Some(RegionId::Palette(i)) => {
            a.drag = Drag::Palette(WidgetKind::PALETTE[i]);
            a.ui.palette_drag = Some(WidgetKind::PALETTE[i]);
        }
        Some(RegionId::Widget(i)) => {
            a.drag = Drag::Widget(i);
            a.ui.keymap.selected = Some(i);
        }
        Some(RegionId::WhpxEnable) => {
            if let Err(e) = env_doctor::launch_elevated_setup() {
                toast(a, format!("{}: {e}", tr(a.ui.lang, "ui.whpx.uac_denied")));
            }
        }
        Some(RegionId::WhpxRestart) => {
            let _ = env_doctor::restart_pc();
        }
        Some(RegionId::OnboardDownload) => start_android_download(a),
        Some(RegionId::OnboardLaunch) => start_android(a),
        _ => {}
    }
}

/// Spec §3.5: push the UI language into the running guest via adb.
fn sync_guest_locale(a: &mut App, lang: Lang) {
    if !android_running(a) {
        return;
    }
    let locale = if lang == Lang::Ru { "ru-RU" } else { "en-US" };
    let l = locale.split('-').next().unwrap_or("en");
    if !shell_cmd(a, &format!("setprop persist.sys.locale {locale}"))
        || !shell_cmd(a, &format!("setprop persist.sys.language {l}"))
    {
        std::thread::spawn(move || {
            let _ = android_runtime::set_locale(locale);
        });
    }
}

fn apply_slider(a: &mut App, x: f32) {
    // Find the slider track among regions to map x -> 0..1.
    if let Some(r) = a.regions.iter().find(|r| r.id == RegionId::CasSlider) {
        a.ui.sharpness = ((x - r.r[0]) / r.r[2]).clamp(0.0, 1.0);
    }
}

fn on_mouse_move(a: &mut App, x: f32, y: f32) {
    a.ui.mouse = (x, y);
    a.ui.hover = hit(&a.regions, x, y);
    match a.drag {
        Drag::Slider => apply_slider(a, x),
        Drag::Widget(i) => {
            let (sw, sh) = a.win;
            a.ui.keymap.move_to(i, x / sw, y / sh);
        }
        Drag::Palette(_) | Drag::None => {}
    }
}

fn on_lbutton_up(a: &mut App, x: f32, y: f32) {
    if let Drag::Palette(kind) = a.drag {
        let (sw, sh) = a.win;
        // Drop only inside the game area (not on the palette itself).
        if y < sh - 80.0 {
            a.ui.keymap.place(kind, x / sw, y / sh);
        }
        a.ui.palette_drag = None;
    }
    a.drag = Drag::None;
    // Forward the tap into the running Android guest when the click
    // landed on the game surface (no UI region hit). Persistent shell
    // first (<5 ms), one-shot adb spawn as fallback.
    if android_running(a)
        && matches!(a.ui.onboard, Onboard::Running | Onboard::Hidden)
        && hit(&a.regions, x, y).is_none()
    {
        if let Some((dw, dh)) = a.android_dims {
            let gx = (x / a.win.0 * dw as f32) as u32;
            let gy = (y / a.win.1 * dh as f32) as u32;
            if !fast_tap(a, gx, gy) && !shell_cmd(a, &format!("input tap {gx} {gy}")) {
                std::thread::spawn(move || {
                    let _ = android_runtime::send_tap(gx, gy);
                });
            }
        }
    }
}

fn on_key(a: &mut App, vk: u16, hwnd: HWND) {
    if let Some(i) = a.ui.keymap.selected {
        // Any key binds the selected widget; ESC removes it.
        if vk == VK_ESCAPE.0 {
            a.ui.keymap.remove(i);
        } else {
            a.ui.keymap.bind(i, vk);
        }
        let _ = a.ui.keymap.save();
        return;
    }
    // Bound keys forward Zone C taps into the running guest.
    if android_running(a) && !a.ui.keymap.open {
        if let Some((dw, dh)) = a.android_dims {
            let tap_pos = a.ui.keymap.widgets.iter().find_map(|w| {
                (w.key == Some(vk) && w.kind == WidgetKind::Tap)
                    .then_some(((w.x * dw as f32) as u32, (w.y * dh as f32) as u32))
            });
            if let Some((gx, gy)) = tap_pos {
                if !fast_tap(a, gx, gy) && !shell_cmd(a, &format!("input tap {gx} {gy}")) {
                    std::thread::spawn(move || {
                        let _ = android_runtime::send_tap(gx, gy);
                    });
                }
                return;
            }
        }
    }
    match vk {
        k if k == VK_F1.0 => a.ui.keymap.toggle(),
        k if k == VK_F2.0 => a.ui.settings.toggle(),
        k if k == VK_F3.0 => a.ui.hud_visible = !a.ui.hud_visible,
        k if k == VK_F4.0 => {
            let next = if current_lang() == Lang::Ru {
                Lang::En
            } else {
                Lang::Ru
            };
            set_lang(next);
            a.ui.lang = next;
            sync_guest_locale(a, next);
        }
        k if k == VK_F5.0 => {
            if matches!(a.ui.banner, WhpxBanner::Missing(_)) {
                if let Err(e) = env_doctor::launch_elevated_setup() {
                    toast(a, format!("{}: {e}", tr(a.ui.lang, "ui.whpx.uac_denied")));
                }
            }
        }
        k if k == VK_F6.0 => {
            a.ui.fps_mode = !a.ui.fps_mode;
            if a.ui.fps_mode {
                a.swipe.begin(a.tick);
            } else {
                a.swipe.end(a.tick);
                fast_up(a);
                a.last_swipe_pos = None;
            }
        }
        k if k == VK_ESCAPE.0 => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        _ => {}
    }
}

fn on_drop_files(a: &mut App, lparam: LPARAM) {
    let hdrop = HDROP(lparam.0 as *mut _);
    unsafe {
        let n = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
        for i in 0..n {
            let mut buf = [0u16; 260];
            let len = DragQueryFileW(hdrop, i, Some(&mut buf[..]));
            if len == 0 {
                continue;
            }
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            let lower = path.to_ascii_lowercase();
            if lower.ends_with(".apk") || lower.ends_with(".xapk") {
                let name = std::path::Path::new(&path)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or(path.clone());
                toast(a, format!("{} {}", tr(a.ui.lang, "ui.apk.dropped"), name));
                // Phase 4: install straight into the running guest.
                if android_running(a) {
                    let p = PathBuf::from(&path);
                    std::thread::spawn(move || {
                        let _ = android_runtime::install_package(&p);
                    });
                }
            }
        }
        DragFinish(hdrop);
    }
}

// ---------------------------------------------------------------------------

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let cs = lparam.0 as *const CREATESTRUCTW;
            let lang_ptr = (*cs).lpCreateParams as *mut Lang;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, lang_ptr as isize);
            LRESULT(0)
        }
        WM_DESTROY => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Lang;
            if !p.is_null() {
                drop(Box::from_raw(p));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_SIZE => {
            let w = (lparam.0 & 0xFFFF) as u32;
            let h = ((lparam.0 >> 16) & 0xFFFF) as u32;
            let mut a = app().lock().unwrap();
            a.win = (w as f32, h as f32);
            a.pending_resize = Some((w, h));
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = lparam_xy(lparam);
            let mut a = app().lock().unwrap();
            on_mouse_move(&mut a, x, y);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = lparam_xy(lparam);
            let mut a = app().lock().unwrap();
            on_lbutton_down(&mut a, x, y);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = lparam_xy(lparam);
            let mut a = app().lock().unwrap();
            on_lbutton_up(&mut a, x, y);
            LRESULT(0)
        }
        WM_INPUT_MSG => {
            if let Some((dx, dy)) = raw::mouse_delta(lparam) {
                let mut a = app().lock().unwrap();
                let tick = a.tick;
                if a.ui.fps_mode {
                    a.swipe.feed(dx as f32, dy as f32, tick);
                    // Forward FPS aim as touch moves on the fast
                    // injector (~16 Hz throttle): DOWN on first feed,
                    // MOVE after — <5 ms, no `input swipe` JVM spawn.
                    if android_running(&mut a) && tick.saturating_sub(a.last_swipe_tick) > 60 {
                        a.last_swipe_tick = tick;
                        let (x2, y2) = a.swipe.pos();
                        let press = a.last_swipe_pos.is_none();
                        if !fast_aim(&mut a, x2, y2, press) {
                            if let Some((x1, y1)) = a.last_swipe_pos {
                                if !shell_cmd(
                                    &mut a,
                                    &format!("input swipe {x1} {y1} {x2} {y2} 60"),
                                ) {
                                    std::thread::spawn(move || {
                                        let _ = android_runtime::send_swipe(x1, y1, x2, y2, 60);
                                    });
                                }
                            }
                        }
                        a.last_swipe_pos = Some((x2, y2));
                    }
                }
            }
            LRESULT(0)
        }
        WM_DROPFILES_MSG => {
            let mut a = app().lock().unwrap();
            on_drop_files(&mut a, lparam);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let mut a = app().lock().unwrap();
            on_key(&mut a, wparam.0 as u16, hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            let lang = {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Lang;
                if p.is_null() {
                    Lang::En
                } else {
                    *p
                }
            };
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let _ = SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, LUXE_ACCENT);
            let title = utf16(&format!("{} — {}", APP_NAME, tr(lang, "app.tagline")));
            let _ = TextOutW(hdc, 24, 24, &title);
            SetTextColor(hdc, LUXE_TEXT);
            let welcome = utf16(tr(lang, "ui.welcome"));
            let _ = TextOutW(hdc, 24, 64, &welcome);
            SetTextColor(hdc, LUXE_DIM);
            let phase = utf16(&format!(
                "{} · {}",
                tr(lang, "ui.phase1"),
                tr(lang, "ui.phase2")
            ));
            let _ = TextOutW(hdc, 24, 92, &phase);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            let hdc = HDC(wparam.0 as *mut _);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            FillRect(hdc, &rc, CreateSolidBrush(COLORREF(LUXE_BG)));
            LRESULT(1)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
