//! AMD FidelityFX CAS compute pass (spec §2.2.3) and a bit-close Rust
//! reference used by the SSIM >= 0.99 verification.

use windows::Win32::Graphics::Direct3D12::*;

use super::device::Dx12;
use super::shader;
use super::{hr, GResult};

pub struct CasPass {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    heap: ID3D12DescriptorHeap,
}

impl CasPass {
    pub fn new(dx: &Dx12) -> GResult<Self> {
        unsafe {
            let blob = shader::compile(shader::CAS_SRC, shader::CAS_ENTRY, "cs_5_1\0")?;
            let root = dx
                .device
                .CreateRootSignature::<ID3D12RootSignature>(0, &blob)
                .map_err(|e| hr("CreateRootSignature(cas)", e))?;
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
                .map_err(|e| hr("CreateComputePipelineState(cas)", e))?;
            let heap = dx
                .device
                .CreateDescriptorHeap::<ID3D12DescriptorHeap>(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 2,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .map_err(|e| hr("CreateDescriptorHeap(cas)", e))?;
            Ok(CasPass { root, pso, heap })
        }
    }

    /// Record the CAS dispatch. Caller guarantees: `src` is in
    /// NON_PIXEL_SHADER_RESOURCE, `dst` in UNORDERED_ACCESS.
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch(
        &self,
        dx: &Dx12,
        list: &ID3D12GraphicsCommandList,
        src: &ID3D12Resource,
        dst: &ID3D12Resource,
        src_wh: (u32, u32),
        dst_wh: (u32, u32),
        sharpness: f32,
    ) -> GResult<()> {
        unsafe {
            let incr = dx
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            let cpu0 = self.heap.GetCPUDescriptorHandleForHeapStart();
            let cpu1 = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: cpu0.ptr + incr as usize,
            };
            let gpu = self.heap.GetGPUDescriptorHandleForHeapStart();

            dx.device.CreateShaderResourceView(
                src,
                Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
                    ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2D: D3D12_TEX2D_SRV {
                            MostDetailedMip: 0,
                            MipLevels: 1,
                            PlaneSlice: 0,
                            ResourceMinLODClamp: 0.0,
                        },
                    },
                }),
                cpu0,
            );
            dx.device.CreateUnorderedAccessView(
                dst,
                None::<&ID3D12Resource>,
                Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
                    ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                    ..Default::default()
                }),
                cpu1,
            );

            list.SetDescriptorHeaps(&[Some(self.heap.clone())]);
            list.SetComputeRootSignature(&self.root);
            list.SetPipelineState(&self.pso);
            list.SetComputeRootDescriptorTable(0, gpu);
            let consts: [u32; 8] = [
                src_wh.0,
                src_wh.1,
                dst_wh.0,
                dst_wh.1,
                sharpness.to_bits(),
                0,
                0,
                0,
            ];
            list.SetComputeRoot32BitConstants(1, 8, consts.as_ptr() as *const _, 0);
            list.Dispatch(dst_wh.0.div_ceil(8), dst_wh.1.div_ceil(8), 1);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Rust reference implementation — mirrors CAS_SRC float math for the
// SSIM comparison. Not perf-critical; runs once per verification.
// ---------------------------------------------------------------------------

