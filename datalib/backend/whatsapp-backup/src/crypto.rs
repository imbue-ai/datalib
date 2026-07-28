//! Hand-rolled AES-256-GCM decrypt matching pycryptodome's behavior with
//! a 16-byte nonce, which is what WhatsApp's crypt15 format uses. The
//! NIST SP 800-38D Algorithm 4 J0 derivation for non-12-byte nonces is
//! `J0 = GHASH(H, IV || 0^s || len(IV)_bits_64)`; pycryptodome implements
//! that. The `aes-gcm` crate (v0.10) instantiates `AesGcm<_, U16>` but
//! produces a different keystream for the same key+IV — confirmed by
//! comparing keystreams byte-for-byte against pycryptodome's output on
//! a real msgstore.db.crypt15 backup. Rather than chase that
//! incompatibility, we compose GCM ourselves from the primitive parts:
//!
//! * `aes::Aes256` for E_K (block encrypt of `J0` and the `H` subkey)
//! * `ctr::Ctr32BE<Aes256>` for the keystream — counter increments the
//!   low 32 bits as a big-endian u32, per NIST.
//! * `ghash::GHash` for the universal hash, both in J0 derivation and
//!   the auth tag computation.
//!
//! Scope: decrypt-and-verify only. Empty AAD (WhatsApp doesn't use AAD
//! for crypt15). The full encrypt direction isn't needed by ingest.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit, KeyIvInit, StreamCipher};
use aes::Aes256;
use ghash::universal_hash::UniversalHash;
use ghash::GHash;
use subtle::ConstantTimeEq;
use thiserror::Error;

type AesCtr = ctr::Ctr32BE<Aes256>;

#[derive(Debug, Error)]
pub enum DecryptError {
    #[error("ciphertext too short: need at least 16 bytes for GCM auth tag, got {0}")]
    TooShort(usize),
    #[error("AES-256-GCM auth tag mismatch — wrong key, wrong IV, or corrupted file")]
    AuthFailed,
}

/// Decrypt `ciphertext_with_tag` (the body of a crypt15 file: the
/// AES-256-GCM ciphertext immediately followed by its 16-byte auth tag)
/// using `iv` (16 bytes), `key` (32 bytes), and `aad` (additional
/// authenticated data — `&[]` for crypt15). Returns the
/// zlib-compressed plaintext.
pub fn decrypt_crypt15(
    ciphertext_with_tag: &[u8],
    iv: &[u8; 16],
    key: &[u8; 32],
    aad: &[u8],
) -> Result<Vec<u8>, DecryptError> {
    if ciphertext_with_tag.len() < 16 {
        return Err(DecryptError::TooShort(ciphertext_with_tag.len()));
    }
    let (ciphertext, tag_in) = ciphertext_with_tag.split_at(ciphertext_with_tag.len() - 16);

    let aes = Aes256::new(GenericArray::from_slice(key));

    // GHASH subkey H = E_K(0^128).
    let h = compute_h(&aes);

    // J0 = GHASH(H, IV || 0^s || len(IV)_bits_64_BE), per NIST SP 800-38D §7.1.
    let j0 = compute_j0(&h, iv);

    // Keystream uses nonce = J0[0..12], initial counter = J0[12..16] interpreted
    // as u32 big-endian, then incremented for each subsequent block. The first
    // counter block in the keystream is J0+1 (the J0 itself is reserved for the
    // tag). The `ctr` crate's `Ctr32BE` does that increment for us; we just
    // hand it J0 directly with the last 4 bytes already incremented.
    let mut counter_init = j0;
    incr_u32_be_lsb(&mut counter_init);
    let mut plaintext = ciphertext.to_vec();
    let mut ctr = AesCtr::new(
        GenericArray::from_slice(key),
        GenericArray::from_slice(&counter_init),
    );
    ctr.apply_keystream(&mut plaintext);

    // GCM auth tag:
    //   S = GHASH(H, AAD || 0^pad_a || C || 0^pad_c || len(AAD)_64_BE || len(C)_64_BE)
    //   T = S XOR E_K(J0)
    let mut ghash = GHash::new(GenericArray::from_slice(&h));
    update_padded(&mut ghash, aad);
    update_padded(&mut ghash, ciphertext);
    let mut len_block = [0u8; 16];
    len_block[..8].copy_from_slice(&((aad.len() as u64) * 8).to_be_bytes());
    len_block[8..].copy_from_slice(&((ciphertext.len() as u64) * 8).to_be_bytes());
    ghash.update(&[GenericArray::clone_from_slice(&len_block)]);
    let s = ghash.finalize();

    let mut tag_block = j0;
    aes.encrypt_block(GenericArray::from_mut_slice(&mut tag_block));
    let mut tag_calc = [0u8; 16];
    for i in 0..16 {
        tag_calc[i] = s[i] ^ tag_block[i];
    }

    if tag_calc.ct_eq(tag_in).into() {
        Ok(plaintext)
    } else {
        Err(DecryptError::AuthFailed)
    }
}

