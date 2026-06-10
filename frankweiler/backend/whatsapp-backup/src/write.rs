//! Encrypt path — inverse of [`crate::decrypt_file`]. Builds a complete
//! crypt15 file (header + ciphertext + GCM tag + MD5 footer) from
//! plaintext SQLite bytes.
//!
//! Used by `whatsapp_make_fixture` to generate the TNG-themed test
//! backup at build time. Not exercised in production decrypt paths —
//! the WhatsApp Android client is what produces real backup files.
//!
//! Determinism is the load-bearing requirement here: the genrule
//! emitting the encrypted backup is cached under Bazel, so the same
//! (root_key, plaintext, iv) inputs must produce byte-identical
//! output across runs. Caller supplies the IV; the writer never
//! generates randomness.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit, KeyIvInit, StreamCipher};
use aes::Aes256;
use ghash::universal_hash::UniversalHash;
use ghash::GHash;
use md5::{Digest, Md5};

use crate::crypto::{compute_h, compute_j0};
use crate::derive_backup_encryption_key;

/// Assemble a complete crypt15 file (single-file backup, with trailing
/// MD5 checksum) from `plaintext_sqlite` bytes.
///
/// `iv` must be exactly 16 bytes; caller picks it. For Bazel-cached
/// fixtures use a fixed value (e.g. all zeros). `root_key` is the
/// 32-byte WhatsApp root key (the value normally hex-encoded in
/// `WHATSAPP_BACKUP_DECRYPTION_KEY`). The function derives the AES
/// key from it the same way decrypt does.
///
/// The plaintext is zlib-deflate-compressed before encryption — the
/// real WhatsApp client does the same, and `decrypt_file` rebuilds
/// the SQLite by inflating the post-GCM bytes.
pub fn encrypt_to_crypt15(
    plaintext_sqlite: &[u8],
    root_key: &[u8; 32],
    iv: &[u8; 16],
) -> anyhow::Result<Vec<u8>> {
    use std::io::Write;

    // 1. Deflate the SQLite bytes.
    let mut deflated = Vec::with_capacity(plaintext_sqlite.len() / 2);
    let mut encoder =
        flate2::write::ZlibEncoder::new(&mut deflated, flate2::Compression::default());
    encoder.write_all(plaintext_sqlite)?;
    encoder.finish()?;

    // 2. Derive the AES key from the root key.
    let aes_key = derive_backup_encryption_key(root_key);

    // 3. AES-256-GCM encrypt with the NIST-J0 derivation matching
    //    decrypt. Reuses `compute_h` / `compute_j0` from crypto.rs so
    //    encrypt and decrypt stay in lockstep.
    let aes = Aes256::new(GenericArray::from_slice(&aes_key));
    let h = compute_h(&aes);
    let j0 = compute_j0(&h, iv);

    let mut ciphertext = deflated.clone();
    let mut counter_init = j0;
    incr_u32_be_lsb(&mut counter_init);
    let mut ctr = ctr::Ctr32BE::<Aes256>::new(
        GenericArray::from_slice(&aes_key),
        GenericArray::from_slice(&counter_init),
    );
    ctr.apply_keystream(&mut ciphertext);

    // 4. Compute GCM auth tag with empty AAD.
    let mut g = GHash::new(GenericArray::from_slice(&h));
    let full_blocks = ciphertext.len() / 16;
    for chunk in ciphertext[..full_blocks * 16].chunks_exact(16) {
        g.update(&[GenericArray::clone_from_slice(chunk)]);
    }
    if ciphertext.len() % 16 != 0 {
        let mut last = [0u8; 16];
        let tail = &ciphertext[full_blocks * 16..];
        last[..tail.len()].copy_from_slice(tail);
        g.update(&[GenericArray::clone_from_slice(&last)]);
    }
    let mut len_block = [0u8; 16];
    // AAD bit length = 0; ciphertext bit length follows.
    len_block[8..].copy_from_slice(&((ciphertext.len() as u64) * 8).to_be_bytes());
    g.update(&[GenericArray::clone_from_slice(&len_block)]);
    let s = g.finalize();

    let mut tag_block = j0;
    aes.encrypt_block(GenericArray::from_mut_slice(&mut tag_block));
    let mut tag = [0u8; 16];
    for i in 0..16 {
        tag[i] = s[i] ^ tag_block[i];
    }

    // 5. Build the BackupPrefix protobuf carrying the IV.
    let proto = build_backup_prefix(iv);
    assert!(
        proto.len() <= u8::MAX as usize,
        "protobuf too long for crypt15 framing"
    );

    // 6. Concatenate framing + ciphertext + GCM tag.
    let mut out = Vec::with_capacity(2 + proto.len() + ciphertext.len() + 32);
    out.push(proto.len() as u8); // single-byte size
    out.push(0x01); // msgstore features flag
    out.extend_from_slice(&proto);
    out.extend_from_slice(&ciphertext);
    out.extend_from_slice(&tag);

    // 7. Trailing MD5 over everything written so far (the file-format
    //    integrity check; not cryptographic, but
    //    `wa-crypt-tools.wadecrypt` rejects files where this doesn't
    //    match, so we have to produce it).
    let mut md = Md5::new();
    md.update(&out);
    let checksum = md.finalize();
    out.extend_from_slice(&checksum);

    Ok(out)
}

/// Build the smallest BackupPrefix that decrypts:
///   field 1 (key_type)      = 1                       (`Key_Type.HSM_CONTROLLED`)
///   field 3 (c15_iv submsg) = { field 1 (iv) = 16 bytes }
///
/// Real WhatsApp backups also carry `info` (field 4: app version,
/// jid suffix, feature flags). The decrypt path ignores all of that
/// — only the IV in field 3 is load-bearing — so this minimal proto
/// is enough for fixtures.
fn build_backup_prefix(iv: &[u8; 16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    // field 1, wire type 0 (varint), value 1
    out.push(0x08);
    out.push(0x01);
    // field 3, wire type 2 (length-delimited)
    out.push(0x1a);
    // submessage length = tag(1) + length(1) + iv(16) = 18
    out.push(18);
    // sub field 1, wire type 2, length 16
    out.push(0x0a);
    out.push(16);
    out.extend_from_slice(iv);
    out
}

fn incr_u32_be_lsb(block: &mut [u8; 16]) {
    let c = u32::from_be_bytes([block[12], block[13], block[14], block[15]]);
    let c = c.wrapping_add(1);
    block[12..].copy_from_slice(&c.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decrypt_file;

    /// Encrypt-then-decrypt round-trip with the all-zeros root key
    /// — the fixture key. Proves the encrypt path inverts cleanly.
    #[test]
    fn round_trip_all_zeros_key() {
        let plaintext = b"SQLite format 3\0... (pretend this is a tiny sqlite file) ...".repeat(20);
        let root_key = [0u8; 32];
        let iv = [0xa5u8; 16]; // arbitrary fixed test IV
        let encrypted = encrypt_to_crypt15(&plaintext, &root_key, &iv).unwrap();

        // Re-use `decrypt_file` by writing to disk.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), &encrypted).unwrap();
        let recovered = decrypt_file(tmp.path(), &root_key).unwrap();
        assert_eq!(recovered, plaintext);
    }
}
