//! Meshtastic PKI direct-message decrypt.
//!
//! Mirrors the firmware's `CryptoEngine::decryptCurve25519` (see
//! `src/mesh/CryptoEngine.cpp` in the Meshtastic firmware): X25519 ECDH
//! → SHA-256 → AES-256-CCM with an 8-byte authentication tag. Used when
//! the radio's phone-API hands us a `MeshPacket::Encrypted` whose `to`
//! matches our own node number — that happens when the firmware itself
//! couldn't decrypt the PKC DM (typically because the sender's public
//! key wasn't loaded into the radio's nodeDB at decrypt time).
//!
//! Wire layout (matching firmware):
//! ```text
//!   [ ciphertext  | auth_tag (8 B) | extra_nonce (4 B, little-endian u32) ]
//!   |  N bytes    |   8 bytes      |  4 bytes                              |
//! ```
//! Total ciphertext-and-trailer length therefore equals `plaintext_len + 12`
//! (the [`PKC_OVERHEAD`] constant).
//!
//! Nonce (13 bytes total, zero-padded to the right):
//! ```text
//!   [ packet_id (4 LE) | extra_nonce (4 LE) | from_node (4 LE) | 0 ]
//! ```
//! Note: the firmware's `initNonce` overwrites the high half of
//! `packet_id` with `extra_nonce` (a quirk of using `memcpy` with
//! `sizeof(uint32_t)` as the offset rather than `sizeof(uint64_t)`).
//! `packet_id` is a u32 on the wire anyway, so the high half it writes
//! is always zero pre-overwrite — we mirror the layout exactly.

use aes::Aes256;
use ccm::Ccm;
use ccm::aead::generic_array::GenericArray;
use ccm::aead::{AeadInPlace, KeyInit};
use ccm::consts::{U8, U13};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

/// Bytes added to the plaintext when encrypted: 8-byte CCM tag + 4-byte
/// extra-nonce, always trailing the ciphertext.
pub const PKC_OVERHEAD: usize = 12;

/// AES-256-CCM with a 13-byte nonce and an 8-byte tag, matching the
/// firmware's `aes_ccm_ae`/`aes_ccm_ad` calls.
type Aes256Ccm = Ccm<Aes256, U8, U13>;

/// Decrypt a Meshtastic PKC DM. Returns the plaintext `meshtastic.Data`
/// protobuf bytes (the caller decodes them) on success, or `None` if any
/// step fails. Never panics.
pub fn decrypt(
    our_private_key: &[u8; 32],
    peer_public_key: &[u8; 32],
    from_node: u32,
    packet_id: u32,
    ciphertext_and_trailer: &[u8],
) -> Option<Vec<u8>> {
    // Need at least the 12-byte overhead, otherwise there's no auth tag
    // or nonce to read.
    if ciphertext_and_trailer.len() < PKC_OVERHEAD {
        return None;
    }
    let split = ciphertext_and_trailer.len() - PKC_OVERHEAD;
    let (ct, trailer) = ciphertext_and_trailer.split_at(split);
    let (tag_bytes, extra_nonce_bytes) = trailer.split_at(8);

    let extra_nonce = u32::from_le_bytes(
        extra_nonce_bytes
            .try_into()
            .expect("split_at guarantees len 4"),
    );

    // ECDH: derive the 32-byte shared point, then SHA-256 it to get the
    // AES-256 key. `StaticSecret::diffie_hellman` accepts a non-clamped
    // private key by clamping internally, matching libsodium / the
    // firmware's `crypto_scalarmult`.
    let our_secret = StaticSecret::from(*our_private_key);
    let peer_public = PublicKey::from(*peer_public_key);
    let shared_point = our_secret.diffie_hellman(&peer_public);
    let key_bytes: [u8; 32] = Sha256::digest(shared_point.as_bytes()).into();

    let nonce = build_nonce(from_node, packet_id, extra_nonce);

    let mut buf = ct.to_vec();
    let cipher = Aes256Ccm::new(&key_bytes.into());
    cipher
        .decrypt_in_place_detached(
            GenericArray::from_slice(&nonce),
            &[],
            &mut buf,
            GenericArray::from_slice(tag_bytes),
        )
        .ok()?;
    Some(buf)
}

