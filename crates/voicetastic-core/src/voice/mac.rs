//! 4-byte header MAC: integrity (always) + authenticity (when keyed).
//!
//! Every wire frame carries a 4-byte trailing tag covering the 12 logical
//! header bytes (`bytes[0..12]`). Two computation modes share the same
//! 4-byte slot, distinguished by the `mac_keyed` flag in the flags byte:
//!
//! - **Keyed (option A)**: `HMAC-SHA256(channel_psk, header[0..12])[..4]`.
//!   Provides authenticity *and* integrity for any peer that holds the
//!   channel PSK. A receiver without the PSK MUST reject the frame
//!   ([`VoiceError::MacKeyMissing`]).
//! - **Unkeyed**: `SHA-256(header[0..12])[..4]`. Pure integrity: catches
//!   on-air bit-flips and accidental misroutes; offers no protection
//!   against a deliberate attacker on the channel. SHA-256 truncated to
//!   32 bits matches a CRC's accidental-corruption capability while
//!   re-using the digest primitive already pulled in by [`super::crypto`].
//!
//! Tag width is fixed at 4 bytes — a deliberate trade against the
//! 215-byte payload budget. 32 bits is sufficient for collision-resistant
//! integrity on a 12-byte input (`2^32` random matches per chunk, well
//! beyond per-message frame counts) without burning further LoRa airtime.
//!
//! Other key-scoping options (per-sender derived key, per-message envelope
//! key) are documented in [`docs/wiki/Encryption.md`] as future work.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use super::consts::{HEADER_MAC_LEN, HEADER_SIZE};
use super::error::{Result, VoiceError};

/// Bit in the flags byte (`bytes[1]`) that distinguishes keyed (HMAC) from
/// unkeyed (SHA-256) MAC. Set by the sender at serialize time based on
/// whether a MAC key was provided.
pub(crate) const MAC_KEYED_FLAG: u8 = 0x08;

/// Compute the 4-byte trailing MAC over the first 12 bytes of a header.
/// If `key` is `Some`, returns HMAC-SHA256 truncated; otherwise returns
/// SHA-256 truncated. The caller is responsible for setting the
/// [`MAC_KEYED_FLAG`] bit in the flags byte before computing — flipping
/// it after the fact would invalidate the tag.
///
/// # Panics
/// HMAC init with an empty key will panic (HMAC-SHA256 requires at least
/// 1 byte). Callers MUST ensure `key` is `None` or `Some(&[u8])` with
/// non-zero length.
pub(crate) fn compute_tag(header_no_mac: &[u8], key: Option<&[u8]>) -> [u8; HEADER_MAC_LEN] {
    let mut out = [0u8; HEADER_MAC_LEN];
    match key {
        Some(k) => {
            // SAFETY: HMAC-SHA256 accepts any key length >= 1 byte. The
            // channel PSK is always at least 1 byte or `None`.
            let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(k)
                .expect("HMAC-SHA256 accepts any non-empty key");
            mac.update(header_no_mac);
            out.copy_from_slice(&mac.finalize().into_bytes()[..HEADER_MAC_LEN]);
        }
        None => {
            let mut h = Sha256::new();
            h.update(header_no_mac);
            out.copy_from_slice(&h.finalize()[..HEADER_MAC_LEN]);
        }
    }
    out
}

/// Verify a parsed 16-byte header against `key`. Returns
/// [`VoiceError::BadMac`] on mismatch and [`VoiceError::MacKeyMissing`]
/// if the header advertises a keyed MAC but the receiver has no PSK.
///
/// `header_with_mac` MUST be exactly [`HEADER_SIZE`] bytes; the caller
/// has already validated this in [`super::ChunkHeader::parse`].
pub(crate) fn verify(header_with_mac: &[u8], key: Option<&[u8]>) -> Result<()> {
    debug_assert_eq!(header_with_mac.len(), HEADER_SIZE);
    let keyed = header_with_mac[1] & MAC_KEYED_FLAG != 0;
    let effective_key = match (keyed, key) {
        (true, Some(k)) => Some(k),
        (true, None) => return Err(VoiceError::MacKeyMissing),
        (false, _) => None,
    };
    let expected = compute_tag(
        &header_with_mac[..HEADER_SIZE - HEADER_MAC_LEN],
        effective_key,
    );
    // Constant-time compare to avoid leaking a partial-match oracle on
    // the keyed path. 4 bytes is short enough that a naive `==` would
    // realistically not be exploitable, but the discipline is cheap.
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
        // Plausible 12-byte header with the keyed bit clear.
        [
            0x02, 0x00, 0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x00, 0x00, 0x00, 0x05, 0x00,
        ]
    }

    #[test]
    fn unkeyed_roundtrip() {
        let h12 = header_no_mac();
        let tag = compute_tag(&h12, None);
        let mut frame = [0u8; HEADER_SIZE];
        frame[..12].copy_from_slice(&h12);
        frame[12..].copy_from_slice(&tag);
        assert!(verify(&frame, None).is_ok());
        // Tampered field -> rejected.
        frame[6] ^= 1;
        assert!(matches!(verify(&frame, None), Err(VoiceError::BadMac)));
    }

    #[test]
    fn keyed_roundtrip() {
        let mut h12 = header_no_mac();
        h12[1] |= MAC_KEYED_FLAG;
        let key = b"channel-psk";
        let tag = compute_tag(&h12, Some(key));
        let mut frame = [0u8; HEADER_SIZE];
        frame[..12].copy_from_slice(&h12);
        frame[12..].copy_from_slice(&tag);
        assert!(verify(&frame, Some(key)).is_ok());
        // Wrong key -> rejected.
        assert!(matches!(
            verify(&frame, Some(b"wrong")),
            Err(VoiceError::BadMac),
        ));
        // No key -> MacKeyMissing (we cannot verify a keyed MAC blind).
        assert!(matches!(
            verify(&frame, None),
            Err(VoiceError::MacKeyMissing),
        ));
    }

    #[test]
    fn keyed_and_unkeyed_tags_differ() {
        let mut h12 = header_no_mac();
        let unkeyed = compute_tag(&h12, None);
        h12[1] |= MAC_KEYED_FLAG;
        let keyed = compute_tag(&h12, Some(b"channel-psk"));
        assert_ne!(unkeyed, keyed);
    }
}
