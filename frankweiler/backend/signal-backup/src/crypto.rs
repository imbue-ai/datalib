//! Crypto primitives for the Signal backup container. All key strings
//! and modes come from `dump.py` in the Python reference — kept as
//! `&[u8]` constants so the byte-for-byte match is easy to verify.

use aes::cipher::{
    generic_array::GenericArray, BlockDecryptMut, BlockEncryptMut, KeyIvInit, StreamCipher,
};
use aes::Aes256;
use anyhow::{anyhow, Result};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::local;

/// HKDF info string for deriving the local-backup metadata key from K_B.
pub const LOCAL_BACKUP_METADATA_INFO: &[u8] = b"20241011_SIGNAL_LOCAL_BACKUP_METADATA_KEY";
/// HKDF info string for deriving K_B from the (normalized) AEP.
pub const BACKUP_KEY_INFO: &[u8] = b"20240801_SIGNAL_BACKUP_KEY";
/// HKDF info-prefix for the message-backup (main) keys — the actual
/// info passed to HKDF is `MESSAGE_BACKUP_INFO || backup_id`.
pub const MESSAGE_BACKUP_INFO: &[u8] = b"20241007_SIGNAL_BACKUP_ENCRYPT_MESSAGE_BACKUP:";
/// HKDF info-prefix for media IDs (consumed by `frankweiler-etl-signal`,
/// not by this crate; exposed for symmetry with the Python reference).
pub const MEDIA_ID_INFO: &[u8] = b"20241007_SIGNAL_BACKUP_MEDIA_ID:";

const IV_LENGTH: usize = 16;
const MAC_LENGTH: usize = 32;

type HmacSha256 = Hmac<Sha256>;
type Aes256CbcDec = cbc::Decryptor<Aes256>;
type Aes256CbcEnc = cbc::Encryptor<Aes256>;
type Aes256Ctr = ctr::Ctr128BE<Aes256>;

/// Normalize an Account Entropy Pool string per Signal's spec:
/// strip whitespace, replace `#`→`o` and `=`→`0` (font-ambiguity
/// chars), drop everything non-alphanumeric, lowercase. Must end up
/// exactly 64 chars of `[a-z0-9]`.
pub fn normalize_passphrase(text: &str) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let c = match c {
            '#' => 'o',
            '=' => '0',
            _ => c,
        };
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        }
    }
    if out.len() != 64
        || !out
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Err(anyhow!(
            "invalid passphrase: expected 64 [a-z0-9] after normalization, got {}",
            out.len()
        ));
    }
    Ok(out)
}

fn hkdf_sha256(info: &[u8], ikm: &[u8], length: usize) -> Vec<u8> {
    let hk = Hkdf::<Sha256>::new(None, ikm);
    let mut out = vec![0u8; length];
    hk.expand(info, &mut out).expect("hkdf expand length valid");
    out
}

/// K_B = HKDF(BACKUP_KEY_INFO, passphrase).
pub fn derive_backup_key(passphrase: &str) -> [u8; 32] {
    let v = hkdf_sha256(BACKUP_KEY_INFO, passphrase.as_bytes(), 32);
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    out
}

fn derive_metadata_key(backup_key: &[u8; 32]) -> [u8; 32] {
    let v = hkdf_sha256(LOCAL_BACKUP_METADATA_INFO, backup_key, 32);
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    out
}

/// Returns `(hmac_key, aes_key)` for the `main` blob.
pub fn derive_message_keys(backup_key: &[u8; 32], backup_id: &[u8]) -> ([u8; 32], [u8; 32]) {
    let mut info = Vec::with_capacity(MESSAGE_BACKUP_INFO.len() + backup_id.len());
    info.extend_from_slice(MESSAGE_BACKUP_INFO);
    info.extend_from_slice(backup_id);
    let material = hkdf_sha256(&info, backup_key, 64);
    let mut hmac_key = [0u8; 32];
    let mut aes_key = [0u8; 32];
    hmac_key.copy_from_slice(&material[..32]);
    aes_key.copy_from_slice(&material[32..]);
    (hmac_key, aes_key)
}

