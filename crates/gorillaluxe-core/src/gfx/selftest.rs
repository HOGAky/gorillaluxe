//! Automated Verification Test 2 (spec §4, PHASE 2):
//!   1. "Guest" writes a procedural RGBA pattern into a SHARED GPU
//!      texture; the "host" imports it via `OpenSharedHandle` — zero
//!      CPU pixel copies (`steady_state_cpu_copy_bytes == 0`), synced
//!      by a cross-device shared fence.
//!   2. HLSL CAS pass at adjustable sharpness.
//!   3. Readback-heap verification: non-black R8G8B8A8_UNORM,
//!      SSIM >= 0.99 vs the Rust CAS reference, Cyrillic atlas
//!      coverage for U+0410..U+044F, RU<->EN hot-swap < 1 ms.
//!   4. Flip-model swapchain present via a hidden window.

use std::time::Instant;

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

use super::buffers::{
    barrier, close_handle, create_shared_handle, create_texture_rgba, open_shared_handle,
    read_texture_rgba,
};
use super::cas::{cas_reference, ssim};
use super::composite::Composite;
use super::device::Dx12;
use super::pattern::pattern_image;
use super::{hr, reset_steady_state_counter, stats, GResult};
use crate::i18n::{set_lang, trc, Lang};
use crate::ui::atlas::{self};
use crate::ui::renderer::UiRenderer;
use crate::ui::scene::{build_frame, HudStats, UiState};

const GW: u32 = 1024;
const GH: u32 = 576;
const DW: u32 = 1920;
const DH: u32 = 1080;
const SHARPNESS: f32 = 0.6;
const SSIM_MIN: f64 = 0.99;
const HOTSWAP_US_MAX: f64 = 1000.0; // < 1 ms

#[derive(Debug)]
pub struct Report {
    pub guest_adapter: String,
    pub host_adapter: String,
    pub is_warp: bool,
    pub shared_import: bool,
    pub shared_fence: bool,
    pub presented: bool,
    pub nonblack: bool,
    pub ssim: f64,
    pub cyr_coverage: u64,
    pub cyr_glyphs: usize,
    pub hotswap_us: f64,
    pub hotswap_changed: bool,
    pub ui_nonblack: bool,
    /// Phase 3: glyph cells must be letter shapes, not solid blocks.
    pub glyph_cells: usize,
    pub glyph_fill_bad: usize,
    /// Offending codepoints (U+ hex) for `glyph_fill_bad`.
    pub glyph_fill_chars: Vec<u16>,
    /// Production: zero ClearType vertical streaks in the multi-size
    /// ANTIALIASED_QUALITY atlas.
    pub glyph_streak_bad: usize,
    pub steady_bytes: u64,
    pub verification_bytes: u64,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.shared_import
            && self.shared_fence
            && self.nonblack
            && self.ssim >= SSIM_MIN
            && self.cyr_glyphs >= 48
            && self.hotswap_us < HOTSWAP_US_MAX
            && self.hotswap_changed
            && self.ui_nonblack
            && self.glyph_cells > 0
            && self.glyph_fill_bad == 0
            && self.glyph_streak_bad == 0
            && self.steady_bytes == 0
    }
}

fn create_shared_fence(dx: &Dx12) -> GResult<ID3D12Fence> {
    unsafe {
        dx.device
            .CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_SHARED)
            .map_err(|e| hr("CreateFence(shared)", e))
    }
}

/// WNDPROC thunk — `DefWindowProcW` is exposed as a Rust-ABI fn.
unsafe extern "system" fn selftest_wndproc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    w: windows::Win32::Foundation::WPARAM,
    l: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, w, l)
}

/// Hidden WS_POPUP window used for the headless swapchain present.
fn hidden_window() -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;

    unsafe {
        let hinst = HINSTANCE::from(GetModuleHandleW(None).ok()?);
        let cls = WNDCLASSW {
            lpfnWndProc: Some(selftest_wndproc),
            hInstance: hinst,
            lpszClassName: windows::core::w!("GLXSelfTest"),
            ..Default::default()
        };
        RegisterClassW(&cls);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            windows::core::w!("GLXSelfTest"),
            windows::core::w!("GLXSelfTest"),
            WS_POPUP,
            0,
            0,
            DW as i32,
            DH as i32,
            None,
            None,
            Some(hinst),
            None,
        )
        .ok()?;
        if hwnd.is_invalid() {
            None
        } else {
            Some(hwnd)
        }
    }
}

