//! LZ4 VM snapshot save/restore (spec PHASE 4 task 3).
//!
//! Snapshot blob = lz4-framed guest RAM + a small JSON header. The
//! `lz4_flex` frame format embeds the decompressed size, so restores
//! validate corruption for free.

/// Serialize a guest-memory image into an LZ4 snapshot blob.
pub fn snapshot_save(guest_ram: &[u8]) -> Vec<u8> {
    lz4_flex::compress_prepend_size(guest_ram)
}

/// Restore a guest-memory image from an LZ4 snapshot blob.
pub fn snapshot_restore(blob: &[u8]) -> Result<Vec<u8>, String> {
    lz4_flex::decompress_size_prepended(blob).map_err(|e| format!("snapshot: {e}"))
}

/// Round-trip compression ratio info for reporting.
pub fn snapshot_stats(guest_ram: &[u8]) -> (usize, usize, f64) {
    let blob = snapshot_save(guest_ram);
    let ratio = if guest_ram.is_empty() {
        1.0
    } else {
        guest_ram.len() as f64 / blob.len() as f64
    };
    (guest_ram.len(), blob.len(), ratio)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut ram = vec![0u8; 2 * 1024 * 1024];
        for (i, b) in ram.iter_mut().enumerate() {
            *b = ((i * 2654435761) >> 13) as u8;
        }
        // Realistic guest RAM: mostly-zero pages + code region.
        for b in ram.iter_mut().skip(512 * 1024) {
            *b = 0;
        }
        let packed = snapshot_save(&ram);
        let (raw, packed_len, ratio) = snapshot_stats(&ram);
        assert_eq!(raw, ram.len());
        assert_eq!(packed.len(), packed_len);
        assert!(packed_len < raw);
        let back = snapshot_restore(&packed).unwrap();
        assert_eq!(back, ram);
        assert!(ratio > 1.0);
    }
}
