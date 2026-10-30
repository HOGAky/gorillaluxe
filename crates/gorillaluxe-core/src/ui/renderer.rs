//! DX12 UI quad renderer (SDF rounded rects + R8 atlas glyphs) and a
//! fullscreen blit pass used to composite CAS output onto the back buffer.

use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::gfx::buffers::{
    create_resource, create_upload_buffer, tex_desc, upload_texture, write_upload,
};
use crate::gfx::device::Dx12;
use crate::gfx::shader;
use crate::gfx::{hr, GResult};

use super::atlas::FontAtlas;
use super::scene::Instance;

const INST_CAP: usize = 2048;
const INST_STRIDE: u32 = 64;

fn blend_desc() -> D3D12_BLEND_DESC {
    let rt = D3D12_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        LogicOpEnable: false.into(),
        SrcBlend: D3D12_BLEND_ONE,
        DestBlend: D3D12_BLEND_INV_SRC_ALPHA,
        BlendOp: D3D12_BLEND_OP_ADD,
        SrcBlendAlpha: D3D12_BLEND_ONE,
        DestBlendAlpha: D3D12_BLEND_ZERO,
        BlendOpAlpha: D3D12_BLEND_OP_ADD,
        LogicOp: D3D12_LOGIC_OP_NOOP,
        RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    let mut d = D3D12_BLEND_DESC {
        AlphaToCoverageEnable: false.into(),
        IndependentBlendEnable: false.into(),
        ..Default::default()
    };
    d.RenderTarget = [rt; 8];
    d
}

fn raster_desc() -> D3D12_RASTERIZER_DESC {
    D3D12_RASTERIZER_DESC {
        FillMode: D3D12_FILL_MODE_SOLID,
        CullMode: D3D12_CULL_MODE_NONE,
        FrontCounterClockwise: false.into(),
        DepthBias: 0,
        DepthBiasClamp: 0.0,
        SlopeScaledDepthBias: 0.0,
        DepthClipEnable: true.into(),
        MultisampleEnable: false.into(),
        AntialiasedLineEnable: false.into(),
        ForcedSampleCount: 0,
        ConservativeRaster: D3D12_CONSERVATIVE_RASTERIZATION_MODE_OFF,
    }
}

fn depth_stencil_off() -> D3D12_DEPTH_STENCIL_DESC {
    D3D12_DEPTH_STENCIL_DESC {
        DepthEnable: false.into(),
        DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ZERO,
        DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
        StencilEnable: false.into(),
        ..Default::default()
    }
}

/// Fullscreen-triangle blit: linear SRV sample -> RTV.
pub struct BlitPass {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    srv_heap: ID3D12DescriptorHeap,
}

impl BlitPass {
    pub fn new(dx: &Dx12) -> GResult<Self> {
        unsafe {
            let vs = shader::compile(shader::BLIT_VS_SRC, shader::UI_VS_ENTRY, "vs_5_1\0")?;
            let ps = shader::compile(shader::BLIT_PS_SRC, shader::UI_PS_ENTRY, "ps_5_1\0")?;
            let root = dx
                .device
                .CreateRootSignature::<ID3D12RootSignature>(0, &ps)
                .map_err(|e| hr("CreateRootSignature(blit)", e))?;
            let mut rtv_fmt = [DXGI_FORMAT_UNKNOWN; 8];
            rtv_fmt[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
            let pso = dx
                .device
                .CreateGraphicsPipelineState::<ID3D12PipelineState>(
                    &D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::ManuallyDrop::new(Some(root.clone())),
                        VS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: vs.as_ptr() as *const _,
                            BytecodeLength: vs.len(),
                        },
                        PS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: ps.as_ptr() as *const _,
                            BytecodeLength: ps.len(),
                        },
                        BlendState: blend_desc(),
                        SampleMask: u32::MAX,
                        RasterizerState: raster_desc(),
                        DepthStencilState: depth_stencil_off(),
                        PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                        NumRenderTargets: 1,
                        RTVFormats: rtv_fmt,
                        DSVFormat: DXGI_FORMAT_UNKNOWN,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        ..Default::default()
                    },
                )
                .map_err(|e| hr("CreateGraphicsPipelineState(blit)", e))?;
            let srv_heap = dx
                .device
                .CreateDescriptorHeap::<ID3D12DescriptorHeap>(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .map_err(|e| hr("CreateDescriptorHeap(blit)", e))?;
            Ok(BlitPass { root, pso, srv_heap })
        }
    }

    /// Record: draw `src` (any `*_SHADER_RESOURCE` state) onto `rtv`.
    /// Caller owns the state transitions of `src`/`rt`.
    pub fn draw(
        &self,
        dx: &Dx12,
        list: &ID3D12GraphicsCommandList,
        src: &ID3D12Resource,
        rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
        w: u32,
        h: u32,
    ) -> GResult<()> {
        unsafe {
            let cpu = self.srv_heap.GetCPUDescriptorHandleForHeapStart();
            dx.device.CreateShaderResourceView(
                src,
                Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
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
                cpu,
            );
            let gpu = self.srv_heap.GetGPUDescriptorHandleForHeapStart();
            list.SetDescriptorHeaps(&[Some(self.srv_heap.clone())]);
            list.SetGraphicsRootSignature(&self.root);
            list.SetPipelineState(&self.pso);
            list.RSSetViewports(&[D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: w as f32,
                Height: h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]);
            list.RSSetScissorRects(&[windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: w as i32,
                bottom: h as i32,
            }]);
            list.OMSetRenderTargets(1, Some(&rtv), false.into(), None);
            list.SetGraphicsRootDescriptorTable(0, gpu);
            list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            list.DrawInstanced(3, 1, 0, 0);
            Ok(())
        }
    }
}

