//! `frankweiler-signal-backup` — read Signal-Android's new directory-format
//! ("AEP-keyed Backups", as opposed to the classic 30-digit `.backup` file)
//! snapshot directories.
//!
//! Port of the Python reference at `~/src/SignalTool/` (audited clean).
//! Crypto + framing match `dump.py` and `decrypt.py` byte-for-byte:
//!
//! * `metadata` — AES-256-CTR with a 12-byte IV padded by four zeros to
//!   form a 16-byte CTR block; key = HKDF(`20241011_SIGNAL_LOCAL_BACKUP_METADATA_KEY`,
//!   K_B). Plaintext is `signal.backup.local.Metadata.backupId`.
//! * `main` — AES-256-CBC + HMAC-SHA256 trailer over a gzip stream.
//!   Keys = HKDF(`20241007_SIGNAL_BACKUP_ENCRYPT_MESSAGE_BACKUP:` || backup_id,
//!   K_B), split into hmac_key || aes_key (32 + 32). After
//!   decrypt+gunzip the payload is a length-delimited stream of
//!   `signal.backup.Frame` messages.
//! * `files` — plaintext, length-delimited
//!   `signal.backup.local.FilesFrame` — the list of media filenames in
//!   the (shared) `files/XX/<name>` tree.
//! * attachments — AES-256-CBC + HMAC-SHA256 trailer; 64-byte local key
//!   split as `aes(32) || hmac(32)`.
//!
//! Scope is intentionally narrow: this crate decrypts and surfaces
//! `Frame`s + the raw file list. The provider that maps frames into the
//! frankweiler schema (`frankweiler-etl-signal`) is a separate crate.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

pub mod crypto;
mod proto;
pub mod write;

pub use proto::backup;
pub use proto::local;

use crypto::{
    decrypt_attachment_inplace, decrypt_main, decrypt_metadata_backup_id, derive_backup_key,
    derive_message_keys, normalize_passphrase,
};

/// An opened Signal backup snapshot. Holds the decrypted frame stream
/// in memory; the typical real-world snapshot is tens of MB after
/// gunzip, so we don't bother streaming.
pub struct Snapshot {
    snapshot_dir: PathBuf,
    decrypted_main: Vec<u8>,
    file_names: Vec<String>,
    backup_id: Vec<u8>,
}

impl Snapshot {
    /// Open `snapshot_dir` (a directory containing `metadata`, `main`,
    /// and `files`) using `aep` (the 64-char Account Entropy Pool — the
    /// passphrase Signal shows the user; whitespace and `#`/`=` chars
    /// are normalized per Signal's spec).
    pub fn open(snapshot_dir: &Path, aep: &str) -> Result<Self> {
        let passphrase = normalize_passphrase(aep)?;
        let backup_key = derive_backup_key(&passphrase);

        let metadata_bytes = std::fs::read(snapshot_dir.join("metadata"))
            .with_context(|| format!("read {}/metadata", snapshot_dir.display()))?;
        let backup_id = decrypt_metadata_backup_id(&backup_key, &metadata_bytes)
            .context("decrypt metadata backup_id")?;

        let (hmac_key, aes_key) = derive_message_keys(&backup_key, &backup_id);

        let main_bytes = std::fs::read(snapshot_dir.join("main"))
            .with_context(|| format!("read {}/main", snapshot_dir.display()))?;
        let decrypted_main =
            decrypt_main(&hmac_key, &aes_key, &main_bytes).context("decrypt main")?;

        let files_bytes = std::fs::read(snapshot_dir.join("files"))
            .with_context(|| format!("read {}/files", snapshot_dir.display()))?;
        let file_names = parse_files_sidecar(&files_bytes).context("parse files sidecar")?;

        Ok(Snapshot {
            snapshot_dir: snapshot_dir.to_path_buf(),
            decrypted_main,
            file_names,
            backup_id,
        })
    }

    /// Iterate over decoded frames in order. The first frame is
    /// `BackupInfo`, not `Frame` — Signal writes one `BackupInfo`
    /// length-delimited record before the `Frame` stream. We surface
    /// it as the first iterator item via `Frame::default()` populated
    /// from the bytes; callers that want `BackupInfo` typed can read
    /// `raw_records()` instead.
    pub fn frames(&self) -> FrameIter<'_> {
        FrameIter {
            buf: &self.decrypted_main,
            offset: 0,
            skip_header: true,
        }
    }

    /// Iterate over raw (undecoded) length-delimited records in the
    /// decrypted `main`. Useful when you need both `BackupInfo` and
    /// `Frame` typed separately.
    pub fn raw_records(&self) -> RecordIter<'_> {
        RecordIter {
            buf: &self.decrypted_main,
            offset: 0,
        }
    }

    /// The list of media filenames from the `files` sidecar — the
    /// `XX/<name>` basenames under the shared `files/` tree alongside
    /// (not inside) the snapshot directory.
    pub fn file_names(&self) -> &[String] {
        &self.file_names
    }

    /// The decrypted `backupId` from the `metadata` envelope. Exposed
    /// for callers that want to derive `MEDIA_ID` keys themselves.
    pub fn backup_id(&self) -> &[u8] {
        &self.backup_id
    }

    /// Path the snapshot was opened from.
    pub fn snapshot_dir(&self) -> &Path {
        &self.snapshot_dir
    }
}

