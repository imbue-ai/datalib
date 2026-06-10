//! `frankweiler-whatsapp-backup` — decrypt WhatsApp-Android crypt15
//! `msgstore.db.crypt15` backups into plaintext SQLite bytes.
//!
//! Scope is intentionally narrow: the base `msgstore.db.crypt15` file only.
//! No crypt12/14 support, no increment-file replay
//! (`msgstore-increment-N.db.crypt15`), no wa.db contacts DB — those
//! are separate format variants and the upstream reverse-engineering
//! tool (`wa-crypt-tools`) covers them if we need them later.
//!
//! Crypt15 format (base file):
//!
//! ```text
//! [ 1 byte  ] protobuf_size (big-endian u8 — yes, just one byte; max 255)
//! [ 0..1 B  ] optional msgstore-features flag: present iff next byte == 0x01
//! [ N bytes ] BackupPrefix protobuf (length = protobuf_size)
//!     field 1 (varint)             — key_type
//!     field 2 (length-delimited)   — submessage with the IV (c15_iv):
//!         field 1 (length-delimited) — 16-byte IV (GCM nonce)
//!     field 3 (length-delimited)   — version_info / device info (ignored)
//!     field 5 (length-delimited)   — feature flags (ignored)
//! [ M bytes ] AES-256-GCM ciphertext (zlib-deflated SQLite)
//! [ 16 bytes ] GCM auth tag
//! [ 16 bytes ] MD5 checksum over (size_prefix || features_flag || protobuf
//!              || ciphertext || tag). Optional integrity check; not
//!              cryptographic. Trailing on single-file backups only;
//!              omitted in multifile / incremental backups.
//! ```
//!
//! Decryption: AES-256-GCM with the raw 32-byte key (as a hex string in
//! `WHATSAPP_BACKUP_DECRYPTION_KEY`) and the 16-byte IV from the header.
//! Output is a zlib-compressed stream; inflate to get the SQLite bytes.
//!
//! The key is passed in by the caller — this crate never reads env or
//! files holding the key.

mod crypto;
mod header;
mod key;

pub use crypto::{decrypt_crypt15, DecryptError};
pub use header::{parse_header, BackupHeader, HeaderError};
pub use key::derive_backup_encryption_key;

use std::path::Path;

use anyhow::{Context, Result};

/// Decrypt the crypt15 file at `path` using `root_key` (32 raw bytes
/// from `encrypted_backup.key` or its hex representation in
/// `WHATSAPP_BACKUP_DECRYPTION_KEY`) and return the plaintext SQLite
/// bytes (after zlib inflate).
///
/// The root key is *not* the AES key — WhatsApp derives the AES-256
/// GCM key from it via a one-shot HMAC-SHA256 KDF
/// ([`derive_backup_encryption_key`]). This function applies that
/// derivation internally.
pub fn decrypt_file(path: &Path, root_key: &[u8; 32]) -> Result<Vec<u8>> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let header = parse_header(&bytes).context("parse crypt15 header")?;
    // Trailing 16 bytes are an MD5 file checksum; the 16 bytes before
    // those are the AES-256-GCM auth tag. Pass (ciphertext || GCM tag)
    // to the AEAD layer; ignore the MD5 here (caller can verify
    // separately if they want — it's a non-cryptographic integrity
    // check anyway).
    if bytes.len() < header.body_offset + 32 {
        anyhow::bail!(
            "file too short: header ends at {} but file is only {} bytes (need at least 32 trailing bytes for GCM tag + MD5)",
            header.body_offset,
            bytes.len()
        );
    }
    let ciphertext_with_tag = &bytes[header.body_offset..bytes.len() - 16];
    let aes_key = derive_backup_encryption_key(root_key);
    let decrypted = decrypt_crypt15(ciphertext_with_tag, &header.iv, &aes_key, &[])
        .context("AES-256-GCM decrypt")?;
    let mut sqlite_bytes = Vec::with_capacity(decrypted.len() * 4);
    let mut decoder = flate2::read::ZlibDecoder::new(&decrypted[..]);
    std::io::Read::read_to_end(&mut decoder, &mut sqlite_bytes).context("zlib inflate")?;
    Ok(sqlite_bytes)
}

/// Decode `hex_key` (64 hex chars, optional whitespace) into a 32-byte
/// AES-256 key. Intended for callers reading the key out of the
/// `WHATSAPP_BACKUP_DECRYPTION_KEY` env var; rejects anything else.
pub fn decode_hex_key(hex_key: &str) -> Result<[u8; 32]> {
    let trimmed: String = hex_key.chars().filter(|c| !c.is_whitespace()).collect();
    if trimmed.len() != 64 {
        anyhow::bail!(
            "expected 64 hex chars for 32-byte key, got {} chars after trim",
            trimmed.len()
        );
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let lo = trimmed.as_bytes()[i * 2];
        let hi = trimmed.as_bytes()[i * 2 + 1];
        *byte = (hex_nibble(lo)? << 4) | hex_nibble(hi)?;
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => anyhow::bail!("non-hex char 0x{:02x} in key", c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let k = decode_hex_key("0011223344556677889900112233445566778899001122334455667788990011")
            .unwrap();
        assert_eq!(k[0], 0x00);
        assert_eq!(k[1], 0x11);
        assert_eq!(k[31], 0x11);
    }

    #[test]
    fn hex_rejects_short() {
        assert!(decode_hex_key("00").is_err());
    }

    #[test]
    fn hex_rejects_non_hex() {
        let bad = "ZZ".to_string() + &"0".repeat(62);
        assert!(decode_hex_key(&bad).is_err());
    }
}