/// Decrypt the `metadata` envelope: parse the prost `Metadata` message,
/// HKDF a 32-byte metadata key from K_B, AES-CTR-decrypt the
/// `encryptedId`. The 12-byte IV is padded with four zero bytes to form
/// the initial 128-bit CTR block — matches pycryptodome's
/// `nonce=b'', initial_value=iv + b'\x00'*4`.
pub fn decrypt_metadata_backup_id(backup_key: &[u8; 32], metadata_bytes: &[u8]) -> Result<Vec<u8>> {
    let metadata: local::Metadata =
        prost::Message::decode(metadata_bytes).map_err(|e| anyhow!("decode Metadata: {e}"))?;
    let bid = metadata
        .backup_id
        .ok_or_else(|| anyhow!("metadata missing backupId"))?;
    if bid.iv.len() != 12 {
        return Err(anyhow!(
            "expected 12-byte metadata IV, got {}",
            bid.iv.len()
        ));
    }
    let mut counter = [0u8; 16];
    counter[..12].copy_from_slice(&bid.iv);
    // last 4 bytes already zero — matches `iv + b'\x00'*4`
    let meta_key = derive_metadata_key(backup_key);
    let mut cipher = Aes256Ctr::new(
        GenericArray::from_slice(&meta_key),
        GenericArray::from_slice(&counter),
    );
    let mut out = bid.encrypted_id.clone();
    cipher.apply_keystream(&mut out);
    Ok(out)
}

/// Decrypt + gunzip the `main` blob. Layout:
/// `body || hmac_sha256(hmac_key, body)(32)`, where
/// `body = iv(16) || aes_cbc_pkcs7(aes_key, iv, gzip(frames))`.
pub fn decrypt_main(hmac_key: &[u8; 32], aes_key: &[u8; 32], main: &[u8]) -> Result<Vec<u8>> {
    if main.len() < IV_LENGTH + MAC_LENGTH {
        return Err(anyhow!("main too short: {}", main.len()));
    }
    let (body, mac) = main.split_at(main.len() - MAC_LENGTH);
    verify_hmac(hmac_key, body, mac).map_err(|_| anyhow!("bad main MAC"))?;
    let (iv, ct) = body.split_at(IV_LENGTH);
    let mut buf = ct.to_vec();
    let pt_len = Aes256CbcDec::new(
        GenericArray::from_slice(aes_key),
        GenericArray::from_slice(iv),
    )
    .decrypt_padded_mut::<cipher::block_padding::Pkcs7>(&mut buf)
    .map_err(|e| anyhow!("CBC decrypt/unpad failed: {e}"))?
    .len();
    buf.truncate(pt_len);

    use std::io::Read;
    let mut gz = flate2::read::GzDecoder::new(&buf[..]);
    let mut out = Vec::with_capacity(buf.len() * 4);
    gz.read_to_end(&mut out)
        .map_err(|e| anyhow!("gunzip main: {e}"))?;
    Ok(out)
}

/// Decrypt one attachment blob (the `files/XX/<media_name>` content)
/// using its 64-byte local key. Layout matches `decrypt.py:_decrypt`:
/// `iv(16) || ct || hmac_sha256(hmac_key, iv||ct)(32)`. PKCS7 padded.
/// Key split: `aes(32) || hmac(32)`.
pub fn decrypt_attachment_inplace(enc: &[u8], local_key: &[u8; 64]) -> Result<Vec<u8>> {
    if enc.len() < IV_LENGTH + MAC_LENGTH {
        return Err(anyhow!("attachment too short: {}", enc.len()));
    }
    let aes_key: &[u8; 32] = local_key[..32].try_into().unwrap();
    let hmac_key: &[u8; 32] = local_key[32..].try_into().unwrap();
    let (body, mac) = enc.split_at(enc.len() - MAC_LENGTH);
    verify_hmac(hmac_key, body, mac).map_err(|_| anyhow!("bad attachment MAC"))?;
    let (iv, ct) = body.split_at(IV_LENGTH);
    let mut buf = ct.to_vec();
    let pt_len = Aes256CbcDec::new(
        GenericArray::from_slice(aes_key),
        GenericArray::from_slice(iv),
    )
    .decrypt_padded_mut::<cipher::block_padding::Pkcs7>(&mut buf)
    .map_err(|e| anyhow!("attachment CBC decrypt/unpad failed: {e}"))?
    .len();
    buf.truncate(pt_len);
    Ok(buf)
}

