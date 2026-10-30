//! Procedural guest framebuffer stub: a deterministic RGBA pattern
//! written by a compute shader (mirrors what the guest's virtio-gpu
//! blob will contain in later phases). The CPU twin `pattern_image`
//! reproduces it bit-exactly for verification.

use windows::Win32::Graphics::Direct3D12::*;

use super::buffers::barrier;
use super::device::Dx12;
use super::shader;
use super::{hr, GResult};

/// CPU twin of `shader::PATTERN_SRC` — identical integer math.
pub fn pattern_pixel(x: u32, y: u32, w: u32, h: u32) -> [u8; 4] {
    let r = ((x * 255) / (w.saturating_sub(1)).max(1)) as u8;
    let g = ((y * 255) / (h.saturating_sub(1)).max(1)) as u8;
    let b = ((((x / 32) + (y / 32)) & 1) * 255) as u8;
    let m = (((x + y) & 1) * 48) as u8;
    [r.saturating_add(m), g.saturating_add(m), b, 255]
}

/// Reference RGBA image (row-major, top-left origin — same as HLSL).
pub fn pattern_image(w: u32, h: u32) -> Vec<u8> {
    let mut img = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let px = pattern_pixel(x, y, w, h);
            let o = ((y * w + x) * 4) as usize;
            img[o..o + 4].copy_from_slice(&px);
        }
    }
    img
}

/// Compute pass that writes the pattern into a UAV texture.
pub struct PatternPass {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    heap: ID3D12DescriptorHeap,
}

impl PatternPass {
    pub fn new(dx: &Dx12) -> GResult<Self> {
        unsafe {
            let blob = shader::compile(shader::PATTERN_SRC, shader::PATTERN_ENTRY, "cs_5_1\0")?;
            let root = dx
                .device
                .CreateRootSignature::<ID3D12RootSignature>(0, &blob)
                .map_err(|e| hr("CreateRootSignature(pattern)", e))?;
            let pso = dx
                .device
                .CreateComputePipelineState::<ID3D12PipelineState>(
                    &D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::ManuallyDrop::new(Some(root.clone())),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: blob.as_ptr() as *const _,
                            BytecodeLength: blob.len(),
                        },
                        ..Default::default()
                    },
                )
                .map_err(|e| hr("CreateComputePipelineState(pattern)", e))?;
            let heap = dx
                .device
                .CreateDescriptorHeap::<ID3D12DescriptorHeap>(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 2,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .map_err(|e| hr("CreateDescriptorHeap(pattern)", e))?;
            Ok(PatternPass { root, pso, heap })
        }
    }

    /// Dispatch the pattern write. `dst` must be in UNORDERED_ACCESS state
    /// before and stays there after (caller manages barriers around it).
    pub fn dispatch(
        &self,
        dx: &Dx12,
        list: &ID3D12GraphicsCommandList,
        dst: &ID3D12Resource,
        w: u32,
        h: u32,
    ) -> GResult<()> {
        unsafe {
            let cpu = self.heap.GetCPUDescriptorHandleForHeapStart();
            let gpu = self.heap.GetGPUDescriptorHandleForHeapStart();
            dx.device.CreateUnorderedAccessView(
                dst,
                None::<&ID3D12Resource>,
                Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
                    ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                    ..Default::default()
                }),
                cpu,
            );
            list.SetDescriptorHeaps(&[Some(self.heap.clone())]);
            list.SetComputeRootSignature(&self.root);
            list.SetPipelineState(&self.pso);
            list.SetComputeRootDescriptorTable(0, gpu);
            let consts: [u32; 4] = [w, h, 0, 0];
            list.SetComputeRoot32BitConstants(1, 4, consts.as_ptr() as *const _, 0);
            list.Dispatch(w.div_ceil(8), h.div_ceil(8), 1);
            Ok(())
        }
    }
}

/// Record: transition dst -> UAV, dispatch pattern, back to `after`.
pub fn render_guest_frame(
    dx: &Dx12,
    pass: &PatternPass,
    list: &ID3D12GraphicsCommandList,
    dst: &ID3D12Resource,
    w: u32,
    h: u32,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> GResult<()> {
    unsafe {
        list.ResourceBarrier(&[barrier(dst, before, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)]);
    }
    pass.dispatch(dx, list, dst, w, h)?;
    unsafe {
        list.ResourceBarrier(&[barrier(dst, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, after)]);
    }
    Ok(())
}