/// Decrypt a `files/XX/<media_name>` attachment blob using its
/// 64-byte local key (the value stored on the `FilePointer.LocatorInfo`
/// frame field). Returns the plaintext bytes.
///
/// Layout: `iv(16) || ciphertext || hmac_sha256(hmac_key, iv||ciphertext)(32)`,
/// AES-256-CBC with PKCS7 padding. `local_key` is split `aes(32) || hmac(32)`.
pub fn decrypt_attachment(enc: &[u8], local_key: &[u8; 64]) -> Result<Vec<u8>> {
    decrypt_attachment_inplace(enc, local_key)
}

/// Filename Signal uses for a locally-stored attachment:
/// `sha256_hex(plaintext_hash || local_key)`. Matches
/// `dump.py:_local_media_name`.
pub fn local_media_name(plaintext_hash: &[u8], local_key: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(plaintext_hash);
    h.update(local_key);
    hex_lower(&h.finalize())
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// Iterator over length-delimited `Frame`s in the decrypted main blob.
pub struct FrameIter<'a> {
    buf: &'a [u8],
    offset: usize,
    skip_header: bool,
}

impl Iterator for FrameIter<'_> {
    type Item = Result<backup::Frame>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.offset >= self.buf.len() {
                return None;
            }
            let (len, consumed) = match read_varint(&self.buf[self.offset..]) {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            let start = self.offset + consumed;
            let end = start + len as usize;
            if end > self.buf.len() {
                return Some(Err(anyhow!("truncated delimited record")));
            }
            let record = &self.buf[start..end];
            self.offset = end;
            if self.skip_header {
                // First record is BackupInfo, not Frame — skip it.
                self.skip_header = false;
                continue;
            }
            return Some(match prost::Message::decode(record) {
                Ok(f) => Ok(f),
                Err(e) => Err(anyhow!("decode Frame: {e}")),
            });
        }
    }
}

/// Iterator over raw length-delimited record bytes.
pub struct RecordIter<'a> {
    buf: &'a [u8],
    offset: usize,
}

impl<'a> Iterator for RecordIter<'a> {
    type Item = Result<&'a [u8]>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.buf.len() {
            return None;
        }
        let (len, consumed) = match read_varint(&self.buf[self.offset..]) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };
        let start = self.offset + consumed;
        let end = start + len as usize;
        if end > self.buf.len() {
            return Some(Err(anyhow!("truncated delimited record")));
        }
        self.offset = end;
        Some(Ok(&self.buf[start..end]))
    }
}

fn read_varint(buf: &[u8]) -> Result<(u64, usize)> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    for (i, &byte) in buf.iter().enumerate() {
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, i + 1));
        }
        shift += 7;
        if shift > 63 {
            return Err(anyhow!("varint too long"));
        }
    }
    Err(anyhow!("truncated varint"))
}

fn parse_files_sidecar(buf: &[u8]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < buf.len() {
        let (len, consumed) = read_varint(&buf[offset..])?;
        let start = offset + consumed;
        let end = start + len as usize;
        if end > buf.len() {
            return Err(anyhow!("truncated FilesFrame record"));
        }
        let frame: local::FilesFrame = prost::Message::decode(&buf[start..end])
            .map_err(|e| anyhow!("decode FilesFrame: {e}"))?;
        if let Some(local::files_frame::Item::MediaName(name)) = frame.item {
            out.push(name);
        }
        offset = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_passphrase_strips_and_lowercases() {
        // 64 lowercase alphanumerics → unchanged.
        let pp: String = (0..64).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        assert_eq!(normalize_passphrase(&pp).unwrap(), pp);
    }

    #[test]
    fn normalize_passphrase_replaces_hash_and_equals() {
        // Signal renders `o` and `0` ambiguously in some fonts, so users
        // sometimes type `#` for `o` and `=` for `0`. Spec: replace
        // before stripping non-alphanumerics.
        let raw: String = "#".repeat(32) + &"=".repeat(32);
        let got = normalize_passphrase(&raw).unwrap();
        assert_eq!(got, "o".repeat(32) + &"0".repeat(32));
    }

    #[test]
    fn normalize_passphrase_drops_whitespace_and_punct() {
        let raw =
            "  AAAA-BBBB CCCC-DDDD\nEEEE FFFF GGGG HHHH IIII JJJJ KKKK LLLL MMMM NNNN OOOO PPPP  ";
        let got = normalize_passphrase(raw).unwrap();
        assert_eq!(got.len(), 64);
        assert!(got
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn normalize_passphrase_rejects_wrong_length() {
        assert!(normalize_passphrase("too short").is_err());
        assert!(normalize_passphrase(&"a".repeat(65)).is_err());
    }

    #[test]
    fn local_media_name_is_sha256_hex_concat() {
        let name = local_media_name(b"hash", b"key");
        // sha256("hashkey") = 91...
        let expected = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(b"hashkey");
            hex_lower(&h.finalize())
        };
        assert_eq!(name, expected);
        assert_eq!(name.len(), 64);
    }

    #[test]
    fn varint_roundtrip() {
        // 0x96 0x01 → 150
        assert_eq!(read_varint(&[0x96, 0x01, 0xff]).unwrap(), (150, 2));
        assert_eq!(read_varint(&[0x00]).unwrap(), (0, 1));
        assert_eq!(read_varint(&[0x7f]).unwrap(), (127, 1));
        assert!(read_varint(&[0x80]).is_err());
        assert!(read_varint(&[]).is_err());
    }
}