/// Inverse of [`decrypt_attachment_inplace`]. Produces the on-disk
/// `iv(16) || ct || hmac(32)` layout. Exposed so tests and fixture
/// builders can write known-good encrypted blobs without re-deriving
/// the format. `iv` is passed in (not generated) so callers can keep
/// fixtures byte-deterministic.
pub fn encrypt_attachment(plaintext: &[u8], local_key: &[u8; 64], iv: &[u8; 16]) -> Vec<u8> {
    let aes_key: &[u8; 32] = local_key[..32].try_into().unwrap();
    let hmac_key: &[u8; 32] = local_key[32..].try_into().unwrap();
    // CBC encrypt with PKCS7. `encrypt_padded_mut` writes into a
    // pre-sized buffer.
    let pad_len = 16 - (plaintext.len() % 16);
    let mut buf = Vec::with_capacity(plaintext.len() + pad_len);
    buf.extend_from_slice(plaintext);
    buf.resize(plaintext.len() + pad_len, 0);
    let ct_len = Aes256CbcEnc::new(
        GenericArray::from_slice(aes_key),
        GenericArray::from_slice(iv),
    )
    .encrypt_padded_mut::<cipher::block_padding::Pkcs7>(&mut buf, plaintext.len())
    .expect("CBC encrypt with PKCS7 padding succeeds for any plaintext length")
    .len();
    buf.truncate(ct_len);
    let mut body = Vec::with_capacity(IV_LENGTH + buf.len() + MAC_LENGTH);
    body.extend_from_slice(iv);
    body.extend_from_slice(&buf);
    let mut mac = <HmacSha256 as Mac>::new_from_slice(hmac_key).expect("hmac key length");
    mac.update(&body);
    let tag = mac.finalize().into_bytes();
    body.extend_from_slice(&tag);
    body
}

fn verify_hmac(key: &[u8; 32], body: &[u8], expected_mac: &[u8]) -> Result<()> {
    let mut mac = <HmacSha256 as Mac>::new_from_slice(key).expect("hmac key length");
    mac.update(body);
    let computed = mac.finalize().into_bytes();
    if computed.ct_eq(expected_mac).into() {
        Ok(())
    } else {
        Err(anyhow!("hmac mismatch"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_mismatch_returns_error_not_panic() {
        // Construct a "main"-shaped buffer with a wrong MAC and confirm
        // we surface a clean Err rather than panicking inside the MAC
        // comparison path.
        let body = vec![0u8; 64];
        let bad_mac = vec![0u8; MAC_LENGTH];
        let mut blob = body.clone();
        blob.extend_from_slice(&bad_mac);
        let hmac_key = [0u8; 32];
        let aes_key = [0u8; 32];
        let err = decrypt_main(&hmac_key, &aes_key, &blob).unwrap_err();
        assert!(err.to_string().contains("MAC"));
    }

    #[test]
    fn attachment_hmac_mismatch_returns_error_not_panic() {
        let mut blob = vec![0u8; IV_LENGTH + 16];
        blob.extend_from_slice(&[0u8; MAC_LENGTH]);
        let key = [0u8; 64];
        let err = decrypt_attachment_inplace(&blob, &key).unwrap_err();
        assert!(err.to_string().contains("MAC"));
    }

    #[test]
    fn hkdf_backup_key_is_32_bytes() {
        let k = derive_backup_key(&"a".repeat(64));
        assert_eq!(k.len(), 32);
    }
}
