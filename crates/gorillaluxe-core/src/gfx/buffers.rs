//! D3D12 resource helpers: textures, buffers, barriers, shared NT
//! handles (zero-copy import), upload and readback heaps.

use std::ffi::c_void;
use std::mem::ManuallyDrop;

use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::device::Dx12;
use super::{hr, GResult};

/// Copy raw bytes into an UPLOAD-heap buffer (already sized).
/// Used for instance/constant data fed to the GPU each frame.
pub fn write_upload(buf: &ID3D12Resource, data: &[u8]) -> GResult<()> {
    unsafe {
        let mut ptr = std::ptr::null_mut();
        buf.Map(0, None, Some(&mut ptr))
            .map_err(|e| hr("Map(upload)", e))?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, data.len());
        buf.Unmap(0, None);
        Ok(())
    }
}

/// UPLOAD-heap committed buffer (CPU-visible, GPU-readable).
pub fn create_upload_buffer(dx: &Dx12, size: u64) -> GResult<ID3D12Resource> {
    create_resource(
        dx,
        D3D12_HEAP_TYPE_UPLOAD,
        D3D12_HEAP_FLAG_NONE,
        &buffer_desc(size.max(1)),
        D3D12_RESOURCE_STATE_GENERIC_READ,
    )
}

fn heap_props(ty: D3D12_HEAP_TYPE) -> D3D12_HEAP_PROPERTIES {
    D3D12_HEAP_PROPERTIES {
        Type: ty,
        ..Default::default()
    }
}

pub fn tex_desc(
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
    flags: D3D12_RESOURCE_FLAGS,
) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Alignment: 0,
        Width: w as u64,
        Height: h,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: flags,
    }
}

pub fn buffer_desc(size: u64) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: 0,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_NONE,
    }
}

