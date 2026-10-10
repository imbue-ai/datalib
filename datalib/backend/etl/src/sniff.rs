//! What a file's leading bytes say it is. The blob CAS stores the type
//! the bytes name rather than the one a caller declared, which comes from
//! a header, an upstream field or a file name and can be wrong.

/// How many leading bytes [`content_type_from_bytes`] looks at.
pub const SIGNATURE_LEN: usize = 12;

/// The type `bytes` name by their signature, for the formats whose
/// signature names one type and nothing else. TIFF, ZIP and RIFF are left
/// out because they are containers: a camera raw file is a TIFF, a .docx
/// a ZIP, a WAV a RIFF.
pub fn content_type_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    let ct = match bytes {
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n', ..] => "image/png",
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => "image/gif",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "image/webp",
        [b'%', b'P', b'D', b'F', b'-', ..] => "application/pdf",
        [_, _, _, _, b'f', b't', b'y', b'p', b'h', b'e', b'i', b'c' | b'x', ..] => "image/heic",
        [_, _, _, _, b'f', b't', b'y', b'p', b'a', b'v', b'i', b'f', ..] => "image/avif",
        _ => return None,
    };
    Some(ct)
}

/// The type to store for `bytes` a caller declared as `declared`: the one
/// their signature names, unless the declared one already says the same.
pub fn content_type_for<'a>(declared: Option<&'a str>, bytes: &[u8]) -> Option<&'a str> {
    let Some(named) = content_type_from_bytes(bytes) else {
        return declared;
    };
    match declared {
        Some(d) if agrees(d, named) => Some(d),
        _ => Some(named),
    }
}

fn agrees(declared: &str, named: &str) -> bool {
    let essence = declared.split(';').next().unwrap_or("").trim();
    if essence.eq_ignore_ascii_case(named) {
        return true;
    }
    let alias = |a: &str| essence.eq_ignore_ascii_case(a);
    match named {
        "image/jpeg" => alias("image/jpg") || alias("image/pjpeg"),
        "image/png" => alias("image/apng") || alias("image/x-png"),
        "image/heic" => alias("image/heif"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
    const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0\0\x10JFIF\0";
    const GIF: &[u8] = b"GIF89a\x01\0\x01\0";
    const WEBP: &[u8] = b"RIFF\x24\0\0\0WEBPVP8 ";
    const PDF: &[u8] = b"%PDF-1.7\n%";
    const HEIC: &[u8] = b"\0\0\0\x18ftypheic\0\0\0\0";
    const AVIF: &[u8] = b"\0\0\0\x1cftypavif\0\0\0\0";

    #[test]
    fn each_signature_names_its_type() {
        for (bytes, want) in [
            (PNG, "image/png"),
            (JPEG, "image/jpeg"),
            (GIF, "image/gif"),
            (b"GIF87a\x01\0".as_slice(), "image/gif"),
            (WEBP, "image/webp"),
            (PDF, "application/pdf"),
            (HEIC, "image/heic"),
            (AVIF, "image/avif"),
        ] {
            assert_eq!(content_type_from_bytes(bytes), Some(want), "{bytes:?}");
        }
    }

    /// A signature shared by several formats names none of them: a camera
    /// raw file is a TIFF, a .docx is a ZIP, a WAV is a RIFF, a .mov has
    /// an `ftyp` box too.
    #[test]
    fn bytes_without_a_single_type_name_none() {
        for bytes in [
            b"hello, world".as_slice(),
            b"",
            b"\xFF\xD8",
            b"II*\0\x08\0\0\0",
            b"PK\x03\x04\x14\0\x06\0",
            b"RIFF\x24\0\0\0WAVEfmt ",
            b"\0\0\0\x14ftypqt  \0\0\0\0",
            b"BM6\0\0\0\0\0",
            b"GIF8 is not a gif",
            b"%PDF without a version",
        ] {
            assert_eq!(content_type_from_bytes(bytes), None, "{bytes:?}");
        }
    }

    /// A pasted picture named `*.png` on claude.ai came back as JPEG bytes
    /// and was stored as `image/png`.
    #[test]
    fn the_bytes_overrule_a_declared_type_they_disagree_with() {
        assert_eq!(
            content_type_for(Some("image/png"), JPEG),
            Some("image/jpeg")
        );
        assert_eq!(
            content_type_for(Some("application/octet-stream"), PDF),
            Some("application/pdf")
        );
        assert_eq!(content_type_for(Some("image/*"), WEBP), Some("image/webp"));
        assert_eq!(content_type_for(None, GIF), Some("image/gif"));
    }

    #[test]
    fn a_declared_type_the_bytes_agree_with_is_kept_as_written() {
        assert_eq!(content_type_for(Some("image/png"), PNG), Some("image/png"));
        assert_eq!(
            content_type_for(Some("Image/JPEG; q=1"), JPEG),
            Some("Image/JPEG; q=1")
        );
        assert_eq!(content_type_for(Some("image/jpg"), JPEG), Some("image/jpg"));
        assert_eq!(
            content_type_for(Some("image/apng"), PNG),
            Some("image/apng")
        );
        assert_eq!(
            content_type_for(Some("image/heif"), HEIC),
            Some("image/heif")
        );
    }

    #[test]
    fn unrecognized_bytes_keep_the_declared_type() {
        let docx = b"PK\x03\x04\x14\0\x06\0";
        let declared = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
        assert_eq!(content_type_for(Some(declared), docx), Some(declared));
        assert_eq!(
            content_type_for(Some("text/plain"), b"hi"),
            Some("text/plain")
        );
        assert_eq!(content_type_for(None, b"hi"), None);
    }
}