/// GHASH subkey H = E_K(0^128). Both encrypt and decrypt need this;
/// exposing it here lets the encrypt path in `write.rs` reuse it
/// instead of recomputing.
pub(crate) fn compute_h(aes: &Aes256) -> [u8; 16] {
    let mut h = [0u8; 16];
    aes.encrypt_block(GenericArray::from_mut_slice(&mut h));
    h
}

/// J0 per NIST SP 800-38D §7.1: when `len(IV) != 96` bits,
/// `J0 = GHASH(H, IV || 0^s || len(IV)_bits_64_BE)` where `s` zero-pads
/// `IV` to a 128-bit boundary and an extra 64 bits.
pub(crate) fn compute_j0(h: &[u8; 16], iv: &[u8]) -> [u8; 16] {
    let mut ghash = GHash::new(GenericArray::from_slice(h));
    // Pad IV to a multiple of 16 bytes.
    update_padded(&mut ghash, iv);
    // Final block: 64 zero bits followed by len(IV) in bits as 64-bit big-endian.
    let mut tail = [0u8; 16];
    tail[8..].copy_from_slice(&((iv.len() as u64) * 8).to_be_bytes());
    ghash.update(&[GenericArray::clone_from_slice(&tail)]);
    let out = ghash.finalize();
    let mut j0 = [0u8; 16];
    j0.copy_from_slice(&out);
    j0
}

/// Update GHASH with `data`, zero-padding the final partial block out to
/// 16 bytes. Empty input contributes nothing.
fn update_padded(ghash: &mut GHash, data: &[u8]) {
    let full_blocks = data.len() / 16;
    for chunk in data[..full_blocks * 16].chunks_exact(16) {
        ghash.update(&[GenericArray::clone_from_slice(chunk)]);
    }
    let rem = &data[full_blocks * 16..];
    if !rem.is_empty() {
        let mut last = [0u8; 16];
        last[..rem.len()].copy_from_slice(rem);
        ghash.update(&[GenericArray::clone_from_slice(&last)]);
    }
}

