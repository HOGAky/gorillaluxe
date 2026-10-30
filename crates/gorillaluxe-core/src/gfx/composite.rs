//! The compositor (spec §2.2): guest shared texture -> CAS compute ->
//! fullscreen blit onto a render target -> Luxe Dark UI overlay.
//! Steady state performs zero CPU pixel copies.

use windows::Win32::Graphics::Direct3D12::*;

use super::buffers::barrier;
use super::cas::CasPass;
use super::device::Dx12;
use super::pattern::PatternPass;
use super::GResult;
use crate::ui::renderer::{BlitPass, UiRenderer};
use crate::ui::scene::Instance;

pub struct Composite {
    pub pattern: PatternPass,
    pub cas: CasPass,
    pub blit: BlitPass,
}

impl Composite {
    pub fn new(dx: &Dx12) -> GResult<Self> {
        Ok(Composite {
            pattern: PatternPass::new(dx)?,
            cas: CasPass::new(dx)?,
            blit: BlitPass::new(dx)?,
        })
    }

    /// Record the guest pattern write. `guest` must currently be in
    /// `before`; left in NON_PIXEL_SHADER_RESOURCE for the CAS pass.
    pub fn guest_frame(
        &self,
        dx: &Dx12,
        list: &ID3D12GraphicsCommandList,
        guest: &ID3D12Resource,
        w: u32,
        h: u32,
        before: D3D12_RESOURCE_STATES,
    ) -> GResult<()> {
        super::pattern::render_guest_frame(
            dx,
            &self.pattern,
            list,
            guest,
            w,
            h,
            before,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        )
    }

    /// Record the Luxe Dark ambient backdrop (live GUI): obsidian +
    /// grid + golden glow instead of the selftest checkerboard.
    /// `guest` must currently be in `before`; left in
    /// NON_PIXEL_SHADER_RESOURCE for the CAS pass.
    pub fn ambient_frame(
        &self,
        dx: &Dx12,
        list: &ID3D12GraphicsCommandList,
        guest: &ID3D12Resource,
        w: u32,
        h: u32,
        before: D3D12_RESOURCE_STATES,
    ) -> GResult<()> {
        super::pattern::render_ambient_frame(
            dx,
            &self.pattern,
            list,
            guest,
            w,
            h,
            before,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        )
    }

    /// Record CAS: `guest` (NON_PIXEL_SHADER_RESOURCE) -> `out`
    /// (transitioned COMMON/other -> UNORDERED_ACCESS -> PIXEL_SHADER_RESOURCE).
    /// `out_before` is `out`'s current state.
    #[allow(clippy::too_many_arguments)]
    pub fn sharpen(
        &self,
        dx: &Dx12,
        list: &ID3D12GraphicsCommandList,
        guest: &ID3D12Resource,
        out: &ID3D12Resource,
        src_wh: (u32, u32),
        dst_wh: (u32, u32),
        sharpness: f32,
        out_before: D3D12_RESOURCE_STATES,
    ) -> GResult<()> {
        unsafe {
            list.ResourceBarrier(&[barrier(
                out,
                out_before,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
        }
        self.cas.dispatch(
            dx,
            list,
            guest,
            out,
            src_wh,
            dst_wh,
            sharpness.clamp(0.0, 1.0),
        )?;
        unsafe {
            list.ResourceBarrier(&[barrier(
                out,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            )]);
        }
        Ok(())
    }

    /// Record: blit sharpened frame + UI instances onto `rt`
    /// (must be in RENDER_TARGET state already).
    #[allow(clippy::too_many_arguments)]
    pub fn present_frame(
        &self,
        dx: &Dx12,
        ui: &UiRenderer,
        list: &ID3D12GraphicsCommandList,
        sharpened: &ID3D12Resource,
        instances: &[Instance],
        rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
        w: u32,
        h: u32,
    ) -> GResult<()> {
        self.blit.draw(dx, list, sharpened, rtv, w, h)?;
        ui.draw(dx, list, instances, rtv, w, h)
    }
}
