//! 4-byte trailing header integrity tag.
//!
//! Every wire frame carries a 4-byte trailing tag computed as
//! `SHA-256(header[0..12])[..4]`. This is **integrity only**: it catches
//! on-air bit-flips and accidental misroutes, but offers no protection
//! against a deliberate attacker who knows the channel PSK (Meshtastic's
//! channel encryption uses AES-CTR which is bit-flip malleable, so the
//! header MAC also serves as a tamper-detection check on the encrypted
//! payload bytes that carry the header).
//!
//! Tag width is fixed at 4 bytes — a deliberate trade against the
//! 215-byte payload budget. 32 bits is sufficient for collision-resistant
//! integrity on a 12-byte input (`2^32` random matches per chunk, well
//! beyond per-message frame counts) without burning further LoRa airtime.

use sha2::{Digest, Sha256};

use super::consts::{HEADER_MAC_LEN, HEADER_SIZE};
use super::error::{Result, VoiceError};

/// Compute the 4-byte trailing MAC over the first 12 bytes of a header.
pub(crate) fn compute_tag(header_no_mac: &[u8]) -> [u8; HEADER_MAC_LEN] {
    let mut out = [0u8; HEADER_MAC_LEN];
    let mut h = Sha256::new();
    h.update(header_no_mac);
    out.copy_from_slice(&h.finalize()[..HEADER_MAC_LEN]);
    out
}

/// Verify a parsed 16-byte header. Returns [`VoiceError::BadMac`] on
/// mismatch.
///
/// `header_with_mac` MUST be exactly [`HEADER_SIZE`] bytes; the caller
/// has already validated this in [`super::ChunkHeader::parse`].
pub(crate) fn verify(header_with_mac: &[u8]) -> Result<()> {
    debug_assert_eq!(header_with_mac.len(), HEADER_SIZE);
    let expected = compute_tag(&header_with_mac[..HEADER_SIZE - HEADER_MAC_LEN]);
    let mut diff = 0u8;
    for (a, b) in expected
        .iter()
        .zip(&header_with_mac[HEADER_SIZE - HEADER_MAC_LEN..])
    {
        diff |= a ^ b;
    }
    if diff == 0 {
        Ok(())
    } else {
        Err(VoiceError::BadMac)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_no_mac() -> [u8; HEADER_SIZE - HEADER_MAC_LEN] {
        [
            0x03, 0x00, 0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x00, 0x00, 0x00, 0x05, 0x00,
        ]
    }

    #[test]
    fn unkeyed_roundtrip() {
        let h12 = header_no_mac();
        let tag = compute_tag(&h12);
        let mut frame = [0u8; HEADER_SIZE];
        frame[..12].copy_from_slice(&h12);
        frame[12..].copy_from_slice(&tag);
        assert!(verify(&frame).is_ok());
        // Tampered field -> rejected.
        frame[6] ^= 1;
        assert!(matches!(verify(&frame), Err(VoiceError::BadMac)));
    }
}
