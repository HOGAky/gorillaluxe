//! DXGI 1.6 flip-model swapchain (spec §2.2.1): FLIP_DISCARD, 2
//! buffers, frame-latency waitable object, tearing support.

use windows::core::{Interface, IUnknown};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

use super::device::Dx12;
use super::{hr, GResult};

pub const BUFFER_COUNT: u32 = 2;

pub struct SwapChain {
    pub chain: IDXGISwapChain3,
    pub rtv_heap: ID3D12DescriptorHeap,
    pub buffers: Vec<ID3D12Resource>,
    pub latency_event: HANDLE,
    pub width: u32,
    pub height: u32,
}

impl SwapChain {
    pub fn new(dx: &Dx12, hwnd: HWND, w: u32, h: u32) -> GResult<Self> {
        unsafe {
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w,
                Height: h,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: BUFFER_COUNT,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
                Flags: (DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT
                    | DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING)
                    .0 as u32,
            };
            let qunknown: IUnknown = dx
                .queue
                .cast()
                .map_err(|e| hr("queue IUnknown cast", e))?;
            let chain1 = dx
                .factory
                .CreateSwapChainForHwnd(&qunknown, hwnd, &desc, None, None::<&IDXGIOutput>)
                .map_err(|e| hr("CreateSwapChainForHwnd", e))?;
            let chain2: IDXGISwapChain2 = chain1
                .cast()
                .map_err(|e| hr("IDXGISwapChain2 cast", e))?;
            chain2
                .SetMaximumFrameLatency(1)
                .map_err(|e| hr("SetMaximumFrameLatency", e))?;
            let latency_event = chain2.GetFrameLatencyWaitableObject();
            if latency_event.is_invalid() {
                return Err("GetFrameLatencyWaitableObject returned null".into());
            }
            let chain3: IDXGISwapChain3 = chain2
                .cast()
                .map_err(|e| hr("IDXGISwapChain3 cast", e))?;

            let rtv_heap = dx
                .device
                .CreateDescriptorHeap::<ID3D12DescriptorHeap>(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: BUFFER_COUNT,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                    NodeMask: 0,
                })
                .map_err(|e| hr("CreateDescriptorHeap(rtv)", e))?;
            let incr = dx
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize;
            let base = rtv_heap.GetCPUDescriptorHandleForHeapStart();
            let mut buffers = Vec::with_capacity(BUFFER_COUNT as usize);
            for i in 0..BUFFER_COUNT {
                let buf = chain3
                    .GetBuffer::<ID3D12Resource>(i)
                    .map_err(|e| hr("GetBuffer", e))?;
                dx.device.CreateRenderTargetView(
                    &buf,
                    None,
                    D3D12_CPU_DESCRIPTOR_HANDLE {
                        ptr: base.ptr + i as usize * incr,
                    },
                );
                buffers.push(buf);
            }

            Ok(SwapChain {
                chain: chain3,
                rtv_heap,
                buffers,
                latency_event,
                width: w,
                height: h,
            })
        }
    }

    pub fn current_index(&self) -> u32 {
        unsafe { self.chain.GetCurrentBackBufferIndex() }
    }

    /// CPU RTV handle for back buffer `i` (heap created with sequential RTVs).
    pub fn rtv_for(&self, dx: &Dx12, i: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        unsafe {
            let base = self.rtv_heap.GetCPUDescriptorHandleForHeapStart();
            let incr = dx
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize;
            D3D12_CPU_DESCRIPTOR_HANDLE { ptr: base.ptr + i as usize * incr }
        }
    }

    /// Present with tearing (no vsync in headless/bench mode).
    pub fn present(&self) -> GResult<()> {
        unsafe {
            self.chain
                .Present(0, DXGI_PRESENT_ALLOW_TEARING)
                .ok()
                .map_err(|e| hr("Present", e))
        }
    }
}

impl Drop for SwapChain {
    fn drop(&mut self) {
        unsafe {
            if !self.latency_event.is_invalid() {
                let _ = CloseHandle(self.latency_event);
            }
        }
    }
}