/// Build the 13-byte AES-CCM nonce. See module-level docs for the layout
/// and the firmware quirk we mirror.
fn build_nonce(from_node: u32, packet_id: u32, extra_nonce: u32) -> [u8; 13] {
    let mut nonce = [0u8; 13];
    nonce[0..4].copy_from_slice(&packet_id.to_le_bytes());
    nonce[4..8].copy_from_slice(&extra_nonce.to_le_bytes());
    nonce[8..12].copy_from_slice(&from_node.to_le_bytes());
    nonce
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Local mirror of the firmware's encrypt path, for round-trip tests.
    /// Not exposed publicly — the desktop currently only consumes inbound
    /// PKC, never sends it. If sender-side PKC is ever added the encrypt
    /// path belongs in a sibling function with its own tests.
    fn encrypt(
        our_private_key: &[u8; 32],
        peer_public_key: &[u8; 32],
        from_node: u32,
        packet_id: u32,
        plaintext: &[u8],
        extra_nonce: u32,
    ) -> Vec<u8> {
        let our_secret = StaticSecret::from(*our_private_key);
        let peer_public = PublicKey::from(*peer_public_key);
        let shared_point = our_secret.diffie_hellman(&peer_public);
        let key_bytes: [u8; 32] = Sha256::digest(shared_point.as_bytes()).into();
        let nonce = build_nonce(from_node, packet_id, extra_nonce);

        let mut buf = plaintext.to_vec();
        let cipher = Aes256Ccm::new(&key_bytes.into());
        let tag = cipher
            .encrypt_in_place_detached(GenericArray::from_slice(&nonce), &[], &mut buf)
            .expect("encrypt");
        buf.extend_from_slice(tag.as_slice());
        buf.extend_from_slice(&extra_nonce.to_le_bytes());
        buf
    }

    /// Both ends share a key pair derived from a deterministic seed so
    /// the test is reproducible — Diffie–Hellman is symmetric, so Alice
    /// decrypting from Bob == Bob decrypting from Alice.
    fn keypair(seed: u8) -> ([u8; 32], [u8; 32]) {
        let mut sk = [seed; 32];
        // X25519 secret-key clamping. `StaticSecret::from` applies this
        // internally, but we need the public key, so re-derive it here.
        sk[0] &= 248;
        sk[31] &= 127;
        sk[31] |= 64;
        let secret = StaticSecret::from(sk);
        let pk = PublicKey::from(&secret);
        (sk, *pk.as_bytes())
    }

    #[test]
    fn round_trip_short_payload() {
        let (alice_sk, alice_pk) = keypair(0x11);
        let (bob_sk, bob_pk) = keypair(0x22);
        let plaintext = b"hello PKC";
        let ct = encrypt(
            &alice_sk,
            &bob_pk,
            0xdead_beef,
            0x4242,
            plaintext,
            0x1234_5678,
        );
        let pt = decrypt(&bob_sk, &alice_pk, 0xdead_beef, 0x4242, &ct).expect("decrypt");
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn round_trip_max_payload() {
        let (alice_sk, alice_pk) = keypair(0x33);
        let (bob_sk, bob_pk) = keypair(0x44);
        // Largest realistic `meshtastic.Data` payload is ~237 B (MAX_LORA_PAYLOAD
        // minus per-packet overhead); cover that order of magnitude.
        let plaintext: Vec<u8> = (0..200).map(|i| (i & 0xff) as u8).collect();
        let ct = encrypt(&alice_sk, &bob_pk, 7, 0xcafebabe, &plaintext, 0);
        let pt = decrypt(&bob_sk, &alice_pk, 7, 0xcafebabe, &ct).expect("decrypt");
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn rejects_bit_flipped_ciphertext() {
        let (alice_sk, alice_pk) = keypair(0x55);
        let (bob_sk, bob_pk) = keypair(0x66);
        let mut ct = encrypt(&alice_sk, &bob_pk, 1, 1, b"sensitive", 0);
        // Flip a bit in the middle of the ciphertext — the auth tag must catch this.
        ct[3] ^= 0x01;
        assert!(decrypt(&bob_sk, &alice_pk, 1, 1, &ct).is_none());
    }

    #[test]
    fn rejects_truncated_input() {
        // Length less than the 12-byte overhead leaves no room for the
        // auth tag + extra nonce; reject up front.
        assert!(decrypt(&[0; 32], &[0; 32], 0, 0, &[0; 11]).is_none());
        assert!(decrypt(&[0; 32], &[0; 32], 0, 0, &[]).is_none());
    }

    #[test]
    fn rejects_wrong_packet_id() {
        let (alice_sk, alice_pk) = keypair(0x77);
        let (bob_sk, bob_pk) = keypair(0x88);
        let ct = encrypt(&alice_sk, &bob_pk, 42, 1000, b"abc", 0);
        // Same key, wrong packet id → derived nonce changes → tag mismatch.
        assert!(decrypt(&bob_sk, &alice_pk, 42, 1001, &ct).is_none());
    }

    #[test]
    fn rejects_wrong_sender_public_key() {
        let (alice_sk, _alice_pk) = keypair(0x99);
        let (bob_sk, bob_pk) = keypair(0xaa);
        let (_eve_sk, eve_pk) = keypair(0xbb);
        let ct = encrypt(&alice_sk, &bob_pk, 1, 1, b"hi", 0);
        // Bob decrypts pretending it's from Eve — shared secret differs.
        assert!(decrypt(&bob_sk, &eve_pk, 1, 1, &ct).is_none());
    }

    /// Wycheproof X25519 vector — the same one the Meshtastic firmware
    /// tests its `setDHPrivateKey` / `setDHPublicKey` path against (see
    /// `test/test_crypto/test_main.cpp::test_DH25519` in the firmware).
    /// Locks down the dalek crate's interpretation of byte arrays so a
    /// future upstream change can't silently desync us from the radio.
    #[test]
    fn x25519_wycheproof_vector() {
        let public = hex_to_32("504a36999f489cd2fdbc08baff3d88fa00569ba986cba22548ffde80f9806829");
        let secret = hex_to_32("c8a9d5a91091ad851c668b0736c1c9a02936c0d3ad62670858088047ba057475");
        let expected =
            hex_to_32("436a2c040cf45fea9b29a0cb81b1f41458f863d0d61b453d0a982720d6d61320");

        let shared = StaticSecret::from(secret).diffie_hellman(&PublicKey::from(public));
        assert_eq!(shared.as_bytes(), &expected);
    }

    /// Firmware ground-truth: the exact decrypt vector from
    /// `test/test_crypto/test_main.cpp::test_PKC`. Decrypts a 22-byte
    /// PKC payload that was encrypted by the firmware's reference
    /// `encryptCurve25519` and recovers the 10-byte `meshtastic.Data`
    /// plaintext. If this passes, our nonce layout, ECDH, SHA-256, and
    /// AES-CCM(8) are bit-for-bit compatible with the firmware.
    #[test]
    fn firmware_pkc_vector() {
        let our_private =
            hex_to_32("a00330633e63522f8a4d81ec6d9d1e6617f6c8ffd3a4c698229537d44e522277");
        let peer_public =
            hex_to_32("db18fc50eea47f00251cb784819a3cf5fc361882597f589f0d7ff820e8064457");
        let from_node = 0x0929_u32;
        // Firmware's `packetNum` is a u64 (0x13b2d662); only the low 32
        // bits enter the nonce (see `initNonce` in CryptoEngine.cpp).
        let packet_id = 0x13b2_d662_u32;
        let ciphertext_and_trailer =
            hex::decode("40df24abfcc30a17a3d9046726099e796a1c036a792b").unwrap();
        let expected_plaintext = hex::decode("08011204746573744800").unwrap();

        let pt = decrypt(
            &our_private,
            &peer_public,
            from_node,
            packet_id,
            &ciphertext_and_trailer,
        )
        .expect("firmware-encrypted vector must decrypt");
        assert_eq!(pt, expected_plaintext);
    }

    fn hex_to_32(s: &str) -> [u8; 32] {
        let bytes = hex::decode(s).expect("hex");
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        out
    }

    /// CCM nonce layout regression: byte-for-byte equivalent to the
    /// firmware's `initNonce(from_node, packet_id, extra_nonce)` for a
    /// non-zero `extra_nonce`. The high half of `packet_id` is
    /// intentionally overwritten by `extra_nonce` — see module docs.
    #[test]
    fn nonce_layout_matches_firmware() {
        let n = build_nonce(0x0a0b_0c0d, 0x1122_3344, 0x55aa_55aa);
        let expected = [
            0x44, 0x33, 0x22, 0x11, // packet_id LE
            0xaa, 0x55, 0xaa, 0x55, // extra_nonce LE (overlaps high half)
            0x0d, 0x0c, 0x0b, 0x0a, // from_node LE
            0x00, // pad
        ];
        assert_eq!(n, expected);
    }
}