fn rtv_heap1(dx: &Dx12) -> GResult<(ID3D12DescriptorHeap, D3D12_CPU_DESCRIPTOR_HANDLE)> {
    unsafe {
        let heap = dx
            .device
            .CreateDescriptorHeap::<ID3D12DescriptorHeap>(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: 1,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                NodeMask: 0,
            })
            .map_err(|e| hr("CreateDescriptorHeap(rtv1)", e))?;
        let h = heap.GetCPUDescriptorHandleForHeapStart();
        Ok((heap, h))
    }
}

pub fn run(allow_software: bool) -> Result<Report, String> {
    reset_steady_state_counter();

    // Two device objects on the adapter = the cross-device sharing
    // topology the VirtIO-GPU blob pipeline will use (guest renderer
    // exports a blob; host compositor imports it).
    let guest = Dx12::new(allow_software)?;
    let host = Dx12::new(allow_software)?;

    // ---- (1) Guest writes pattern into a shared texture ----------------
    let guest_tex = create_texture_rgba(
        &guest,
        GW,
        GH,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_COMMON,
        true,
    )?;
    let guest_fence = create_shared_fence(&guest)?;
    let res_handle = create_shared_handle(&guest, &guest_tex)?;
    let fence_handle = create_shared_handle(&guest, &guest_fence)?;

    let comp_g = Composite::new(&guest)?;
    let (_ga, glist) = guest.cmd()?;
    comp_g.guest_frame(
        &guest,
        &glist,
        &guest_tex,
        GW,
        GH,
        D3D12_RESOURCE_STATE_COMMON,
    )?;
    unsafe {
        glist.Close().map_err(|e| hr("guest list close", e))?;
        guest
            .queue
            .ExecuteCommandLists(&[Some(glist.cast().map_err(|e| hr("cast", e))?)]);
        // Shared-fence signal: host queue will wait on the imported side.
        guest
            .queue
            .Signal(&guest_fence, 1)
            .map_err(|e| hr("guest fence signal", e))?;
    }

    // ---- Host imports (zero-copy) + waits on the shared fence ----------
    let imported: ID3D12Resource = open_shared_handle(&host, res_handle)?;
    let host_fence: ID3D12Fence = open_shared_handle(&host, fence_handle)?;
    close_handle(res_handle);
    close_handle(fence_handle);

    unsafe {
        host.queue
            .Wait(&host_fence, 1)
            .map_err(|e| hr("host fence wait", e))?;
    }

    // ---- (2) CAS pass: imported -> sharpened output --------------------
    let sharpened = create_texture_rgba(
        &host,
        DW,
        DH,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_COMMON,
        false,
    )?;
    let comp_h = Composite::new(&host)?;
    let (_ha, hlist) = host.cmd()?;
    unsafe {
        // The imported resource arrives in COMMON on the host side —
        // promote it for the CAS SRV read.
        hlist.ResourceBarrier(&[barrier(
            &imported,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        )]);
    }
    comp_h.sharpen(
        &host,
        &hlist,
        &imported,
        &sharpened,
        (GW, GH),
        (DW, DH),
        SHARPNESS,
        D3D12_RESOURCE_STATE_COMMON,
    )?;
    host.submit_wait(&hlist)?;

    // ---- (3) Readback verification of the CAS output -------------------
    let frame = read_texture_rgba(
        &host,
        &sharpened,
        DW,
        DH,
        D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
    )?;
    let nonblack = frame
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] != 0 || p[1] != 0 || p[2] != 0)
        .count()
        > 1000;

    let src_img = pattern_image(GW, GH);
    let reference = cas_reference(&src_img, GW, GH, DW, DH, SHARPNESS);
    let ssim_v = ssim(&frame, &reference);

    // ---- UI overlay render into an offscreen render target -------------
    let atlas = atlas::build_ui_atlases()?;
    let ui = UiRenderer::new(&host, &atlas)?;
    let (_rtv_heap, rtv) = rtv_heap1(&host)?;
    let ui_rt = create_texture_rgba(
        &host,
        DW,
        DH,
        D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        D3D12_RESOURCE_STATE_COMMON,
        false,
    )?;
    unsafe {
        host.device.CreateRenderTargetView(&ui_rt, None, rtv);
    }
    let stats_h = HudStats {
        fps: 240,
        frame_ms: 4.2,
        backend: "DX12 Zero-Copy | WARP",
    };
    let mut ui_state = UiState::default();
    let instances = build_frame(DW as f32, DH as f32, &ui_state, &atlas, &stats_h).instances;
    let (_ua, ulist) = host.cmd()?;
    unsafe {
        // `sharpened` is left in COPY_SOURCE by the readback — return it to
        // PIXEL_SHADER_RESOURCE for the blit.
        ulist.ResourceBarrier(&[barrier(
            &sharpened,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        )]);
        ulist.ResourceBarrier(&[barrier(
            &ui_rt,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        )]);
    }
    comp_h.present_frame(&host, &ui, &ulist, &sharpened, &instances, rtv, DW, DH)?;
    unsafe {
        ulist.ResourceBarrier(&[barrier(
            &ui_rt,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
    }
    host.submit_wait(&ulist)?;
    let ui_frame = read_texture_rgba(&host, &ui_rt, DW, DH, D3D12_RESOURCE_STATE_COPY_SOURCE)?;
    let ui_nonblack = ui_frame
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] != 0 || p[1] != 0 || p[2] != 0)
        .count()
        > 1000;

    // ---- Cyrillic glyph coverage (U+0410..U+044F) ----------------------
    let cyr_coverage = atlas::cyrillic_coverage(&atlas);
    let cyr_glyphs = (0x0410u16..=0x044F)
        .filter(|c| atlas.glyphs.get(c).map(|g| g.coverage > 0).unwrap_or(false))
        .count();

    // ---- (3b) Glyph cells must be letter shapes, not solid blocks -----
    let (glyph_cells, glyph_bad) = atlas::glyph_fill_violations(&atlas);
    let glyph_fill_bad = glyph_bad.len();
    // Production QA: no ClearType vertical streaks anywhere in the
    // multi-size ANTIALIASED_QUALITY atlas.
    let glyph_streak_bad = atlas::streak_violations(&atlas).len();

    // ---- (4) RU<->EN hot-swap < 1 ms -----------------------------------
    let t0 = Instant::now();
    set_lang(Lang::Ru);
    let ru = trc("ui.phase2");
    ui_state.lang = Lang::Ru;
    let _ = build_frame(DW as f32, DH as f32, &ui_state, &atlas, &stats_h);
    set_lang(Lang::En);
    let en = trc("ui.phase2");
    let hotswap_us = t0.elapsed().as_secs_f64() * 1e6;
    let hotswap_changed = ru != en;

    // ---- (5) Flip swapchain present via hidden window ------------------
    let presented = hidden_window()
        .map(
            |hwnd| match super::swapchain::SwapChain::new(&host, hwnd, DW, DH) {
                Ok(sc) => {
                    let idx = sc.current_index();
                    let rt = &sc.buffers[idx as usize];
                    let rtv2 = sc.rtv_for(&host, idx);
                    let (_pa, plist) = match host.cmd() {
                        Ok(x) => x,
                        Err(_) => return false,
                    };
                    unsafe {
                        plist.ResourceBarrier(&[barrier(
                            rt,
                            D3D12_RESOURCE_STATE_COMMON,
                            D3D12_RESOURCE_STATE_RENDER_TARGET,
                        )]);
                    }
                    let ok = comp_h
                        .present_frame(&host, &ui, &plist, &sharpened, &instances, rtv2, DW, DH)
                        .is_ok();
                    unsafe {
                        plist.ResourceBarrier(&[barrier(
                            rt,
                            D3D12_RESOURCE_STATE_RENDER_TARGET,
                            D3D12_RESOURCE_STATE_PRESENT,
                        )]);
                    }
                    ok && host.submit_wait(&plist).is_ok() && sc.present().is_ok()
                }
                Err(_) => false,
            },
        )
        .unwrap_or(false);

    let s = stats();
    Ok(Report {
        guest_adapter: guest.adapter_name.clone(),
        host_adapter: host.adapter_name.clone(),
        is_warp: host.is_warp,
        shared_import: true,
        shared_fence: true,
        presented,
        nonblack,
        ssim: ssim_v,
        cyr_coverage,
        cyr_glyphs,
        hotswap_us,
        hotswap_changed,
        ui_nonblack,
        glyph_cells,
        glyph_fill_chars: glyph_bad,
        glyph_fill_bad,
        glyph_streak_bad,
        steady_bytes: s
            .steady_state_cpu_copy_bytes
            .load(std::sync::atomic::Ordering::SeqCst),
        verification_bytes: s
            .verification_copy_bytes
            .load(std::sync::atomic::Ordering::SeqCst),
    })
}
