//! Phase 1 VMM: boots the real-mode smoke guest under WHPX and
//! captures its debug-port output.

use std::ffi::c_void;
use std::fmt;

use windows::Win32::System::Hypervisor::*;
use windows::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};

use crate::guest;
use crate::whpx::{Partition, Whpx, WhpxError};

const GUEST_MEM_SIZE: usize = 1024 * 1024; // 1 MiB
const MAX_EXITS: usize = 8192;

#[derive(Debug)]
pub enum VmError {
    Whpx(WhpxError),
    Memory(String),
    Exit { reason: i32, detail: String },
    GuestProtocol(String),
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmError::Whpx(e) => write!(f, "{}", e),
            VmError::Memory(m) => write!(f, "guest memory: {}", m),
            VmError::Exit { reason, detail } => {
                write!(f, "unexpected VM exit (reason {}) {}", reason, detail)
            }
            VmError::GuestProtocol(m) => write!(f, "guest protocol: {}", m),
        }
    }
}

impl std::error::Error for VmError {}

impl From<WhpxError> for VmError {
    fn from(e: WhpxError) -> Self {
        VmError::Whpx(e)
    }
}

/// Result of `run_smoke_test`.
pub enum SelfTest {
    /// WHPX ran the guest and the magic sequence was received.
    Pass { transcript: String },
    /// WHPX is not usable on this host; the test did not really run.
    Skipped(&'static str),
}

/// Guard for the host buffer backing guest RAM.
struct GuestMemory {
    ptr: *mut c_void,
    size: usize,
}

impl GuestMemory {
    fn alloc(size: usize) -> Result<Self, VmError> {
        let ptr = unsafe {
            VirtualAlloc(
                None,
                size,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if ptr.is_null() {
            return Err(VmError::Memory("VirtualAlloc failed".into()));
        }
        Ok(GuestMemory { ptr, size })
    }
}

impl Drop for GuestMemory {
    fn drop(&mut self) {
        unsafe {
            let _ = VirtualFree(self.ptr, 0, MEM_RELEASE);
        }
    }
}

fn reg64(v: u64) -> WHV_REGISTER_VALUE {
    let mut r = WHV_REGISTER_VALUE::default();
    r.Reg64 = v;
    r
}

fn segment(attr: u16) -> WHV_REGISTER_VALUE {
    let mut r = WHV_REGISTER_VALUE::default();
    r.Segment = WHV_X64_SEGMENT_REGISTER {
        Base: 0,
        Limit: 0xFFFF,
        Selector: 0,
        Anonymous: WHV_X64_SEGMENT_REGISTER_0 { Attributes: attr },
    };
    r
}

const SEG_ATTR_CODE16: u16 = 0x009B; // present, executable/readable, 16-bit
const SEG_ATTR_DATA16: u16 = 0x0093; // present, writable, 16-bit

fn setup_real_mode(part: &Partition, vp: u32, rip: u64) -> Result<(), VmError> {
    let names = [
        WHvX64RegisterRip,
        WHvX64RegisterRflags,
        WHvX64RegisterRax,
        WHvX64RegisterRbx,
        WHvX64RegisterRcx,
        WHvX64RegisterRdx,
        WHvX64RegisterRsi,
        WHvX64RegisterRdi,
        WHvX64RegisterRbp,
        WHvX64RegisterRsp,
        WHvX64RegisterCs,
        WHvX64RegisterDs,
        WHvX64RegisterEs,
        WHvX64RegisterSs,
        WHvX64RegisterFs,
        WHvX64RegisterGs,
    ];
    let values = [
        reg64(rip),
        reg64(0x2),
        reg64(0),
        reg64(0),
        reg64(0),
        reg64(0),
        reg64(0),
        reg64(0),
        reg64(0),
        reg64(0x8000),
        segment(SEG_ATTR_CODE16),
        segment(SEG_ATTR_DATA16),
        segment(SEG_ATTR_DATA16),
        segment(SEG_ATTR_DATA16),
        segment(SEG_ATTR_DATA16),
        segment(SEG_ATTR_DATA16),
    ];
    part.set_registers(vp, &names, &values)?;
    Ok(())
}

/// Phase 1 acceptance test: run the debug-port guest under WHPX and
/// verify the magic sequence `GLX-PH1-OK` arrives before HLT.
pub fn run_smoke_test() -> Result<SelfTest, VmError> {
    let api = match Whpx::instance() {
        Ok(api) => api,
        Err(WhpxError::NotInstalled) => return Ok(SelfTest::Skipped("whpx_not_installed")),
        Err(e) => return Err(VmError::Whpx(e)),
    };
    match api.hypervisor_present() {
        Ok(true) => {}
        Ok(false) => return Ok(SelfTest::Skipped("hypervisor_not_present")),
        Err(e) => return Err(VmError::Whpx(e)),
    }

    let part = api.create_partition(1)?;

    let mem = GuestMemory::alloc(GUEST_MEM_SIZE)?;
    unsafe {
        std::ptr::copy_nonoverlapping(
            guest::SMOKE_PAYLOAD.as_ptr(),
            (mem.ptr as *mut u8).add(guest::SMOKE_GPA as usize),
            guest::SMOKE_PAYLOAD.len(),
        );
    }
    part.map_gpa_range(
        mem.ptr,
        0,
        mem.size as u32,
        WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
    )?;

    const VP: u32 = 0;
    part.create_vcpu(VP)?;
    setup_real_mode(&part, VP, guest::SMOKE_GPA)?;

    let mut transcript: Vec<u8> = Vec::new();
    let mut halted = false;

    for _ in 0..MAX_EXITS {
        let mut exit = WHV_RUN_VP_EXIT_CONTEXT::default();
        part.run(VP, &mut exit)?;
        let reason = exit.ExitReason;
        if reason == WHvRunVpExitReasonX64IoPortAccess {
            let io = unsafe { exit.Anonymous.IoPortAccess };
            let info = unsafe { io.AccessInfo.AsUINT32 };
            let is_write = (info >> 3) & 1 == 1;
            let access_size = (info & 0x7) as usize;
            if is_write && io.PortNumber == guest::DEBUG_PORT {
                let bytes = io.Rax.to_le_bytes();
                transcript.extend_from_slice(&bytes[..access_size.min(8)]);
            }
            // IO exits are pre-instruction: advance RIP past it.
            let next_rip = exit.VpContext.Rip + io.InstructionByteCount as u64;
            part.set_registers(VP, &[WHvX64RegisterRip], &[reg64(next_rip)])?;
        } else if reason == WHvRunVpExitReasonX64Halt {
            halted = true;
            break;
        } else if reason == WHvRunVpExitReasonMemoryAccess {
            let mem_acc = unsafe { exit.Anonymous.MemoryAccess };
            return Err(VmError::Exit {
                reason: reason.0,
                detail: format!(
                    "guest touched unmapped GPA 0x{:X} (RIP=0x{:X})",
                    mem_acc.Gpa, exit.VpContext.Rip
                ),
            });
        } else {
            return Err(VmError::Exit {
                reason: reason.0,
                detail: format!("(RIP=0x{:X})", exit.VpContext.Rip),
            });
        }
    }

    part.delete_vcpu(VP).ok();

    if !halted {
        return Err(VmError::GuestProtocol(
            "exit budget exhausted before HLT".into(),
        ));
    }

    let text = String::from_utf8_lossy(&transcript);
    if text.contains(std::str::from_utf8(guest::MAGIC).unwrap()) {
        Ok(SelfTest::Pass {
            transcript: text.trim_end().to_string(),
        })
    } else {
        Err(VmError::GuestProtocol(format!(
            "magic {:?} not found in transcript {:?}",
            String::from_utf8_lossy(guest::MAGIC),
            text
        )))
    }
}
