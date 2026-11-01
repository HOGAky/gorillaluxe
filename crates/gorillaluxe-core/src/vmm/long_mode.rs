//! Phase 4: 64-bit Long Mode guest bootstrap (spec PHASE 4 task 1).
//!
//! Builds a minimal long-mode environment inside guest RAM — 4-level
//! PML4 identity map (2 MiB pages), a 64-bit GDT, `EFER.LME/LMA`,
//! `CR4.PAE`, `CR0.PG` — then runs a real 64-bit payload (REX.W
//! `mov r64, imm64` + qword mailbox store + HLT). This is the same
//! CPU state the Linux 6.1 64-bit boot protocol expects once the
//! kernel's `startup_64` hands control to the VMM.
//!
//! The CI acceptance test verifies RAX == signature, the mailbox
//! contents, and that CS still carries the L (64-bit) attribute.

use std::ffi::c_void;

use windows::Win32::System::Hypervisor::*;
use windows::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};

use super::{SelfTest, VmError};
use crate::whpx::{Partition, Whpx, WhpxError};

// Guest RAM layout (1 MiB total).
const CODE_GPA: u64 = 0x1000;
const MAILBOX_GPA: u64 = 0x2000;
const PML4_GPA: u64 = 0x8000;
const PDPT_GPA: u64 = 0x9000;
const PD_GPA: u64 = 0xA000;
const GDT_GPA: u64 = 0xB000;
const MEM_SIZE: usize = 1024 * 1024;

/// `b"GLX-LM64"` packed little-endian — the 64-bit signature.
pub const LM64_MAGIC: u64 = u64::from_le_bytes(*b"GLX-LM64");

// VMCS access-rights format: Limit 0xFFFFFFFF requires G (bit 15) —
// newer WHPX builds reject G=0 + big limits as InvalidVpRegisterValue.
const SEG_ATTR_CODE64: u16 = 0xA09B; // G + P + code/read + L (long mode)
const SEG_ATTR_DATA64: u16 = 0x8093; // G + P + writable data

fn reg64(v: u64) -> WHV_REGISTER_VALUE {
    let mut r = WHV_REGISTER_VALUE::default();
    r.Reg64 = v;
    r
}

fn segment(attr: u16, selector: u16) -> WHV_REGISTER_VALUE {
    let mut r = WHV_REGISTER_VALUE::default();
    r.Segment = WHV_X64_SEGMENT_REGISTER {
        Base: 0,
        Limit: 0xFFFF_FFFF,
        Selector: selector,
        Anonymous: WHV_X64_SEGMENT_REGISTER_0 { Attributes: attr },
    };
    r
}

fn table(base: u64, limit: u16) -> WHV_REGISTER_VALUE {
    let mut r = WHV_REGISTER_VALUE::default();
    r.Table = WHV_X64_TABLE_REGISTER {
        Pad: [0; 3],
        Limit: limit,
        Base: base,
    };
    r
}

struct GuestRam(*mut c_void);