/// Increment the low 32 bits of a 128-bit big-endian counter block,
/// wrapping. The high 96 bits stay fixed.
fn incr_u32_be_lsb(block: &mut [u8; 16]) {
    let c = u32::from_be_bytes([block[12], block[13], block[14], block[15]]);
    let c = c.wrapping_add(1);
    block[12..].copy_from_slice(&c.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NIST SP 800-38D Appendix B, Test Case 16 (AES-256-GCM with 16-byte IV
    /// — non-default nonce size to exercise the GHASH-based J0 derivation).
    #[test]
    fn nist_test_case_16() {
        // Key, IV, P, A, C, T from the official "GCM Test Vectors" PDF,
        // Test Case 16 (the 256-bit-key, non-96-bit IV one).
        let key =
            hex_to_bytes::<32>("feffe9928665731c6d6a8f9467308308feffe9928665731c6d6a8f9467308308");
        let iv = hex_to_bytes::<60>(
            "9313225df88406e555909c5aff5269aa6a7a9538534f7da1e4c303d2a318a728c3c0c95156809539fcf0e2429a6b525416aedbf5a0de6a57a637b39b",
        );
        let plaintext = hex_to_vec(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b39",
        );
        let aad = hex_to_vec("feedfacedeadbeeffeedfacedeadbeefabaddad2");
        let expected_ct = hex_to_vec(
            "5a8def2f0c9e53f1f75d7853659e2a20eeb2b22aafde6419a058ab4f6f746bf40fc0c3b780f244452da3ebf1c5d82cdea2418997200ef82e44ae7e3f",
        );
        let expected_tag = hex_to_bytes::<16>("a44a8266ee1c8eb0c8b5d4cf5ae9f19a");

        // We only have decrypt; encrypt-then-decrypt round-trip is implicit
        // here because we feed (expected_ct || expected_tag) as the input.
        // Since the test vector mandates that this specific (key, iv, aad,
        // ciphertext, tag) verifies and decrypts to plaintext, success is
        // a stronger check than a round-trip.
        let mut ct_with_tag = expected_ct.clone();
        ct_with_tag.extend_from_slice(&expected_tag);

        // The IV in this test vector is 60 bytes, not 16 — exercises the
        // non-12-byte path. We can't pass it as `&[u8; 16]`, so we test
        // compute_j0 + cipher composition directly.
        let aes = Aes256::new(GenericArray::from_slice(&key));
        let mut h = [0u8; 16];
        aes.encrypt_block(GenericArray::from_mut_slice(&mut h));
        let j0 = compute_j0(&h, &iv);

        let mut counter_init = j0;
        incr_u32_be_lsb(&mut counter_init);
        let mut pt = expected_ct.clone();
        let mut ctr = AesCtr::new(
            GenericArray::from_slice(&key),
            GenericArray::from_slice(&counter_init),
        );
        ctr.apply_keystream(&mut pt);
        assert_eq!(pt, plaintext, "plaintext mismatch (J0 derivation wrong)");

        // Verify tag too.
        let mut g = GHash::new(GenericArray::from_slice(&h));
        update_padded(&mut g, &aad);
        update_padded(&mut g, &expected_ct);
        let mut len_block = [0u8; 16];
        len_block[..8].copy_from_slice(&((aad.len() as u64) * 8).to_be_bytes());
        len_block[8..].copy_from_slice(&((expected_ct.len() as u64) * 8).to_be_bytes());
        g.update(&[GenericArray::clone_from_slice(&len_block)]);
        let s = g.finalize();

        let mut tag_block = j0;
        aes.encrypt_block(GenericArray::from_mut_slice(&mut tag_block));
        let mut tag_calc = [0u8; 16];
        for i in 0..16 {
            tag_calc[i] = s[i] ^ tag_block[i];
        }
        assert_eq!(tag_calc, expected_tag, "tag mismatch");
    }

    /// 16-byte IV round-trip: encrypt with our primitives (so we can also
    /// test the inverse path), then decrypt. WhatsApp's case.
    #[test]
    fn round_trip_16_byte_iv() {
        let key = [0x42u8; 32];
        let iv = [0x99u8; 16];

        // Hand-encrypt: AES-CTR with j0+1 as initial counter, then compute
        // the tag the same way decrypt does.
        let plaintext = b"hello whatsapp crypt15 backup test vector".to_vec();
        let aes = Aes256::new(GenericArray::from_slice(&key));
        let mut h = [0u8; 16];
        aes.encrypt_block(GenericArray::from_mut_slice(&mut h));
        let j0 = compute_j0(&h, &iv);

        let mut counter_init = j0;
        incr_u32_be_lsb(&mut counter_init);
        let mut ct = plaintext.clone();
        let mut ctr = AesCtr::new(
            GenericArray::from_slice(&key),
            GenericArray::from_slice(&counter_init),
        );
        ctr.apply_keystream(&mut ct);

        let mut g = GHash::new(GenericArray::from_slice(&h));
        update_padded(&mut g, &ct);
        let mut len_block = [0u8; 16];
        len_block[8..].copy_from_slice(&((ct.len() as u64) * 8).to_be_bytes());
        g.update(&[GenericArray::clone_from_slice(&len_block)]);
        let s = g.finalize();

        let mut tag_block = j0;
        aes.encrypt_block(GenericArray::from_mut_slice(&mut tag_block));
        let mut tag = [0u8; 16];
        for i in 0..16 {
            tag[i] = s[i] ^ tag_block[i];
        }

        let mut ct_tag = ct;
        ct_tag.extend_from_slice(&tag);

        let recovered = decrypt_crypt15(&ct_tag, &iv, &key, b"").unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn wrong_key_fails() {
        let key = [0x42u8; 32];
        let wrong_key = [0x43u8; 32];
        let iv = [0x99u8; 16];

        // Encrypt with `key`.
        let pt = b"plaintext".to_vec();
        let aes = Aes256::new(GenericArray::from_slice(&key));
        let mut h = [0u8; 16];
        aes.encrypt_block(GenericArray::from_mut_slice(&mut h));
        let j0 = compute_j0(&h, &iv);
        let mut counter_init = j0;
        incr_u32_be_lsb(&mut counter_init);
        let mut ct = pt.clone();
        let mut ctr = AesCtr::new(
            GenericArray::from_slice(&key),
            GenericArray::from_slice(&counter_init),
        );
        ctr.apply_keystream(&mut ct);
        let mut g = GHash::new(GenericArray::from_slice(&h));
        update_padded(&mut g, &ct);
        let mut len_block = [0u8; 16];
        len_block[8..].copy_from_slice(&((ct.len() as u64) * 8).to_be_bytes());
        g.update(&[GenericArray::clone_from_slice(&len_block)]);
        let s = g.finalize();
        let mut tag_block = j0;
        aes.encrypt_block(GenericArray::from_mut_slice(&mut tag_block));
        let mut tag = [0u8; 16];
        for i in 0..16 {
            tag[i] = s[i] ^ tag_block[i];
        }
        let mut ct_tag = ct;
        ct_tag.extend_from_slice(&tag);

        // Decrypt with `wrong_key` — must fail with AuthFailed.
        assert!(matches!(
            decrypt_crypt15(&ct_tag, &iv, &wrong_key, b""),
            Err(DecryptError::AuthFailed)
        ));
    }

    fn hex_to_bytes<const N: usize>(s: &str) -> [u8; N] {
        let v = hex_to_vec(s);
        let mut out = [0u8; N];
        out.copy_from_slice(&v);
        out
    }
    fn hex_to_vec(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }
}
