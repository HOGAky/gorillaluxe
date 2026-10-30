//! D3D12 device, command queue and fence — hardware adapters first,
//! WARP (`DXGI_ADAPTER_FLAG_SOFTWARE`) only when `--ci-software-gpu`
//! allows it (spec §1).

use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::{hr, GResult};

pub struct Dx12 {
    pub factory: IDXGIFactory6,
    pub device: ID3D12Device,
    pub queue: ID3D12CommandQueue,
    pub adapter_name: String,
    pub is_warp: bool,
    fence: ID3D12Fence,
    fence_value: std::cell::Cell<u64>,
    fence_event: HANDLE,
}

impl Dx12 {
    /// Pick the best adapter: hardware first; WARP only when
    /// `allow_software` (`--ci-software-gpu`) is set.
    pub fn new(allow_software: bool) -> GResult<Dx12> {
        unsafe {
            let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))
                .map_err(|e| hr("CreateDXGIFactory2", e))?;

            let mut chosen: Option<(IDXGIAdapter1, DXGI_ADAPTER_DESC1, bool)> = None;

            // Hardware adapters first (always preferred).
            for i in 0..32u32 {
                let Ok(adapter) = factory.EnumAdapters1(i) else {
                    break;
                };
                let Ok(desc) = adapter.GetDesc1() else {
                    continue;
                };
                if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
                    continue;
                }
                if Self::try_device(&adapter).is_some() {
                    chosen = Some((adapter, desc, false));
                    break;
                }
            }

            // WARP fallback — only when the flag allows software adapters.
            if chosen.is_none() && allow_software {
                if let Ok(adapter) = factory.EnumWarpAdapter::<IDXGIAdapter1>() {
                    if let Ok(desc) = adapter.GetDesc1() {
                        if Self::try_device(&adapter).is_some() {
                            chosen = Some((adapter, desc, true));
                        }
                    }
                }
            }

            let (adapter, desc, is_warp) = chosen.ok_or_else(|| {
                "no usable D3D12 adapter (need HW GPU or --ci-software-gpu)".to_string()
            })?;
            let device = Self::try_device(&adapter)
                .ok_or_else(|| "D3D12CreateDevice failed on chosen adapter".to_string())?;

            let adapter_name = {
                let end = desc
                    .Description
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(desc.Description.len());
                String::from_utf16_lossy(&desc.Description[..end])
            };

            let queue = device
                .CreateCommandQueue::<ID3D12CommandQueue>(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .map_err(|e| hr("CreateCommandQueue", e))?;

            let fence = device
                .CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE)
                .map_err(|e| hr("CreateFence", e))?;
            let fence_event =
                CreateEventW(None, false, false, None).map_err(|e| hr("CreateEventW", e))?;

            Ok(Dx12 {
                factory,
                device,
                queue,
                adapter_name,
                is_warp,
                fence,
                fence_value: std::cell::Cell::new(0),
                fence_event,
            })
        }
    }

    fn try_device(adapter: &IDXGIAdapter1) -> Option<ID3D12Device> {
        let mut dev: Option<ID3D12Device> = None;
        unsafe {
            D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_11_0, &mut dev).ok()?;
        }
        dev
    }

    /// Fresh direct command allocator + list (single-shot submission model
    /// is fine for the compositor's workload).
    pub fn cmd(&self) -> GResult<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)> {
        unsafe {
            let alloc = self
                .device
                .CreateCommandAllocator::<ID3D12CommandAllocator>(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .map_err(|e| hr("CreateCommandAllocator", e))?;
            let list: ID3D12GraphicsCommandList = self
                .device
                .CreateCommandList(
                    0,
                    D3D12_COMMAND_LIST_TYPE_DIRECT,
                    &alloc,
                    None::<&ID3D12PipelineState>,
                )
                .map_err(|e| hr("CreateCommandList", e))?;
            Ok((alloc, list))
        }
    }

    /// Close, execute, and block until the GPU signals completion.
    pub fn submit_wait(&self, list: &ID3D12GraphicsCommandList) -> GResult<()> {
        unsafe {
            list.Close().map_err(|e| hr("CommandList::Close", e))?;
            self.queue
                .ExecuteCommandLists(&[Some(list.cast().map_err(|e| hr("cast", e))?)]);
            let v = self.fence_value.get() + 1;
            self.fence_value.set(v);
            self.queue
                .Signal(&self.fence, v)
                .map_err(|e| hr("Queue::Signal", e))?;
            if self.fence.GetCompletedValue() < v {
                self.fence
                    .SetEventOnCompletion(v, self.fence_event)
                    .map_err(|e| hr("SetEventOnCompletion", e))?;
                WaitForSingleObject(self.fence_event, 30_000);
            }
            Ok(())
        }
    }

    /// Block until all submitted queue work completes (used before
    /// `IDXGISwapChain::ResizeBuffers` — back buffers must be idle).
    pub fn wait_idle(&self) -> GResult<()> {
        unsafe {
            let v = self.fence_value.get() + 1;
            self.fence_value.set(v);
            self.queue
                .Signal(&self.fence, v)
                .map_err(|e| hr("Queue::Signal", e))?;
            if self.fence.GetCompletedValue() < v {
                self.fence
                    .SetEventOnCompletion(v, self.fence_event)
                    .map_err(|e| hr("SetEventOnCompletion", e))?;
                WaitForSingleObject(self.fence_event, 30_000);
            }
            Ok(())
        }
    }
}

impl Drop for Dx12 {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.fence_event);
        }
    }
}
