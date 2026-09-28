//! Encrypt path — inverse of [`crate::decrypt_file`]. Builds a complete
//! crypt15 file (header + ciphertext + GCM tag + MD5 footer) from
//! plaintext SQLite bytes.

use md5::{Digest, Md5};

use crate::crypto::encrypt_gcm;
use crate::derive_backup_encryption_key;

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

    // 3. AES-256-GCM encrypt, and 4. its auth tag with empty AAD.
    let (ciphertext, tag) = encrypt_gcm(&aes_key, iv, &deflated);

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
