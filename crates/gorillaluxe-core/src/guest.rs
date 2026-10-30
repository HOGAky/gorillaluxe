//! Minimal real-mode guest payload for the Phase 1 WHPX smoke test.
//!
//! Loaded at GPA 0x1000. Prints MAGIC to I/O port DEBUG_PORT, then HLTs.
//! Later phases replace this with a ~40 MB Linux 6.1 micro-guest
//! (bzImage + busybox initramfs) booted via virtio devices.

/// Guest physical address where the payload is loaded.
pub const SMOKE_GPA: u64 = 0x1000;

/// Bochs/QEMU-style debug port; the VMM captures writes here.
pub const DEBUG_PORT: u16 = 0xE9;

/// Bytes the guest must emit for the self-test to pass.
pub const MAGIC: &[u8] = b"GLX-PH1-OK";

/// Expected vCPU register state after HLT (proves full payload execution
/// even when the host does not surface port-IO VM-exits):
/// `mov ax, MAGIC_REG` runs after the print loop, and `lodsb` leaves
/// SI pointing past the terminating NUL.
pub const MAGIC_REG: u64 = 0x600D;
pub const MAGIC_SI: u64 = 0x1F; // MSG_OFFSET + msg len incl. NUL

/// 16-bit real-mode machine code, hand-assembled:
/// ```text
/// 0x00: BA E9 00   mov dx, 0xE9        ; DX = debug port
/// 0x03: BE 12 00   mov si, 0x0012      ; SI = message offset
/// 0x06: AC         lodsb               ; AL = [SI], SI++
/// 0x07: 84 C0      test al, al
/// 0x09: 74 03      jz +3               ; -> MOV AX at 0x0E
/// 0x0B: EE         out dx, al          ; VM-exit to host (if IO exits fire)
/// 0x0C: EB F8      jmp -8              ; -> LODSB at 0x06
/// 0x0E: B8 0D 60   mov ax, 0x600D      ; completion signature
/// 0x11: F4         hlt                 ; VM-exit, done
/// 0x12: "GLX-PH1-OK\r\n", 0
/// ```
pub const SMOKE_PAYLOAD: &[u8] = &[
    0xBA, 0xE9, 0x00, // mov dx, 0xE9
    0xBE, 0x12, 0x00, // mov si, 0x0012
    0xAC,             // lodsb
    0x84, 0xC0,       // test al, al
    0x74, 0x03,       // jz +3 -> 0x0E
    0xEE,             // out dx, al
    0xEB, 0xF8,       // jmp -8 -> 0x06
    0xB8, 0x0D, 0x60, // mov ax, 0x600D
    0xF4,             // hlt
    b'G', b'L', b'X', b'-', b'P', b'H', b'1', b'-', b'O', b'K', b'\r', b'\n', 0x00,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Offset of the embedded message inside the payload.
    const MSG_OFFSET: usize = 0x12;

    #[test]
    fn payload_is_well_formed() {
        // mov si immediate must point at the message.
        let imm = u16::from_le_bytes([SMOKE_PAYLOAD[4], SMOKE_PAYLOAD[5]]) as usize;
        assert_eq!(imm, MSG_OFFSET);
        assert!(SMOKE_PAYLOAD.len() > MSG_OFFSET + MAGIC.len());
        assert_eq!(&SMOKE_PAYLOAD[MSG_OFFSET..MSG_OFFSET + MAGIC.len()], MAGIC);
        assert_eq!(*SMOKE_PAYLOAD.last().unwrap(), 0x00);
        // HLT terminates the code section.
        assert_eq!(SMOKE_PAYLOAD[0x11], 0xF4);
        // MAGIC_SI must equal msg offset + msg length + 1 (NUL consumed by lodsb).
        assert_eq!(MAGIC_SI as usize, SMOKE_PAYLOAD.len());
    }
}