/// Committed resource on the given heap.
pub fn create_resource(
    dx: &Dx12,
    heap: D3D12_HEAP_TYPE,
    heap_flags: D3D12_HEAP_FLAGS,
    desc: &D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> GResult<ID3D12Resource> {
    unsafe {
        let mut res: Option<ID3D12Resource> = None;
        dx.device
            .CreateCommittedResource(&heap_props(heap), heap_flags, desc, state, None, &mut res)
            .map_err(|e| hr("CreateCommittedResource", e))?;
        res.ok_or_else(|| "CreateCommittedResource returned null".into())
    }
}

/// RGBA8 render texture on the DEFAULT heap.
pub fn create_texture_rgba(
    dx: &Dx12,
    w: u32,
    h: u32,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
    shared: bool,
) -> GResult<ID3D12Resource> {
    let heap_flags = if shared {
        D3D12_HEAP_FLAG_SHARED
    } else {
        D3D12_HEAP_FLAG_NONE
    };
    create_resource(
        dx,
        D3D12_HEAP_TYPE_DEFAULT,
        heap_flags,
        &tex_desc(w, h, DXGI_FORMAT_R8G8B8A8_UNORM, flags),
        state,
    )
}

/// Transition barrier helper.
pub fn barrier(
    res: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAGS(0),
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(res.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// Export a device child (resource or fence) as a shareable NT handle.
/// Mirrors spec §2.1 step 3 (the guest blob export / shared fence).
pub fn create_shared_handle<T>(dx: &Dx12, obj: &T) -> GResult<HANDLE>
where
    T: windows::core::Interface + windows::core::imp::CanInto<ID3D12DeviceChild>,
{
    unsafe {
        dx.device
            .CreateSharedHandle::<&T, windows::core::PCWSTR>(
                obj,
                None,
                GENERIC_ALL.0,
                windows::core::PCWSTR::null(),
            )
            .map_err(|e| hr("CreateSharedHandle", e))
    }
}

/// Zero-copy import: open a shared NT handle as a device object.
/// No pixel data crosses the CPU (spec §2.1 step 4).
pub fn open_shared_handle<T: windows::core::Interface>(dx: &Dx12, handle: HANDLE) -> GResult<T> {
    unsafe {
        let mut res: Option<T> = None;
        dx.device
            .OpenSharedHandle(handle, &mut res)
            .map_err(|e| hr("OpenSharedHandle", e))?;
        res.ok_or_else(|| "OpenSharedHandle returned null".into())
    }
}

pub fn close_handle(h: HANDLE) {
    unsafe {
        let _ = CloseHandle(h);
    }
}

/// One-shot upload of `data` into `dst` (DEFAULT heap texture/buffer).
/// Used for one-time data (font atlas) — never on the frame hot path.
pub fn upload_texture(
    dx: &Dx12,
    dst: &ID3D12Resource,
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
    data: &[u8],
    bytes_per_pixel: u32,
) -> GResult<()> {
    unsafe {
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut num_rows = 0u32;
        let mut row_size = 0u64;
        let mut total = 0u64;
        let desc = tex_desc(w, h, format, D3D12_RESOURCE_FLAG_NONE);
        dx.device.GetCopyableFootprints(
            &desc,
            0,
            1,
            0,
            Some(&mut footprint),
            Some(&mut num_rows),
            Some(&mut row_size),
            Some(&mut total),
        );

        let upload = create_resource(
            dx,
            D3D12_HEAP_TYPE_UPLOAD,
            D3D12_HEAP_FLAG_NONE,
            &buffer_desc(total.max(1)),
            D3D12_RESOURCE_STATE_GENERIC_READ,
        )?;

        let mut ptr: *mut c_void = std::ptr::null_mut();
        upload
            .Map(0, None, Some(&mut ptr))
            .map_err(|e| hr("Map(upload)", e))?;
        let src_pitch = (w * bytes_per_pixel) as usize;
        let dst_pitch = footprint.Footprint.RowPitch as usize;
        for row in 0..h as usize {
            std::ptr::copy_nonoverlapping(
                data.as_ptr().add(row * src_pitch),
                (ptr as *mut u8).add(row * dst_pitch),
                src_pitch.min(row_size as usize),
            );
        }
        upload.Unmap(0, None);

        let (_alloc, list) = dx.cmd()?;
        let dst_loc = D3D12_TEXTURE_COPY_LOCATION {
            pResource: ManuallyDrop::new(Some(dst.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        let src_loc = D3D12_TEXTURE_COPY_LOCATION {
            pResource: ManuallyDrop::new(Some(upload.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        };
        list.ResourceBarrier(&[barrier(
            dst,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_COPY_DEST,
        )]);
        list.CopyTextureRegion(&dst_loc, 0, 0, 0, &src_loc, None);
        list.ResourceBarrier(&[barrier(
            dst,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        )]);
        dx.submit_wait(&list)?;
        Ok(())
    }
}

/// Per-frame UPLOAD-heap uploader for the guest-frame hot path.
/// Caches one staging buffer (reallocated only when the payload
/// outgrows `capacity`) and takes explicit `before`/`after` texture
/// states — `upload_texture`'s hardcoded COMMON->COPY_DEST->PSR
/// transition is wrong for a texture whose steady state is
/// NON_PIXEL_SHADER_RESOURCE.
#[derive(Default)]
pub struct FrameUploader {
    buf: Option<ID3D12Resource>,
    capacity: u64,
}

impl FrameUploader {
    pub fn new() -> Self {
        FrameUploader {
            buf: None,
            capacity: 0,
        }
    }

    /// Stage `data` through the cached UPLOAD buffer into `dst` and
    /// leave `dst` in `after`. `before` must be the texture's tracked
    /// current state (COMMON on first use).
    #[allow(clippy::too_many_arguments)]
    pub fn upload(
        &mut self,
        dx: &Dx12,
        dst: &ID3D12Resource,
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
        data: &[u8],
        bytes_per_pixel: u32,
        before: D3D12_RESOURCE_STATES,
        after: D3D12_RESOURCE_STATES,
    ) -> GResult<()> {
        unsafe {
            let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            let mut row_size = 0u64;
            let mut total = 0u64;
            dx.device.GetCopyableFootprints(
                &tex_desc(w, h, format, D3D12_RESOURCE_FLAG_NONE),
                0,
                1,
                0,
                Some(&mut footprint),
                None,
                Some(&mut row_size),
                Some(&mut total),
            );

            if total > self.capacity {
                self.buf = Some(create_upload_buffer(dx, total)?);
                self.capacity = total;
            }
            let upload = self.buf.as_ref().unwrap();

            let mut ptr: *mut c_void = std::ptr::null_mut();
            upload
                .Map(0, None, Some(&mut ptr))
                .map_err(|e| hr("Map(upload)", e))?;
            let src_pitch = (w * bytes_per_pixel) as usize;
            let dst_pitch = footprint.Footprint.RowPitch as usize;
            for row in 0..h as usize {
                std::ptr::copy_nonoverlapping(
                    data.as_ptr().add(row * src_pitch),
                    (ptr as *mut u8).add(row * dst_pitch),
                    src_pitch.min(row_size as usize),
                );
            }
            upload.Unmap(0, None);

            let (_alloc, list) = dx.cmd()?;
            if before != D3D12_RESOURCE_STATE_COPY_DEST {
                list.ResourceBarrier(&[barrier(dst, before, D3D12_RESOURCE_STATE_COPY_DEST)]);
            }
            let dst_loc = D3D12_TEXTURE_COPY_LOCATION {
                pResource: ManuallyDrop::new(Some(dst.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            let src_loc = D3D12_TEXTURE_COPY_LOCATION {
                pResource: ManuallyDrop::new(Some(upload.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: footprint,
                },
            };
            list.CopyTextureRegion(&dst_loc, 0, 0, 0, &src_loc, None);
            if after != D3D12_RESOURCE_STATE_COPY_DEST {
                list.ResourceBarrier(&[barrier(dst, D3D12_RESOURCE_STATE_COPY_DEST, after)]);
            }
            // Wait for the GPU before returning so the cached staging
            // buffer is safe to overwrite on the next frame.
            dx.submit_wait(&list)?;
            Ok(())
        }
    }
}

/// Readback-heap read of an RGBA8 texture (verification path only —
/// counted separately from the zero-copy steady-state metric).
/// `before` is the texture's current state; it is left in COPY_SOURCE.
pub fn read_texture_rgba(
    dx: &Dx12,
    src: &ID3D12Resource,
    w: u32,
    h: u32,
    before: D3D12_RESOURCE_STATES,
) -> GResult<Vec<u8>> {
    unsafe {
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut num_rows = 0u32;
        let mut total = 0u64;
        dx.device.GetCopyableFootprints(
            &tex_desc(w, h, DXGI_FORMAT_R8G8B8A8_UNORM, D3D12_RESOURCE_FLAG_NONE),
            0,
            1,
            0,
            Some(&mut footprint),
            Some(&mut num_rows),
            None,
            Some(&mut total),
        );

        let rb = create_resource(
            dx,
            D3D12_HEAP_TYPE_READBACK,
            D3D12_HEAP_FLAG_NONE,
            &buffer_desc(total.max(1)),
            D3D12_RESOURCE_STATE_COPY_DEST,
        )?;

        let (_alloc, list) = dx.cmd()?;
        if before != D3D12_RESOURCE_STATE_COPY_SOURCE {
            list.ResourceBarrier(&[barrier(src, before, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
        }
        let dst_loc = D3D12_TEXTURE_COPY_LOCATION {
            pResource: ManuallyDrop::new(Some(rb.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        };
        let src_loc = D3D12_TEXTURE_COPY_LOCATION {
            pResource: ManuallyDrop::new(Some(src.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        list.CopyTextureRegion(&dst_loc, 0, 0, 0, &src_loc, None);
        dx.submit_wait(&list)?;

        super::count_verification_bytes((w as u64) * (h as u64) * 4);

        let mut ptr: *mut c_void = std::ptr::null_mut();
        let range = D3D12_RANGE {
            Begin: 0,
            End: total as usize,
        };
        rb.Map(0, Some(&range), Some(&mut ptr))
            .map_err(|e| hr("Map(readback)", e))?;
        let pitch = footprint.Footprint.RowPitch as usize;
        let mut out = vec![0u8; (w * h * 4) as usize];
        for row in 0..h as usize {
            std::ptr::copy_nonoverlapping(
                (ptr as *const u8).add(row * pitch),
                out.as_mut_ptr().add(row * (w * 4) as usize),
                (w * 4) as usize,
            );
        }
        rb.Unmap(0, None);
        Ok(out)
    }
}
