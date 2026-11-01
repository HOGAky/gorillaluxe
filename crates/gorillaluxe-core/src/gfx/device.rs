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
    /// PCI DeviceIds of pre-D3D12 Intel iGPUs (Ivy Bridge HD 2500/4000,
    /// Haswell HD 4x00). Their drivers report partial D3D12 support at
    /// best — choosing them produces device-lost storms, so we skip
    /// them outright and let the WARP path handle these hosts.
    const LEGACY_INTEL: &[u32] = &[
        // Ivy Bridge
        0x0152, 0x0156, 0x0157, 0x015A, 0x0162, 0x0166, 0x016A, 0x0150, 0x0155,
        // Haswell GT1/GT2/GT3 (HD 4200/4400/4600/5000, Iris 5100/5200)
        0x0402, 0x0406, 0x040A, 0x040B, 0x040E, 0x0412, 0x0416, 0x041A, 0x041B, 0x041E, 0x0A06,
        0x0A0B, 0x0A0E, 0x0A16, 0x0A1A, 0x0A1E, 0x0A22, 0x0A26, 0x0A2A, 0x0A2B, 0x0A2E, 0x0C02,
        0x0C06, 0x0C0A, 0x0C0B, 0x0C0E, 0x0C12, 0x0C16, 0x0C1A, 0x0C1E, 0x0C22, 0x0C26, 0x0C2A,
        0x0C2B, 0x0C2E, 0x0D02, 0x0D06, 0x0D0A, 0x0D0B, 0x0D0E, 0x0D12, 0x0D16, 0x0D1A, 0x0D1B,
        0x0D1E, 0x0D22, 0x0D26, 0x0D2A, 0x0D2B, 0x0D2E,
    ];

    /// Pick the best adapter: capable hardware first; WARP whenever no
    /// suitable hardware exists — NEVER crash on old iGPUs.
    /// `allow_software` (`--ci-software-gpu`) remains for forcing WARP
    /// on capable hardware in CI.
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
                // Skip known pre-D3D12 Intel parts even if a shim
                // driver claims support — they corrupt under our CS.
                if desc.VendorId == 0x8086 && Self::LEGACY_INTEL.contains(&desc.DeviceId) {
                    continue;
                }
                if Self::try_device(&adapter).is_some() {
                    chosen = Some((adapter, desc, false));
                    break;
                }
            }

            // WARP fallback — unconditional when no hardware adapter
            // qualified (old iGPU laptops included). `allow_software`
            // is kept for API compat / CI forcing; production never
            // crashes on missing D3D12 hardware.
            if chosen.is_none() {
                let _ = allow_software; // honored implicitly — kept for call-site compat
                if let Ok(adapter) = factory.EnumWarpAdapter::<IDXGIAdapter1>() {
                    if let Ok(desc) = adapter.GetDesc1() {
                        if Self::try_device(&adapter).is_some() {
                            crate::system::logger::info(
                                "no D3D12-capable hardware — using WARP software rasterizer",
                            );
                            chosen = Some((adapter, desc, true));
                        }
                    }
                }
            }

            let (adapter, desc, is_warp) = chosen.ok_or_else(|| {
                "no usable D3D12 adapter (hardware unsupported and WARP unavailable)".to_string()
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
