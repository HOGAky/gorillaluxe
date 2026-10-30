//! gorillaluxe.exe — Luxe Dark player shell (Phase 1).
//!
//! Phase 1 delivers the native Win32 window with the Windows 11 dark
//! immersive title bar and rounded corners. The DX12 zero-copy
//! compositor + FidelityFX CAS pass + ImGui overlay land in Phase 2.
#![windows_subsystem = "windows"]

use gorillaluxe_core::config::Config;
use gorillaluxe_core::i18n::{tr, Lang};
use gorillaluxe_core::APP_NAME;
use windows::core::w;
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, EndPaint, FillRect, SetBkMode, SetTextColor, TextOutW,
    UpdateWindow, HDC, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
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

    unsafe { run_app(cfg.lang) };
}

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn run_app(lang: Lang) {
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

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).into() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
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
