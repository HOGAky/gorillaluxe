//! Host Raw Input (`WM_INPUT`) plumbing — registers the mouse with
//! `RIDEV_INPUTSINK` and extracts relative deltas (spec §3.2).

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::Input::*;

/// Register the mouse for background (`INPUTSINK`) raw input on `hwnd`.
/// HID usage page 0x01, usage 0x02 = generic mouse.
pub fn register_mouse(hwnd: HWND) -> Result<(), String> {
    let dev = RAWINPUTDEVICE {
        usUsagePage: 0x01,
        usUsage: 0x02,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: hwnd,
    };
    unsafe {
        RegisterRawInputDevices(&[dev], std::mem::size_of::<RAWINPUTDEVICE>() as u32)
            .map_err(|e| format!("RegisterRawInputDevices failed: {e}"))
    }
}

/// Extract the relative mouse delta from a `WM_INPUT` `lparam`.
/// Returns `(dx, dy)` counts, or `None` for non-mouse data.
pub fn mouse_delta(lparam: LPARAM) -> Option<(i32, i32)> {
    unsafe {
        let hraw = HRAWINPUT(lparam.0 as *mut _);
        let mut size: u32 = 0;
        let header_sz = std::mem::size_of::<RAWINPUTHEADER>() as u32;
        let r = GetRawInputData(hraw, RID_INPUT, None, &mut size, header_sz);
        if r == u32::MAX || size == 0 || size > 1 << 16 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        let got = GetRawInputData(
            hraw,
            RID_INPUT,
            Some(buf.as_mut_ptr() as *mut _),
            &mut size,
            header_sz,
        );
        if got == u32::MAX || got != size {
            return None;
        }
        let raw = &*(buf.as_ptr() as *const RAWINPUT);
        if raw.header.dwType != RIM_TYPEMOUSE.0 {
            return None;
        }
        let m = raw.data.mouse;
        if m.lLastX == 0 && m.lLastY == 0 {
            return None;
        }
        Some((m.lLastX, m.lLastY))
    }
}
