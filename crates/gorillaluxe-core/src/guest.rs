//! Minimal real-mode guest payload for the Phase 1 WHPX smoke test.
//!
//! Loaded at GPA 0x1000. Copies MAGIC to the mailbox at MAILBOX_GPA,
//! then sets a completion signature in RAX and HLTs.
//!
//! Design note: WHPX on windows-latest does surface port-IO VM-exits,
//! but for 16-bit real-mode instructions it reports
//! InstructionByteCount = 0 and undefined AccessInfo fields, so the VMM
//! cannot advance RIP generically. A memory mailbox avoids the issue
//! entirely and additionally lets the host verify the full transcript.
//! Later phases replace this with a ~40 MB Linux 6.1 micro-guest
//! (bzImage + busybox initramfs) booted via virtio devices.

/// Guest physical address where the payload is loaded.
pub const SMOKE_GPA: u64 = 0x1000;

/// Guest physical address of the transcript mailbox.
pub const MAILBOX_GPA: u64 = 0x2000;

/// Bytes the guest must write into the mailbox.
pub const MAGIC_MSG: &[u8] = b"GLX-PH1-OK\r\n\0";

/// Expected vCPU register state after HLT (proves full payload execution):
/// `mov ax, MAGIC_REG` runs after the copy loop, `lodsb` leaves SI past
/// the terminating NUL, and `stosb`-style DI points past the mailbox.
pub const MAGIC_REG: u64 = 0x600D;
/// SI ends at msg abs address + msg length (incl. NUL consumed).
pub const MAGIC_SI: u64 = 0x1021;
/// DI ends at MAILBOX_GPA + msg length (incl. NUL written).
pub const MAGIC_DI: u64 = 0x200D;

/// 16-bit real-mode machine code, hand-assembled.
/// All data addresses are absolute linear addresses (real mode, DS=ES=0).
/// ```text
/// 0x00: BE 14 10   mov si, 0x1014      ; SI = message (GPA 0x1014)
/// 0x03: BF 00 20   mov di, 0x2000      ; DI = mailbox (GPA 0x2000)
/// 0x06: AC         lodsb               ; AL = [SI], SI++
/// 0x07: 88 05      mov [di], al        ; mailbox byte
/// 0x09: 47         inc di
/// 0x0A: 84 C0      test al, al
/// 0x0C: 74 02      jz +2               ; -> MOV AX at 0x10
/// 0x0E: EB F6      jmp -10             ; -> LODSB at 0x06
/// 0x10: B8 0D 60   mov ax, 0x600D      ; completion signature
/// 0x13: F4         hlt                 ; VM-exit, done
/// 0x14: "GLX-PH1-OK\r\n", 0
/// ```
pub const SMOKE_PAYLOAD: &[u8] = &[
    0xBE, 0x14, 0x10, // mov si, 0x1014
    0xBF, 0x00, 0x20, // mov di, 0x2000
    0xAC, // lodsb
    0x88, 0x05, // mov [di], al
    0x47, // inc di
    0x84, 0xC0, // test al, al
    0x74, 0x02, // jz +2 -> 0x10
    0xEB, 0xF6, // jmp -10 -> 0x06
    0xB8, 0x0D, 0x60, // mov ax, 0x600D
    0xF4, // hlt
    b'G', b'L', b'X', b'-', b'P', b'H', b'1', b'-', b'O', b'K', b'\r', b'\n', 0x00,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Offset of the embedded message inside the payload.
    const MSG_OFFSET: usize = 0x14;
    const CODE_HLT_OFFSET: usize = 0x13;

    #[test]
    fn payload_is_well_formed() {
        // mov si imm = absolute msg address; mov di imm = mailbox.
        let si = u16::from_le_bytes([SMOKE_PAYLOAD[1], SMOKE_PAYLOAD[2]]) as u64;
        let di = u16::from_le_bytes([SMOKE_PAYLOAD[4], SMOKE_PAYLOAD[5]]) as u64;
        assert_eq!(si, SMOKE_GPA + MSG_OFFSET as u64);
        assert_eq!(di, MAILBOX_GPA);
        // Message content and terminator.
        assert_eq!(&SMOKE_PAYLOAD[MSG_OFFSET..], MAGIC_MSG);
        // HLT terminates the code section.
        assert_eq!(SMOKE_PAYLOAD[CODE_HLT_OFFSET], 0xF4);
        // Completion state contracts.
        assert_eq!(MAGIC_SI, SMOKE_GPA + SMOKE_PAYLOAD.len() as u64);
        assert_eq!(MAGIC_DI, MAILBOX_GPA + MAGIC_MSG.len() as u64);
    }
}