/// SDF/rounded-rect + glyph quad renderer for the Luxe Dark overlay.
pub struct UiRenderer {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    srv_heap: ID3D12DescriptorHeap,
    inst_buf: ID3D12Resource,
    _atlas_tex: ID3D12Resource,
}

impl UiRenderer {
    pub fn new(dx: &Dx12, atlas: &FontAtlas) -> GResult<Self> {
        unsafe {
            let vs = shader::compile(shader::UI_VS_SRC, shader::UI_VS_ENTRY, "vs_5_1\0")?;
            let ps = shader::compile(shader::UI_PS_SRC, shader::UI_PS_ENTRY, "ps_5_1\0")?;
            let root = dx
                .device
                .CreateRootSignature::<ID3D12RootSignature>(0, &ps)
                .map_err(|e| hr("CreateRootSignature(ui)", e))?;
            let mut rtv_fmt = [DXGI_FORMAT_UNKNOWN; 8];
            rtv_fmt[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
            let pso = dx
                .device
                .CreateGraphicsPipelineState::<ID3D12PipelineState>(
                    &D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::ManuallyDrop::new(Some(root.clone())),
                        VS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: vs.as_ptr() as *const _,
                            BytecodeLength: vs.len(),
                        },
                        PS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: ps.as_ptr() as *const _,
                            BytecodeLength: ps.len(),
                        },
                        BlendState: blend_desc(),
                        SampleMask: u32::MAX,
                        RasterizerState: raster_desc(),
                        DepthStencilState: depth_stencil_off(),
                        PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                        NumRenderTargets: 1,
                        RTVFormats: rtv_fmt,
                        DSVFormat: DXGI_FORMAT_UNKNOWN,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        ..Default::default()
                    },
                )
                .map_err(|e| hr("CreateGraphicsPipelineState(ui)", e))?;

            // Instance buffer: persistent UPLOAD-heap buffer read as
            // a StructuredBuffer SRV (CPU writes per frame, GPU reads).
            let inst_buf = create_upload_buffer(dx, (INST_CAP * INST_STRIDE as usize) as u64)?;

            // Atlas: R8_UNORM texture uploaded once via a staging copy
            // (upload_texture leaves it in PIXEL_SHADER_RESOURCE state).
            let atlas_tex = create_resource(
                dx,
                D3D12_HEAP_TYPE_DEFAULT,
                D3D12_HEAP_FLAG_NONE,
                &tex_desc(
                    atlas.width,
                    atlas.height,
                    DXGI_FORMAT_R8_UNORM,
                    D3D12_RESOURCE_FLAG_NONE,
                ),
                D3D12_RESOURCE_STATE_COMMON,
            )?;
            upload_texture(
                dx,
                &atlas_tex,
                atlas.width,
                atlas.height,
                DXGI_FORMAT_R8_UNORM,
                &atlas.pixels,
                1,
            )?;

            let srv_heap = dx
                .device
                .CreateDescriptorHeap::<ID3D12DescriptorHeap>(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 2,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .map_err(|e| hr("CreateDescriptorHeap(ui)", e))?;
            // Static atlas SRV at slot 1 (written once, inst SRV at slot 0 per draw).
            let incr = dx
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            let base = srv_heap.GetCPUDescriptorHandleForHeapStart();
            dx.device.CreateShaderResourceView(
                &atlas_tex,
                Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_R8_UNORM,
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
                D3D12_CPU_DESCRIPTOR_HANDLE { ptr: base.ptr + incr },
            );

            Ok(UiRenderer { root, pso, srv_heap, inst_buf, _atlas_tex: atlas_tex })
        }
    }

    /// Record UI draws: writes instances into the upload buffer and
    /// draws them onto `rtv`. Caller handles render-target barriers.
    pub fn draw(
        &self,
        dx: &Dx12,
        list: &ID3D12GraphicsCommandList,
        instances: &[Instance],
        rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
        w: u32,
        h: u32,
    ) -> GResult<()> {
        if instances.len() > INST_CAP {
            return Err(format!("too many UI instances: {} > {}", instances.len(), INST_CAP));
        }
        unsafe {
            write_upload(
                &self.inst_buf,
                std::slice::from_raw_parts(
                    instances.as_ptr() as *const u8,
                    instances.len() * INST_STRIDE as usize,
                ),
            )?;

            // Instance StructuredBuffer SRV at slot 0.
            let cpu0 = self.srv_heap.GetCPUDescriptorHandleForHeapStart();
            dx.device.CreateShaderResourceView(
                &self.inst_buf,
                Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_UNKNOWN,
                    ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_SRV {
                            FirstElement: 0,
                            NumElements: instances.len() as u32,
                            StructureByteStride: INST_STRIDE,
                            Flags: D3D12_BUFFER_SRV_FLAG_NONE,
                        },
                    },
                }),
                cpu0,
            );
            let gpu0 = self.srv_heap.GetGPUDescriptorHandleForHeapStart();

            list.SetDescriptorHeaps(&[Some(self.srv_heap.clone())]);
            list.SetGraphicsRootSignature(&self.root);
            list.SetPipelineState(&self.pso);
            list.RSSetViewports(&[D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: w as f32,
                Height: h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]);
            list.RSSetScissorRects(&[windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: w as i32,
                bottom: h as i32,
            }]);
            list.OMSetRenderTargets(1, Some(&rtv), false.into(), None);
            let scr: [f32; 2] = [w as f32, h as f32];
            list.SetGraphicsRoot32BitConstants(1, 2, scr.as_ptr() as *const _, 0);
            list.SetGraphicsRootDescriptorTable(0, gpu0);
            list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            list.DrawInstanced(6, instances.len() as u32, 0, 0);
            Ok(())
        }
    }
}
