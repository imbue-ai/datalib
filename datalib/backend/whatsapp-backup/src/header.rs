//! Minimal protobuf scanner that pulls the 16-byte IV out of the
//! `BackupPrefix` header at the front of a crypt15 file.
//!
//! We deliberately do *not* depend on prost or a generated proto here —
//! the only field we need is the IV, the format is single-purpose, and
//! avoiding codegen keeps the crate buildable in pure cargo without a
//! protoc dependency. The scan is wire-format aware enough to skip
//! varint, length-delimited, and fixed32/64 fields, which covers every
//! field WhatsApp uses in BackupPrefix today.
//!
//! File framing (single-file backup):
//!   byte 0          — protobuf size (single u8, big-endian; max 255)
//!   byte 1          — optional msgstore-features flag, present iff == 0x01
//!   N bytes         — BackupPrefix protobuf (length = size from byte 0)
//!   [...ciphertext, 16-byte GCM tag, 16-byte MD5 checksum...]
//!
//! BackupPrefix fields we care about:
//!   field 3 (c15_iv, length-delimited submessage) → field 1 (IV, 16 bytes)

use thiserror::Error;

#[derive(Debug, Error)]
pub enum HeaderError {
    #[error("file too short ({0} bytes) to contain a crypt15 header")]
    TooShort(usize),
    #[error("malformed varint in header (overflow or unterminated)")]
    BadVarint,
    #[error("unsupported protobuf wire type {0}")]
    UnsupportedWireType(u32),
    #[error("header advertises {advertised} bytes but only {available} are present")]
    HeaderOverrun { advertised: usize, available: usize },
    #[error("no IV field (field 3 → submessage field 1) found in header")]
    MissingIv,
    #[error("IV must be 16 bytes, got {0}")]
    BadIvLength(usize),
}

#[derive(Debug, Clone)]
pub struct BackupHeader {
    /// 16-byte GCM nonce, parsed out of `BackupPrefix.iv`.
    pub iv: [u8; 16],
    /// Byte offset at which the AES-256-GCM ciphertext starts. Equals
    /// `(uvarint length) + (advertised header length)`.
    pub body_offset: usize,
}

pub fn parse_header(bytes: &[u8]) -> Result<BackupHeader, HeaderError> {
    if bytes.len() < 2 {
        return Err(HeaderError::TooShort(bytes.len()));
    }
    // Single-byte protobuf size at offset 0 — NOT a varint, just a u8.
    let proto_size = bytes[0] as usize;
    // Optional msgstore-features flag at offset 1: consumed only if the
    // byte is exactly 0x01. Anything else means the flag is absent and
    // that byte is the first byte of the protobuf instead.
    let proto_start = if bytes[1] == 0x01 { 2 } else { 1 };
    let proto_end = proto_start + proto_size;
    if proto_end > bytes.len() {
        return Err(HeaderError::HeaderOverrun {
            advertised: proto_size,
            available: bytes.len() - proto_start,
        });
    }
    let header = &bytes[proto_start..proto_end];
    let iv = scan_for_iv(header)?;
    Ok(BackupHeader {
        iv,
        body_offset: proto_end,
    })
}

/// Walk the top-level BackupPrefix fields; when we hit field 3 (a
/// length-delimited submessage), recurse to find field 1 within it
/// (the 16-byte IV).
fn scan_for_iv(header: &[u8]) -> Result<[u8; 16], HeaderError> {
    let mut pos = 0usize;
    while pos < header.len() {
        let (tag, n) = read_varint(header, pos)?;
        pos += n;
        let field_no = (tag >> 3) as u32;
        let wire_type = (tag & 0x07) as u32;
        if field_no == 3 && wire_type == 2 {
            let (sub_len, n2) = read_varint(header, pos)?;
            pos += n2;
            let sub_end = pos + sub_len as usize;
            if sub_end > header.len() {
                return Err(HeaderError::HeaderOverrun {
                    advertised: sub_len as usize,
                    available: header.len() - pos,
                });
            }
            return scan_for_iv_submessage(&header[pos..sub_end]);
        }
        pos += skip_field(header, pos, wire_type)?;
    }
    Err(HeaderError::MissingIv)
}