fn clamp3(v: [f32; 3]) -> [f32; 3] {
    [
        v[0].clamp(0.0, 1.0),
        v[1].clamp(0.0, 1.0),
        v[2].clamp(0.0, 1.0),
    ]
}
fn min3(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> [f32; 3] {
    [
        a[0].min(b[0].min(c[0])),
        a[1].min(b[1].min(c[1])),
        a[2].min(b[2].min(c[2])),
    ]
}
fn max3(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> [f32; 3] {
    [
        a[0].max(b[0].max(c[0])),
        a[1].max(b[1].max(c[1])),
        a[2].max(b[2].max(c[2])),
    ]
}
fn minv(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].min(b[2])]
}
fn maxv(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0].max(b[0]), a[1].max(b[1]), a[2].max(b[2])]
}
fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn add4(a: [f32; 3], b: [f32; 3], c: [f32; 3], d: [f32; 3]) -> [f32; 3] {
    [
        a[0] + b[0] + c[0] + d[0],
        a[1] + b[1] + c[1] + d[1],
        a[2] + b[2] + c[2] + d[2],
    ]
}
fn mul(a: [f32; 3], s: [f32; 3]) -> [f32; 3] {
    [a[0] * s[0], a[1] * s[1], a[2] * s[2]]
}
fn div(a: [f32; 3], s: [f32; 3]) -> [f32; 3] {
    [a[0] / s[0], a[1] / s[1], a[2] / s[2]]
}
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}
fn sat(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

fn src_px(img: &[u8], w: u32, h: u32, x: i32, y: i32) -> [f32; 3] {
    let cx = x.clamp(0, w as i32 - 1) as u32;
    let cy = y.clamp(0, h as i32 - 1) as u32;
    let o = ((cy * w + cx) * 4) as usize;
    [
        img[o] as f32 / 255.0,
        img[o + 1] as f32 / 255.0,
        img[o + 2] as f32 / 255.0,
    ]
}

/// Reference CAS output (RGB8), same math as `CAS_SRC`.
pub fn cas_reference(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32, sharpness: f32) -> Vec<u8> {
    let mut out = vec![0u8; (dw * dh * 4) as usize];
    for oy in 0..dh {
        for ox in 0..dw {
            // Same exact-rational index math as CAS_SRC — integer
            // division, no fp, so GPU and reference always agree on
            // the sampled source texel.
            let icx = ((2 * ox + 1) * sw / (2 * dw)) as i32;
            let icy = ((2 * oy + 1) * sh / (2 * dh)) as i32;
            let e = src_px(src, sw, sh, icx, icy);
            let b = src_px(src, sw, sh, icx, icy - 1);
            let d = src_px(src, sw, sh, icx - 1, icy);
            let f = src_px(src, sw, sh, icx + 1, icy);
            let hh = src_px(src, sw, sh, icx, icy + 1);
            let mn = minv(min3(b, d, f), minv(hh, e));
            let mut mx = maxv(max3(b, d, f), maxv(hh, e));
            for c in &mut mx {
                if *c < 1e-5 {
                    *c = 1e-5;
                }
            }
            let peak = -1.0 / lerp(8.0, 5.0, sharpness);
            let mut wv = [0.0f32; 3];
            for i in 0..3 {
                wv[i] = peak * sat(mn[i].min(2.0 - mx[i]) / mx[i]).sqrt();
            }
            let col = div(
                add(e, mul(add4(b, d, f, hh), wv)),
                [1.0 + 4.0 * wv[0], 1.0 + 4.0 * wv[1], 1.0 + 4.0 * wv[2]],
            );
            let col = clamp3(col);
            let o = ((oy * dw + ox) * 4) as usize;
            out[o] = (col[0] * 255.0).round() as u8;
            out[o + 1] = (col[1] * 255.0).round() as u8;
            out[o + 2] = (col[2] * 255.0).round() as u8;
            out[o + 3] = 255;
        }
    }
    out
}

/// Global SSIM between two same-size RGBA8 frames (luminance, single
/// window — sufficient for an identical-algorithm comparison).
pub fn ssim(a: &[u8], b: &[u8]) -> f64 {
    let n = (a.len() / 4).min(b.len() / 4) as f64;
    if n == 0.0 {
        return 0.0;
    }
    let lum = |img: &[u8], i: usize| -> f64 {
        0.2126 * img[i * 4] as f64 + 0.7152 * img[i * 4 + 1] as f64 + 0.0722 * img[i * 4 + 2] as f64
    };
    let (mut ma, mut mb) = (0.0f64, 0.0f64);
    let cnt = n as usize;
    for i in 0..cnt {
        ma += lum(a, i);
        mb += lum(b, i);
    }
    ma /= n;
    mb /= n;
    let (mut va, mut vb, mut cov) = (0.0f64, 0.0f64, 0.0f64);
    for i in 0..cnt {
        let da = lum(a, i) - ma;
        let db = lum(b, i) - mb;
        va += da * da;
        vb += db * db;
        cov += da * db;
    }
    va /= n;
    vb /= n;
    cov /= n;
    let c1 = 6.5025f64; // (0.01*255)^2
    let c2 = 58.5225f64; // (0.03*255)^2
    ((2.0 * ma * mb + c1) * (2.0 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx::pattern::pattern_image;

    #[test]
    fn ssim_identical_is_one() {
        let img = pattern_image(64, 36);
        assert!((ssim(&img, &img) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cas_reference_deterministic() {
        let src = pattern_image(64, 36);
        let a = cas_reference(&src, 64, 36, 128, 72, 0.6);
        let b = cas_reference(&src, 64, 36, 128, 72, 0.6);
        assert_eq!(a, b);
        // Non-black output.
        assert!(a.chunks(4).filter(|p| p[..3] != [0, 0, 0]).count() > 100);
    }

    /// Spec-mandated regression: the guest texture and the window are
    /// almost NEVER the same size (portrait `720x1280` guest into a
    /// `1280x720` landscape window, etc.) — the CAS dispatch maps
    /// `dtid`→`src` proportionally, so every output quadrant must get
    /// real pixels, not a cropped top-left corner.
    #[test]
    fn cas_scales_mismatched_dims() {
        // Portrait guest buffer -> landscape window.
        let src = pattern_image(72, 128);
        let out = cas_reference(&src, 72, 128, 128, 72, 0.6);
        let lit = |qx: std::ops::Range<u32>, qy: std::ops::Range<u32>| -> usize {
            qy.clone()
                .flat_map(|y| qx.clone().map(move |x| (y * 128 + x) as usize * 4))
                .filter(|&o| out[o] as u16 + out[o + 1] as u16 + out[o + 2] as u16 > 16)
                .count()
        };
        // All four quadrants of the scaled output carry image data.
        assert!(lit(0..64, 0..36) > 64, "top-left quadrant black");
        assert!(lit(64..128, 0..36) > 64, "top-right quadrant black");
        assert!(lit(0..64, 36..72) > 64, "bottom-left quadrant black");
        assert!(lit(64..128, 36..72) > 64, "bottom-right quadrant black");
        // And the reverse orientation: landscape guest -> portrait window.
        let src2 = pattern_image(128, 72);
        let out2 = cas_reference(&src2, 128, 72, 72, 128, 0.6);
        let lit2 = |y: u32| -> usize {
            (0..72u32)
                .map(|x| (y * 72 + x) as usize * 4)
                .filter(|&o| out2[o] as u16 + out2[o + 1] as u16 + out2[o + 2] as u16 > 16)
                .count()
        };
        assert!(lit2(10) > 32 && lit2(118) > 32, "vertical bands black");
    }
}
