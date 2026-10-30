//! gorillaluxe.exe — Luxe Dark player shell (Phase 1).
//!
//! Phase 1 delivers the native Win32 window with the Windows 11 dark
//! immersive title bar and rounded corners. The DX12 zero-copy
//! compositor + FidelityFX CAS pass + ImGui overlay land in Phase 2.
#![windows_subsystem = "windows"]

use gorillaluxe_core::config::Config;
use gorillaluxe_core::gfx::buffers::{barrier, create_texture_rgba};
use gorillaluxe_core::gfx::composite::Composite;
use gorillaluxe_core::gfx::device::Dx12;
use gorillaluxe_core::gfx::swapchain::SwapChain;
use gorillaluxe_core::i18n::{current_lang, set_lang, tr, Lang};
use gorillaluxe_core::ui::atlas;
use gorillaluxe_core::ui::renderer::UiRenderer;
use gorillaluxe_core::ui::scene::{build_scene, HudStats};
use gorillaluxe_core::ui::theme::FONT_UI;
use gorillaluxe_core::APP_NAME;
use windows::core::w;
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
    D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
    D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_RENDER_TARGET,
};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, EndPaint, FillRect, InvalidateRect, SetBkMode, SetTextColor,
    TextOutW, UpdateWindow, HDC, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_F4};
use windows::Win32::UI::WindowsAndMessaging::*;

// Luxe Dark palette.
const LUXE_BG: u32 = 0x00141210; // COLORREF 0x00BBGGRR -> RGB(0x10,0x12,0x14)
const LUXE_ACCENT: COLORREF = COLORREF(0x0027A2C9); // gold RGB(0xC9,0xA2,0x27)
const LUXE_TEXT: COLORREF = COLORREF(0x00E8E4DC);   // warm white
const LUXE_DIM: COLORREF = COLORREF(0x00908C85);    // muted grey

// DWM attributes (Windows 11 21H2+).
const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
const DWMWCP_ROUND: u32 = 2;

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
            // Accepted per spec; honored by the Phase 2 DXGI adapter pick.
            "--ci-software-gpu" => cfg.ci_software_gpu = true,
            _ => {}
        }
    }

    unsafe { run_app(cfg.lang, cfg.ci_software_gpu, cfg.cas_sharpness) };
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn run_app(lang: Lang, allow_warp: bool, sharpness: f32) {
    set_lang(lang);
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

    // Phase 1 stashes the language in the window via GWLP_USERDATA.
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
        Some(Box::into_raw(Box::new(lang)) as *const _),
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

    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = UpdateWindow(hwnd);

    // Phase 2: DX12 zero-copy compositor (pattern -> CAS -> blit -> UI).
    // Falls back to the GDI Luxe Dark shell when no usable adapter exists.
    if let Err(e) = run_dx12(hwnd, allow_warp, sharpness) {
        eprintln!("DX12 compositor unavailable ({e}); falling back to GDI shell");
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

const WIN_W: u32 = 1280;
const WIN_H: u32 = 720;
const GUEST_W: u32 = 1024;
const GUEST_H: u32 = 576;

fn run_dx12(hwnd: HWND, allow_warp: bool, sharpness: f32) -> Result<(), String> {
    let dx = Dx12::new(allow_warp)?;
    let swap = SwapChain::new(&dx, hwnd, WIN_W, WIN_H)?;
    let comp = Composite::new(&dx)?;
    let atlas = atlas::build(FONT_UI)?;
    let ui = UiRenderer::new(&dx, &atlas)?;

    // Guest framebuffer analog: shared-flagged UAV texture.
    let guest = create_texture_rgba(
        &dx,
        GUEST_W,
        GUEST_H,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_COMMON,
        true,
    )?;
    // CAS output -> blit source.
    let sharpened = create_texture_rgba(
        &dx,
        WIN_W,
        WIN_H,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_COMMON,
        false,
    )?;

    let mut guest_state = D3D12_RESOURCE_STATE_COMMON;
    let mut out_state = D3D12_RESOURCE_STATE_COMMON;
    let mut frames = 0u64;
    let mut last = std::time::Instant::now();
    let mut fps = 0u32;

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

            // Frame pacing via the swapchain latency waitable object.
            let _ = WaitForSingleObject(swap.latency_event, 500);

            let (_alloc, list) = dx.cmd()?;
            comp.guest_frame(&dx, &list, &guest, GUEST_W, GUEST_H, guest_state)?;
            guest_state = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
            comp.sharpen(
                &dx,
                &list,
                &guest,
                &sharpened,
                (GUEST_W, GUEST_H),
                (WIN_W, WIN_H),
                sharpness,
                out_state,
            )?;
            out_state = windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE;

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
            let insts = build_scene(
                WIN_W as f32,
                WIN_H as f32,
                current_lang(),
                &atlas,
                &stats,
            );
            comp.present_frame(
                &dx,
                &ui,
                &list,
                &sharpened,
                &insts,
                swap.rtv_for(&dx, idx),
                WIN_W,
                WIN_H,
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
    Ok(())
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            // Recover the language pointer passed via CreateWindowExW.
            let cs = lparam.0 as *const CREATESTRUCTW;
            let lang_ptr = (*cs).lpCreateParams as *mut Lang;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, lang_ptr as isize);
            LRESULT(0)
        }
        WM_DESTROY => {
            // Free the stashed Lang box.
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Lang;
            if !p.is_null() {
                drop(Box::from_raw(p));
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            PostQuitMessage(0);
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
        WM_KEYDOWN => {
            // F4 toggles RU<->EN instantly (spec §3.5 pill toggle);
            // ESC quits. DX12 path rebuilds the scene next frame —
            // no restart needed.
            if wparam.0 as u32 == VK_F4.0 as u32 {
                let next = if current_lang() == Lang::Ru { Lang::En } else { Lang::Ru };
                set_lang(next);
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Lang;
                if !p.is_null() {
                    *p = next;
                }
                let _ = InvalidateRect(Some(hwnd), None, true);
            } else if wparam.0 as u32 == VK_ESCAPE.0 as u32 {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            // Paint Luxe Dark background ourselves.
            let hdc = HDC(wparam.0 as *mut _);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            FillRect(hdc, &rc, CreateSolidBrush(COLORREF(LUXE_BG)));
            LRESULT(1)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}
