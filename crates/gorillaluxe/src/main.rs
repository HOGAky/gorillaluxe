//! gorillaluxe.exe — Luxe Dark player shell.
//!
//! Phase 2 delivered the DX12 zero-copy compositor (guest shared
//! texture -> CAS -> flip swapchain) with the bilingual Luxe Dark UI.
//! Phase 3 adds full mouse interactivity (dock buttons, hover,
//! sliders, keymapper canvas, WHPX one-click setup), WM_SIZE
//! swapchain resizing, raw-input Infinite Swipe, and the ambient
//! Luxe Dark backdrop.
#![windows_subsystem = "windows"]

use std::sync::{Mutex, OnceLock};

use gorillaluxe_core::config::Config;
use gorillaluxe_core::gfx::buffers::{barrier, create_texture_rgba};
use gorillaluxe_core::gfx::composite::Composite;
use gorillaluxe_core::gfx::device::Dx12;
use gorillaluxe_core::gfx::swapchain::SwapChain;
use gorillaluxe_core::i18n::{current_lang, set_lang, tr, Lang};
use gorillaluxe_core::input::infinite_swipe::InfiniteSwipe;
use gorillaluxe_core::input::raw;
use gorillaluxe_core::system::env_doctor;
use gorillaluxe_core::ui::atlas;
use gorillaluxe_core::ui::keymapper::WidgetKind;
use gorillaluxe_core::ui::renderer::UiRenderer;
use gorillaluxe_core::ui::scene::{
    build_frame, hit, Frame, HudStats, Region, RegionId, UiState, WhpxBanner,
};
use gorillaluxe_core::ui::settings_panel::{FPS_PRESETS, RES_PRESETS};
use gorillaluxe_core::ui::theme::FONT_UI;
use gorillaluxe_core::APP_NAME;
use windows::core::w;
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COMMON,
    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE, D3D12_RESOURCE_STATE_PRESENT,
    D3D12_RESOURCE_STATE_RENDER_TARGET,
};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, EndPaint, FillRect, SetBkMode, SetTextColor,
    TextOutW, UpdateWindow, HDC, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::WaitForSingleObject;
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
}

fn app_cell() -> &'static OnceLock<Mutex<App>> {
    static APP: OnceLock<Mutex<App>> = OnceLock::new();
    &APP
}

fn app() -> &'static Mutex<App> {
    app_cell().get().expect("APP initialized before window creation")
}

fn main() {
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
        fps_idx: FPS_PRESETS.iter().position(|&f| f == cfg.fps_cap).unwrap_or(1),
        res_idx: RES_PRESETS.iter().position(|&r| r == (cfg.res_w, cfg.res_h)).unwrap_or(0),
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
        hbrBackground: CreateSolidBrush(COLORREF(LUXE_BG)).into(),
        lpszMenuName: windows::core::PCWSTR::null(),
        lpszClassName: class_name,
        hIconSm: Default::default(),
    };
    RegisterClassExW(&wc);

    let hwnd = CreateWindowExW(
        WS_EX_NOREDIRECTIONBITMAP | WS_EX_APPWINDOW,
        class_name,
        w!("GorillaLuxe"),
        WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        1280,
        720,
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
        a.ui.banner = if report.whpx_ok {
            WhpxBanner::Hidden
        } else if env_doctor::setup_completed() {
            WhpxBanner::RestartReady
        } else {
            WhpxBanner::Missing(report.detail.unwrap_or_else(|| "unknown".into()))
        };
        // Restore persisted keymap.
        a.ui.keymap = gorillaluxe_core::ui::keymapper::Keymapper::load();
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

fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    ((rc.right - rc.left).max(1) as u32, (rc.bottom - rc.top).max(1) as u32)
}

fn run_dx12(hwnd: HWND, cfg: &Config) -> Result<(), String> {
    let dx = Dx12::new(cfg.ci_software_gpu)?;
    let (mut win_w, mut win_h) = client_size(hwnd);
    let mut swap = SwapChain::new(&dx, hwnd, win_w, win_h)?;
    let comp = Composite::new(&dx)?;
    let atlas = atlas::build(FONT_UI)?;
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
                sw = a.win.0;
                sh = a.win.1;
            }

            // FPS cap pacing (plus the swapchain latency waitable).
            let frame_target = std::time::Duration::from_secs_f64(
                1.0 / app().lock().unwrap().fps_cap as f64,
            );
            let _ = WaitForSingleObject(swap.latency_event, 500);
            let now = std::time::Instant::now();
            if now < frame_deadline {
                std::thread::sleep(frame_deadline - now);
            }
            frame_deadline = std::time::Instant::now() + frame_target;

            let (_alloc, list) = dx.cmd()?;
            comp.ambient_frame(&dx, &list, &guest, res.0, res.1, guest_state)?;
            guest_state = windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
            let sharpness = app().lock().unwrap().ui.sharpness;
            comp.sharpen(
                &dx,
                &list,
                &guest,
                &sharpened,
                res,
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
                backend: if dx.is_warp { "DX12 Zero-Copy | WARP" } else { "DX12 Zero-Copy" },
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

    // Persist UI choices on clean exit.
    {
        let mut a = app().lock().unwrap();
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
            let next = if current_lang() == Lang::Ru { Lang::En } else { Lang::Ru };
            set_lang(next);
            a.ui.lang = next;
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
        _ => {}
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
}

fn on_key(a: &mut App, vk: u16, hwnd: HWND) {
    if let Some(i) = a.ui.keymap.selected {
        // Any key binds the selected widget; ESC removes it.
        if vk == VK_ESCAPE.0 as u16 {
            a.ui.keymap.remove(i);
        } else {
            a.ui.keymap.bind(i, vk);
        }
        let _ = a.ui.keymap.save();
        return;
    }
    match vk {
        k if k == VK_F1.0 as u16 => a.ui.keymap.toggle(),
        k if k == VK_F2.0 as u16 => a.ui.settings.toggle(),
        k if k == VK_F3.0 as u16 => a.ui.hud_visible = !a.ui.hud_visible,
        k if k == VK_F4.0 as u16 => {
            let next = if current_lang() == Lang::Ru { Lang::En } else { Lang::Ru };
            set_lang(next);
            a.ui.lang = next;
        }
        k if k == VK_F5.0 as u16 => {
            if matches!(a.ui.banner, WhpxBanner::Missing(_)) {
                if let Err(e) = env_doctor::launch_elevated_setup() {
                    toast(a, format!("{}: {e}", tr(a.ui.lang, "ui.whpx.uac_denied")));
                }
            }
        }
        k if k == VK_F6.0 as u16 => {
            a.ui.fps_mode = !a.ui.fps_mode;
            if a.ui.fps_mode {
                a.swipe.begin(a.tick);
            } else {
                a.swipe.end(a.tick);
            }
        }
        k if k == VK_ESCAPE.0 as u16 => unsafe {
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
                    .unwrap_or(path);
                toast(
                    a,
                    format!("{} {}", tr(a.ui.lang, "ui.apk.dropped"), name),
                );
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
            let phase = utf16(&format!("{} · {}", tr(lang, "ui.phase1"), tr(lang, "ui.phase2")));
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