impl GuestRam {
    fn alloc() -> Result<Self, VmError> {
        let ptr = unsafe { VirtualAlloc(None, MEM_SIZE, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
        if ptr.is_null() {
            return Err(VmError::Memory("VirtualAlloc failed".into()));
        }
        Ok(GuestRam(ptr))
    }
    unsafe fn w64(&self, gpa: u64, v: u64) {
        std::ptr::write((self.0 as *mut u8).add(gpa as usize) as *mut u64, v);
    }
    unsafe fn write(&self, gpa: u64, data: &[u8]) {
        std::ptr::copy_nonoverlapping(
            data.as_ptr(),
            (self.0 as *mut u8).add(gpa as usize),
            data.len(),
        );
    }
}

impl Drop for GuestRam {
    fn drop(&mut self) {
        unsafe {
            let _ = VirtualFree(self.0, 0, MEM_RELEASE);
        }
    }
}

/// Write PML4/PDPT/PD identity map (0..1 GiB, 2 MiB pages), GDT and
/// the 64-bit payload into the guest image.
fn build_guest_image(mem: &GuestRam) {
    unsafe {
        // PML4[0] -> PDPT; PDPT[0] -> PD; PD[i] -> 2MiB page at i*2MiB.
        mem.w64(PML4_GPA, PDPT_GPA | 0x3);
        mem.w64(PDPT_GPA, PD_GPA | 0x3);
        for i in 0..512u64 {
            mem.w64(PD_GPA + i * 8, (i * 0x20_0000) | 0x83); // P | RW | PS
        }
        // GDT: null, code64 (sel 0x08), data64 (sel 0x10).
        mem.w64(GDT_GPA, 0);
        mem.w64(GDT_GPA + 8, 0x00AF_9B00_0000_FFFF); // 64-bit code, L=1
        mem.w64(GDT_GPA + 16, 0x008F_9300_0000_FFFF); // data
                                                      // Payload: mov rax, MAGIC; mov rcx, MAILBOX; mov [rcx], rax; hlt.
        let mut code = Vec::new();
        code.extend_from_slice(&[0x48, 0xB8]);
        code.extend_from_slice(&LM64_MAGIC.to_le_bytes());
        code.extend_from_slice(&[0x48, 0xB9]);
        code.extend_from_slice(&MAILBOX_GPA.to_le_bytes());
        code.extend_from_slice(&[0x48, 0x89, 0x01, 0xF4]);
        mem.write(CODE_GPA, &code);
    }
}

/// Switch the vCPU directly into 64-bit Long Mode (spec §3.2 note:
/// standard instruction lengths and MMIO/PIO exits apply here, unlike
/// the 16-bit real-mode workaround).
fn setup_long_mode(part: &Partition, vp: u32) -> Result<(), VmError> {
    let names = [
        WHvX64RegisterRip,
        WHvX64RegisterRflags,
        WHvX64RegisterCr0,
        WHvX64RegisterCr3,
        WHvX64RegisterCr4,
        WHvX64RegisterEfer,
        WHvX64RegisterGdtr,
        WHvX64RegisterCs,
        WHvX64RegisterDs,
        WHvX64RegisterEs,
        WHvX64RegisterSs,
        WHvX64RegisterRsp,
    ];
    let values = [
        reg64(CODE_GPA),      // Rip
        reg64(0x2),           // Rflags
        reg64(0x8000_0031),   // CR0: PG|PE|ET|MP
        reg64(PML4_GPA),      // CR3 -> PML4
        reg64(0x20),          // CR4: PAE
        reg64(0x500),         // EFER: LME|LMA
        table(GDT_GPA, 0x17), // GDTR
        segment(SEG_ATTR_CODE64, 0x08),
        segment(SEG_ATTR_DATA64, 0x10),
        segment(SEG_ATTR_DATA64, 0x10),
        segment(SEG_ATTR_DATA64, 0x10),
        reg64(0x7000), // Rsp
    ];
    part.set_registers(vp, &names, &values)?;
    Ok(())
}

/// Phase 4 acceptance (WHPX path): boot a vCPU straight into 64-bit
/// long mode and verify the signature + CS L-bit after HLT.
pub fn run_long_mode_test() -> Result<SelfTest, VmError> {
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

    // Hosted CI runners can transiently refuse a vCPU run
    // (WHvRunVpExitReasonUnsupportedFeature / canceled exits under
    // nested-virt load) — recreate the partition and retry a few
    // times before declaring failure. A real guest bug (triple
    // fault at a stable RIP) still fails after all attempts.
    let mut last_err = String::new();
    let mut regs = Vec::new();
    let mut preempted_total = 0u32;
    let mem = GuestRam::alloc()?;
    'attempt: for attempt in 1..=3u32 {
        let part = api.create_partition(1)?;
        build_guest_image(&mem);
        unsafe {
            part.map_gpa_range(
                mem.0,
                0,
                MEM_SIZE as u32,
                WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite | WHvMapGpaRangeFlagExecute,
            )?;
        }

        const VP: u32 = 0;
        part.create_vcpu(VP)?;
        setup_long_mode(&part, VP)?;

        // Straight 64-bit payload → the run must reach HLT.
        // Canceled/None exits are scheduler preemption, not guest
        // faults — re-run the vCPU instead of failing the attempt.
        let mut exit = WHV_RUN_VP_EXIT_CONTEXT::default();
        let mut preempted = 0u32;
        for _ in 0..64 {
            part.run(VP, &mut exit)?;
            if exit.ExitReason == WHvRunVpExitReasonCanceled
                || exit.ExitReason == WHvRunVpExitReasonNone
            {
                preempted += 1;
                continue;
            }
            break;
        }
        if exit.ExitReason != WHvRunVpExitReasonX64Halt {
            last_err = format!(
                "reason={} RIP=0x{:X} (attempt {attempt}/3, preempted {preempted})",
                exit.ExitReason.0, exit.VpContext.Rip
            );
            continue 'attempt;
        }
        preempted_total += preempted;
        regs = part.get_registers(
            VP,
            &[WHvX64RegisterRax, WHvX64RegisterCs, WHvX64RegisterEfer],
        )?;
        part.delete_vcpu(VP).ok();
        break 'attempt;
    }
    if regs.is_empty() {
        return Err(VmError::Exit {
            reason: -1,
            detail: format!("{last_err} (expected straight 64-bit run to HLT)"),
        });
    }

    let rax = unsafe { regs[0].Reg64 };
    let cs_attr = unsafe { regs[1].Segment.Anonymous.Attributes };
    let efer = unsafe { regs[2].Reg64 };
    let mailbox =
        unsafe { std::slice::from_raw_parts((mem.0 as *const u8).add(MAILBOX_GPA as usize), 8) };
    let sig_ok = rax == LM64_MAGIC && mailbox == b"GLX-LM64";
    let long_ok = cs_attr & 0x2000 != 0 && efer & 0x500 == 0x500;

    if sig_ok && long_ok {
        Ok(SelfTest::Pass {
            transcript: format!(
                "long-mode signature={:#X} CS.attr=0x{:X} EFER=0x{:X} preempted={}",
                rax, cs_attr, efer, preempted_total
            ),
        })
    } else {
        Err(VmError::GuestProtocol(format!(
            "rax={:#X} mailbox={:?} cs.attr=0x{:X} efer=0x{:X}",
            rax, mailbox, cs_attr, efer
        )))
    }
}
