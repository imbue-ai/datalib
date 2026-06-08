//! Write side of the Signal backup container — exists so we can
//! generate test fixtures with known-good crypto without baking a
//! special "unencrypted" mode into the reader. The fixture's AEP is
//! published right alongside the fixture (`"0".repeat(64)`); every
//! crypto step runs exactly the way it would against a real backup.
//!
//! Reverses [`crate::crypto`]:
//!
//! * `metadata` — build `local::Metadata` with a fresh `backup_id`,
//!   AES-CTR-encrypt it under the metadata key, prost-encode.
//! * `main` — length-delimit `BackupInfo` + each `Frame`, gzip, AES-CBC
//!   encrypt (PKCS7 pad), HMAC-SHA256 trailer.
//! * `files` — length-delimited `local::FilesFrame` per media name.
//!
//! IVs and `backup_id` are passed in by the caller (not generated
//! here) so the fixture is byte-deterministic. Pass any fixed
//! 16-byte / 12-byte value you like; the test fixture passes zeros.

use std::path::Path;

use aes::cipher::{generic_array::GenericArray, BlockEncryptMut, KeyIvInit, StreamCipher};
use aes::Aes256;
use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use prost::Message;
use sha2::Sha256;

use crate::crypto::{derive_backup_key, derive_message_keys, normalize_passphrase};
use crate::local;
use crate::{backup, hex_lower};

type HmacSha256 = Hmac<Sha256>;
type Aes256CbcEnc = cbc::Encryptor<Aes256>;
type Aes256Ctr = ctr::Ctr128BE<Aes256>;

const IV_LENGTH: usize = 16;

/// Inputs needed to produce one snapshot dir. `backup_id` and the two
/// IVs are passed in (not generated) for fixture determinism.
pub struct SnapshotInput<'a> {
    /// Account Entropy Pool. Normalized before key derivation.
    pub aep: &'a str,
    /// 16-byte backup id — opaque random material that's encrypted
    /// into the `metadata` envelope and fed into the HKDF info for
    /// the main-blob keys.
    pub backup_id: &'a [u8; 16],
    /// 12-byte IV used in the metadata AES-CTR step (padded to 16 with
    /// four zero bytes — matches the reader's expectation).
    pub metadata_iv: &'a [u8; 12],
    /// 16-byte IV used in the main-blob AES-CBC step.
    pub main_iv: &'a [u8; 16],
    /// `BackupInfo` to prepend to the frame stream. Signal-Android
    /// always emits one of these as the first length-delimited
    /// record; emit whatever you like (or `BackupInfo::default()`).
    pub backup_info: backup::BackupInfo,
    /// Frames to emit after `backup_info`, in order.
    pub frames: &'a [backup::Frame],
    /// Media filenames for the `files` sidecar. Empty Vec is fine —
    /// fixture media decryption isn't exercised in the round-trip
    /// test, but the sidecar still has to be present.
    pub file_names: &'a [String],
}

/// Write the three files (`metadata`, `main`, `files`) into `out_dir`.
/// `out_dir` is created if missing.
pub fn write_snapshot(out_dir: &Path, input: &SnapshotInput<'_>) -> Result<()> {
    std::fs::create_dir_all(out_dir)
        .map_err(|e| anyhow!("create snapshot dir {}: {e}", out_dir.display()))?;
    let passphrase = normalize_passphrase(input.aep)?;
    let backup_key = derive_backup_key(&passphrase);

    let metadata_bytes = encode_metadata(&backup_key, input.backup_id, input.metadata_iv)?;
    std::fs::write(out_dir.join("metadata"), &metadata_bytes)
        .map_err(|e| anyhow!("write metadata: {e}"))?;

    let (hmac_key, aes_key) = derive_message_keys(&backup_key, input.backup_id);
    let main_bytes = encode_main(
        &hmac_key,
        &aes_key,
        input.main_iv,
        &input.backup_info,
        input.frames,
    )?;
    std::fs::write(out_dir.join("main"), &main_bytes).map_err(|e| anyhow!("write main: {e}"))?;

    std::fs::write(
        out_dir.join("files"),
        encode_files_sidecar(input.file_names),
    )
    .map_err(|e| anyhow!("write files: {e}"))?;
    Ok(())
}

fn encode_metadata(
    backup_key: &[u8; 32],
    backup_id: &[u8; 16],
    iv12: &[u8; 12],
) -> Result<Vec<u8>> {
    // Pad IV → 16-byte initial CTR block: iv || 0000.
    let mut counter = [0u8; 16];
    counter[..12].copy_from_slice(iv12);
    let meta_key = derive_metadata_key(backup_key);
    let mut encrypted_id = backup_id.to_vec();
    Aes256Ctr::new(
        GenericArray::from_slice(&meta_key),
        GenericArray::from_slice(&counter),
    )
    .apply_keystream(&mut encrypted_id);

    let metadata = local::Metadata {
        version: 1,
        backup_id: Some(local::metadata::EncryptedBackupId {
            iv: iv12.to_vec(),
            encrypted_id,
        }),
    };
    Ok(metadata.encode_to_vec())
}