fn scan_for_iv_submessage(sub: &[u8]) -> Result<[u8; 16], HeaderError> {
    let mut pos = 0usize;
    while pos < sub.len() {
        let (tag, n) = read_varint(sub, pos)?;
        pos += n;
        let field_no = (tag >> 3) as u32;
        let wire_type = (tag & 0x07) as u32;
        if field_no == 1 && wire_type == 2 {
            let (len, n2) = read_varint(sub, pos)?;
            pos += n2;
            let len = len as usize;
            if len != 16 {
                return Err(HeaderError::BadIvLength(len));
            }
            if pos + len > sub.len() {
                return Err(HeaderError::HeaderOverrun {
                    advertised: len,
                    available: sub.len() - pos,
                });
            }
            let mut iv = [0u8; 16];
            iv.copy_from_slice(&sub[pos..pos + 16]);
            return Ok(iv);
        }
        pos += skip_field(sub, pos, wire_type)?;
    }
    Err(HeaderError::MissingIv)
}

fn skip_field(buf: &[u8], pos: usize, wire_type: u32) -> Result<usize, HeaderError> {
    match wire_type {
        0 => {
            // Varint.
            let (_, n) = read_varint(buf, pos)?;
            Ok(n)
        }
        1 => Ok(8), // fixed64
        5 => Ok(4), // fixed32
        2 => {
            // Length-delimited.
            let (len, n) = read_varint(buf, pos)?;
            Ok(n + len as usize)
        }
        other => Err(HeaderError::UnsupportedWireType(other)),
    }
}

fn read_varint(buf: &[u8], start: usize) -> Result<(u64, usize), HeaderError> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    let mut i = start;
    while i < buf.len() {
        let b = buf[i];
        value |= ((b & 0x7F) as u64) << shift;
        i += 1;
        if b & 0x80 == 0 {
            return Ok((value, i - start));
        }
        shift += 7;
        if shift >= 64 {
            return Err(HeaderError::BadVarint);
        }
    }
    Err(HeaderError::BadVarint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iv_with_features_flag() {
        // Hand-built mini BackupPrefix:
        //   field 1 (key_type) = 1
        //   field 3 (c15_iv submessage) = { field 1 (IV) = 16 bytes }
        let iv: [u8; 16] = [
            0xc3, 0xac, 0x8a, 0x79, 0xbe, 0x70, 0x97, 0xd0, 0x28, 0x42, 0x31, 0x6d, 0x62, 0x57,
            0x0d, 0x2a,
        ];
        let mut proto = Vec::new();
        proto.extend_from_slice(&[0x08, 0x01]); // key_type=1
        proto.extend_from_slice(&[0x1a, 0x12]); // field 3, length 18
        proto.extend_from_slice(&[0x0a, 0x10]); // sub field 1, length 16
        proto.extend_from_slice(&iv);

        let mut full = Vec::new();
        full.push(proto.len() as u8); // single-byte size
        full.push(0x01); // features flag present
        full.extend_from_slice(&proto);
        full.extend_from_slice(&[0xff; 64]); // pretend ciphertext+tag+md5

        let parsed = parse_header(&full).unwrap();
        assert_eq!(parsed.iv, iv);
        assert_eq!(parsed.body_offset, 2 + proto.len());
    }

    #[test]
    fn parses_iv_without_features_flag() {
        // Same as above, but no 0x01 features byte — first protobuf
        // byte (0x08) is at offset 1 directly.
        let iv: [u8; 16] = [0xaa; 16];
        let mut proto = Vec::new();
        proto.extend_from_slice(&[0x08, 0x01]);
        proto.extend_from_slice(&[0x1a, 0x12, 0x0a, 0x10]);
        proto.extend_from_slice(&iv);

        let mut full = vec![proto.len() as u8];
        full.extend_from_slice(&proto);
        full.extend_from_slice(&[0xff; 64]);

        let parsed = parse_header(&full).unwrap();
        assert_eq!(parsed.iv, iv);
        assert_eq!(parsed.body_offset, 1 + proto.len());
    }

    #[test]
    fn rejects_missing_iv() {
        // Header with key_type but no field 3.
        let proto = vec![0x08, 0x01];
        let mut full = vec![proto.len() as u8, 0x01]; // size, features flag
        full.extend_from_slice(&proto);
        assert!(matches!(parse_header(&full), Err(HeaderError::MissingIv)));
    }

    #[test]
    fn rejects_truncated() {
        let too_short = vec![0u8];
        assert!(matches!(
            parse_header(&too_short),
            Err(HeaderError::TooShort(1))
        ));
    }
}
