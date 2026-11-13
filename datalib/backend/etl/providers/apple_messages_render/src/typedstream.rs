//! The text inside a `message.attributedBody`.
//!
//! Since macOS Ventura a message's body is not in `message.text` but in
//! this blob: Apple's `typedstream` archive of an NSAttributedString.
//! Only the string is wanted here. It sits right after the archived
//! class name, as a length-prefixed UTF-8 run; the attribute runs after
//! it (message parts, mentions, the attachment's transfer guid) are not
//! read.

/// The class `NSString`, version 1.
const STRING_CLASS: &[u8] = b"NSString\x01";
/// The type of the value that follows: one new type string, `+` (UTF-8).
const UTF8_TYPE: &[u8] = b"\x84\x01+";
/// The smallest back-reference. NSString's superclass, NSObject, is
/// always one, and its number depends on how many classes the archive
/// named first: 0x94 for an NSAttributedString, 0x95 for a mutable one.
const FIRST_REFERENCE: u8 = 0x92;

pub fn attributed_body_text(blob: &[u8]) -> Option<String> {
    let class_end = blob
        .windows(STRING_CLASS.len())
        .position(|w| w == STRING_CLASS)?
        + STRING_CLASS.len();
    if *blob.get(class_end)? < FIRST_REFERENCE {
        return None;
    }
    let type_at = class_end + 1;
    if blob.get(type_at..type_at + UTF8_TYPE.len())? != UTF8_TYPE {
        return None;
    }
    let (len, at) = read_int(blob, type_at + UTF8_TYPE.len())?;
    String::from_utf8(blob.get(at..at + len)?.to_vec()).ok()
}

/// A typedstream integer: one byte below 0x80, else a tag and its
/// little-endian bytes.
fn read_int(b: &[u8], i: usize) -> Option<(usize, usize)> {
    match *b.get(i)? {
        0x81 => Some((
            u16::from_le_bytes(b.get(i + 1..i + 3)?.try_into().ok()?) as usize,
            i + 3,
        )),
        0x82 => Some((
            u32::from_le_bytes(b.get(i + 1..i + 5)?.try_into().ok()?) as usize,
            i + 5,
        )),
        n if n < 0x80 => Some((n as usize, i + 1)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMMUTABLE_HEAD: &[u8] =
        b"\x04\x0bstreamtyped\x81\xe8\x03\x84\x01@\x84\x84\x84\x12NSAttributedString\
          \x00\x84\x84\x08NSObject\x00\x85\x92\x84\x84\x84\x08NSString\x01\x94\x84\x01+";

    /// An NSMutableAttributedString holding an NSMutableString: two more
    /// classes ahead of NSString, so its superclass reference is 0x95.
    const MUTABLE_HEAD: &[u8] = b"\x04\x0bstreamtyped\x81\xe8\x03\x84\x01@\x84\x84\x84\x19NSMutableAttributedString\
          \x00\x84\x84\x12NSAttributedString\x00\x84\x84\x08NSObject\x00\x85\x92\x84\x84\x84\x0fNSMutableString\
          \x01\x84\x84\x08NSString\x01\x95\x84\x01+";

    fn archive(len_prefix: &[u8], text: &[u8]) -> Vec<u8> {
        archive_with(IMMUTABLE_HEAD, len_prefix, text)
    }

    /// The head of an archive up to the string; the tail is attributes
    /// and does not matter here.
    fn archive_with(head: &[u8], len_prefix: &[u8], text: &[u8]) -> Vec<u8> {
        let mut v = head.to_vec();
        v.extend_from_slice(len_prefix);
        v.extend_from_slice(text);
        v.extend_from_slice(b"\x86\x84\x02iI\x01\x11\x92");
        v
    }

    #[test]
    fn short_and_long_bodies_decode() {
        let short = archive(&[17], b"Hello from Imbue!");
        assert_eq!(
            attributed_body_text(&short).as_deref(),
            Some("Hello from Imbue!")
        );
        let text = "x".repeat(300);
        let long = archive(&[0x81, 0x2c, 0x01], text.as_bytes());
        assert_eq!(attributed_body_text(&long).as_deref(), Some(text.as_str()));
        assert_eq!(attributed_body_text(b"not an archive"), None);
        assert_eq!(attributed_body_text(&archive(&[0x80], b"")), None);
    }

    /// A message whose body Messages archived as mutable strings (seen on
    /// macOS 26) was reported as "not a typedstream archive".
    #[test]
    fn a_mutable_archive_decodes() {
        let text = "y".repeat(200);
        let body = archive_with(MUTABLE_HEAD, &[0x81, 0xc8, 0x00], text.as_bytes());
        assert_eq!(attributed_body_text(&body).as_deref(), Some(text.as_str()));
        let short = archive_with(MUTABLE_HEAD, &[2], b"ok");
        assert_eq!(attributed_body_text(&short).as_deref(), Some("ok"));
    }
}