fn derive_metadata_key(backup_key: &[u8; 32]) -> [u8; 32] {
    use hkdf::Hkdf;
    let hk = Hkdf::<Sha256>::new(None, backup_key);
    let mut out = [0u8; 32];
    hk.expand(crate::crypto::LOCAL_BACKUP_METADATA_INFO, &mut out)
        .expect("hkdf len ok");
    out
}

fn encode_main(
    hmac_key: &[u8; 32],
    aes_key: &[u8; 32],
    iv: &[u8; 16],
    backup_info: &backup::BackupInfo,
    frames: &[backup::Frame],
) -> Result<Vec<u8>> {
    // Length-delimited stream: BackupInfo first, then each Frame.
    let mut buf: Vec<u8> = Vec::new();
    encode_one_delimited(&mut buf, backup_info);
    for f in frames {
        encode_one_delimited(&mut buf, f);
    }

    let compressed = gzip(&buf)?;

    // AES-CBC encrypt with PKCS7 padding. The `cipher` crate's
    // `encrypt_padded_vec_mut` allocates and pads internally; we
    // then prefix the IV and append the HMAC.
    let pt = compressed;
    let pad_len = 16 - (pt.len() % 16);
    let mut buf = Vec::with_capacity(pt.len() + pad_len);
    buf.extend_from_slice(&pt);
    buf.resize(pt.len() + pad_len, 0);
    let ct_len = Aes256CbcEnc::new(
        GenericArray::from_slice(aes_key),
        GenericArray::from_slice(iv),
    )
    .encrypt_padded_mut::<cipher::block_padding::Pkcs7>(&mut buf, pt.len())
    .map_err(|e| anyhow!("CBC encrypt failed: {e}"))?
    .len();
    buf.truncate(ct_len);

    let mut body = Vec::with_capacity(IV_LENGTH + buf.len());
    body.extend_from_slice(iv);
    body.extend_from_slice(&buf);

    let mut mac = <HmacSha256 as Mac>::new_from_slice(hmac_key).expect("hmac key length");
    mac.update(&body);
    let tag = mac.finalize().into_bytes();
    body.extend_from_slice(&tag);
    Ok(body)
}

fn encode_one_delimited(buf: &mut Vec<u8>, msg: &impl Message) {
    let len = msg.encoded_len();
    write_varint(buf, len as u64);
    msg.encode(buf).expect("prost encode ok");
}

fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn gzip(input: &[u8]) -> Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(input)
        .map_err(|e| anyhow!("gzip write: {e}"))?;
    enc.finish().map_err(|e| anyhow!("gzip finish: {e}"))
}

fn encode_files_sidecar(names: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for name in names {
        let frame = local::FilesFrame {
            item: Some(local::files_frame::Item::MediaName(name.clone())),
        };
        encode_one_delimited(&mut out, &frame);
    }
    out
}

/// Suppress "unused" complaint for the hex helper when only `write`
/// is consumed (e.g. by the fixture binary).
#[allow(dead_code)]
fn _keep_hex_alive() -> String {
    hex_lower(&[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Snapshot;
    use tempfile::tempdir;

    #[test]
    fn roundtrip_through_snapshot_open() {
        let dir = tempdir().unwrap();
        let aep: String = "0".repeat(64);
        let mut recipients = Vec::new();
        for id in 1u64..=3 {
            recipients.push(backup::Frame {
                item: Some(backup::frame::Item::Recipient(backup::Recipient {
                    id,
                    destination: Some(backup::recipient::Destination::Contact(backup::Contact {
                        e164: Some(10000 + id),
                        profile_given_name: Some(format!("User{id}")),
                        ..Default::default()
                    })),
                })),
            });
        }
        let backup_info = backup::BackupInfo {
            version: 1,
            backup_time_ms: 1_700_000_000_000,
            ..Default::default()
        };
        write_snapshot(
            dir.path(),
            &SnapshotInput {
                aep: &aep,
                backup_id: &[7u8; 16],
                metadata_iv: &[0u8; 12],
                main_iv: &[0u8; 16],
                backup_info,
                frames: &recipients,
                file_names: &[],
            },
        )
        .unwrap();

        let snap = Snapshot::open(dir.path(), &aep).unwrap();
        let frames: Vec<_> = snap.frames().map(|r| r.unwrap()).collect();
        assert_eq!(frames.len(), 3);
        let mut ids: Vec<u64> = frames
            .iter()
            .filter_map(|f| match &f.item {
                Some(backup::frame::Item::Recipient(r)) => Some(r.id),
                _ => None,
            })
            .collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2, 3]);
    }
}
