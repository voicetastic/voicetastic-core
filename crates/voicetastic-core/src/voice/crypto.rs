//! AES-256-GCM envelope keyed via HKDF-SHA256.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

use super::consts::{GCM_NONCE_LEN, GCM_TAG_LEN};
use super::error::{Result, VoiceError};

/// 256-bit AES-GCM key derived per-message via [`derive_key`].
#[derive(Clone)]
pub struct EnvelopeKey([u8; 32]);

impl std::fmt::Debug for EnvelopeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never log raw key material.
        f.write_str("EnvelopeKey(***)")
    }
}

impl EnvelopeKey {
    /// Wrap raw key material (e.g. for tests).
    pub fn from_bytes(b: [u8; 32]) -> Self {
        Self(b)
    }
}

/// Per-message envelope key:
/// `key = HKDF-SHA256(salt = channel_psk, ikm = message_id_be || from_be,
/// info = "voicetastic/v2")`.
///
/// The HKDF `info` string is preserved across protocol revisions for
/// forward-compat with already-shipped derivers; do not change it.
pub fn derive_key(channel_psk: &[u8], message_id: u32, from_node_num: u32) -> EnvelopeKey {
    let mut ikm = [0u8; 8];
    ikm[..4].copy_from_slice(&message_id.to_be_bytes());
    ikm[4..].copy_from_slice(&from_node_num.to_be_bytes());
    let hk = Hkdf::<Sha256>::new(Some(channel_psk), &ikm);
    let mut out = [0u8; 32];
    hk.expand(b"voicetastic/v2", &mut out)
        .expect("HKDF SHA-256 32 B is always valid");
    EnvelopeKey(out)
}

/// Encrypt `plaintext` under `key` with random nonce, binding the 12-byte
/// `header_aad`. Returns `nonce || ciphertext || tag`.
pub fn encrypt_body(key: &EnvelopeKey, header_aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let aes_key = Key::<Aes256Gcm>::from_slice(&key.0);
    let cipher = Aes256Gcm::new(aes_key);
    let mut nonce_bytes = [0u8; GCM_NONCE_LEN];
    getrandom::fill(&mut nonce_bytes).expect("OS RNG");
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad: header_aad,
            },
        )
        .expect("AES-GCM encrypt cannot fail with valid inputs");
    let mut out = Vec::with_capacity(GCM_NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    out
}

/// Reverse of [`encrypt_body`]. Verifies the GCM tag against `header_aad`.
pub fn decrypt_body(key: &EnvelopeKey, header_aad: &[u8], body: &[u8]) -> Result<Vec<u8>> {
    if body.len() < GCM_NONCE_LEN + GCM_TAG_LEN {
        return Err(VoiceError::BodyTooShortForEnv(body.len()));
    }
    let aes_key = Key::<Aes256Gcm>::from_slice(&key.0);
    let cipher = Aes256Gcm::new(aes_key);
    let nonce = Nonce::from_slice(&body[..GCM_NONCE_LEN]);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: &body[GCM_NONCE_LEN..],
                aad: header_aad,
            },
        )
        .map_err(|_| VoiceError::BadTag)
}

#[cfg(test)]
mod tests {
    use super::super::consts::HEADER_SIZE;
    use super::*;

    #[test]
    fn encryption_envelope_roundtrip() {
        let key = derive_key(b"psk", 0xDEADBEEF, 0x12345678);
        let header = [0u8; HEADER_SIZE];
        let pt = b"some plaintext";
        let ct = encrypt_body(&key, &header, pt);
        assert!(ct.len() >= GCM_NONCE_LEN + GCM_TAG_LEN);
        let pt2 = decrypt_body(&key, &header, &ct).unwrap();
        assert_eq!(pt2, pt);
        // Tampered AAD -> auth failure.
        let mut bad_aad = header;
        bad_aad[3] ^= 1;
        assert!(decrypt_body(&key, &bad_aad, &ct).is_err());
    }
}
