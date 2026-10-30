//! Zero-overhead frame publisher (spec §3.4).
//!
//! Publishes rendered RGBA frames into a Windows named file mapping
//! `Global\GorillaLuxe_Frame_<ID>` and signals the named event
//! `Global\GorillaLuxe_Event_<ID>` so external consumers (bots,
//! capture tools, the DX12 readback path) can pull frames without any
//! socket or pipe overhead.
//!
//! Layout: `[u64 seq][u64 payload_len][payload bytes…]`. `seq` is a
//! strictly increasing frame counter — readers that miss an event can
//! still poll `seq`.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    FILE_MAP_READ, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_ALL_ACCESS,
};

pub const FRAME_HEADER: usize = 16;
pub const PIXELS_1080P_RGBA: usize = 1920 * 1080 * 4;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Publisher side: owns the mapping + event handles.
pub struct ShmPublisher {
    map: HANDLE,
    event: HANDLE,
    view: *mut u8,
    capacity: usize,
    seq: u64,
    pub map_name: String,
    pub event_name: String,
}

unsafe impl Send for ShmPublisher {}

impl ShmPublisher {
    /// Create named mapping+event for `payload_bytes` of frame data.
    pub fn create(id: u32, payload_bytes: usize) -> Result<Self, String> {
        let total = FRAME_HEADER + payload_bytes;
        let map_name = format!("Global\\GorillaLuxe_Frame_{id}");
        let event_name = format!("Global\\GorillaLuxe_Event_{id}");
        unsafe {
            let map = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                (total >> 32) as u32,
                total as u32,
                PCWSTR(wide(&map_name).as_ptr()),
            )
            .map_err(|e| format!("CreateFileMappingW: {e}"))?;
            let view = MapViewOfFile(map, FILE_MAP_ALL_ACCESS, 0, 0, total);
            if view.Value.is_null() {
                let _ = CloseHandle(map);
                return Err("MapViewOfFile failed".into());
            }
            let event = CreateEventW(
                None,
                false,
                false,
                PCWSTR(wide(&event_name).as_ptr()),
            )
            .map_err(|e| {
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(map);
                format!("CreateEventW: {e}")
            })?;
            Ok(ShmPublisher {
                map,
                event,
                view: view.Value as *mut u8,
                capacity: payload_bytes,
                seq: 0,
                map_name,
                event_name,
            })
        }
    }

    /// Copy one frame into shared memory and pulse the event.
    /// Pure memcpy — no syscalls beyond `SetEvent`.
    pub fn write_frame(&mut self, payload: &[u8]) -> Result<(), String> {
        if payload.len() > self.capacity {
            return Err(format!(
                "frame {}B exceeds mapping capacity {}B",
                payload.len(),
                self.capacity
            ));
        }
        self.seq += 1;
        unsafe {
            let hdr = self.view;
            (hdr as *mut u64).write_unaligned(self.seq);
            (hdr.add(8) as *mut u64).write_unaligned(payload.len() as u64);
            std::ptr::copy_nonoverlapping(payload.as_ptr(), hdr.add(FRAME_HEADER), payload.len());
            SetEvent(self.event).map_err(|e| e.to_string())
        }
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }
}

impl Drop for ShmPublisher {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view as *mut _,
            });
            let _ = CloseHandle(self.event);
            let _ = CloseHandle(self.map);
        }
    }
}

/// Reader side: opens an existing publisher's mapping read-only.
pub struct ShmReader {
    map: HANDLE,
    event: HANDLE,
    view: *const u8,
    payload_bytes: usize,
}

unsafe impl Send for ShmReader {}

impl ShmReader {
    pub fn open(id: u32, payload_bytes: usize) -> Result<Self, String> {
        let map_name = format!("Global\\GorillaLuxe_Frame_{id}");
        let event_name = format!("Global\\GorillaLuxe_Event_{id}");
        unsafe {
            let map = OpenFileMappingW(
                FILE_MAP_READ.0,
                false,
                PCWSTR(wide(&map_name).as_ptr()),
            )
            .map_err(|e| format!("OpenFileMappingW: {e}"))?;
            let view = MapViewOfFile(map, FILE_MAP_READ, 0, 0, FRAME_HEADER + payload_bytes);
            if view.Value.is_null() {
                let _ = CloseHandle(map);
                return Err("MapViewOfFile(read) failed".into());
            }
            let event = OpenEventW(
                EVENT_ALL_ACCESS,
                false,
                PCWSTR(wide(&event_name).as_ptr()),
            )
            .map_err(|e| {
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(map);
                format!("OpenEventW: {e}")
            })?;
            Ok(ShmReader {
                map,
                event,
                view: view.Value as *const u8,
                payload_bytes,
            })
        }
    }

    /// Wait up to `timeout_ms` for a new frame pulse.
    pub fn wait(&self, timeout_ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.event, timeout_ms).0 == 0 }
    }

    /// Latest sequence number (lock-free header read).
    pub fn seq(&self) -> u64 {
        unsafe { (self.view as *const u64).read_unaligned() }
    }

    /// Copy the current frame payload into `dst`.
    /// Returns `(seq, len)`; `Err` if dst too small.
    pub fn read_frame(&self, dst: &mut [u8]) -> Result<(u64, usize), String> {
        unsafe {
            let len = (self.view.add(8) as *const u64).read_unaligned() as usize;
            let seq = self.seq();
            if len > self.payload_bytes || len > dst.len() {
                return Err(format!("frame len {len} > dst {}B", dst.len()));
            }
            std::ptr::copy_nonoverlapping(self.view.add(FRAME_HEADER), dst.as_mut_ptr(), len);
            Ok((seq, len))
        }
    }
}

impl Drop for ShmReader {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view as *mut _,
            });
            let _ = CloseHandle(self.event);
            let _ = CloseHandle(self.map);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn sixty_1080p_frames_under_60ms() {
        let mut pub_ = ShmPublisher::create(0xFACE, PIXELS_1080P_RGBA).unwrap();
        assert_eq!(pub_.map_name, "Global\\GorillaLuxe_Frame_61260");
        let reader = ShmReader::open(0xFACE, PIXELS_1080P_RGBA).unwrap();
        let mut frame = vec![0xABu8; PIXELS_1080P_RGBA];
        let mut sink = vec![0u8; PIXELS_1080P_RGBA];
        let t = Instant::now();
        for i in 0..60u8 {
            frame[12345] = i;
            pub_.write_frame(&frame).unwrap();
        }
        let write_ms = t.elapsed().as_secs_f64() * 1000.0;
        let (seq, len) = reader.read_frame(&mut sink).unwrap();
        assert_eq!(seq, 60);
        assert_eq!(len, PIXELS_1080P_RGBA);
        assert_eq!(sink[12345], 59);
        assert!(sink.iter().all(|&b| b == 0xAB || b == 59));
        assert!(write_ms < 60.0, "60 frames took {write_ms:.1}ms CPU");
    }
}
