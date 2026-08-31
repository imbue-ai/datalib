//! PNG payload: the chunks that determine the pixels.
//!
//! PNG's own critical/ancillary distinction almost gives us the answer
//! for free — critical chunks (an uppercase first letter) are the ones
//! a decoder must understand — so the recipe is "every critical chunk,
//! plus `tRNS`".
//!
//! `tRNS` is the one ancillary chunk included, because it carries
//! transparency for palette and grayscale images: dropping it would
//! merge a transparent PNG with an opaque one that is otherwise
//! identical, and that is a false *merge*, the direction we refuse.
//!
//! Everything else is out: `tEXt`/`iTXt`/`zTXt` (where every tagger
//! writes), `eXIf`, `tIME`, `pHYs`, `bKGD`. So are the colour-space
//! chunks `gAMA`, `cHRM`, `sRGB` and `iCCP` — the same call made for
//! JPEG's ICC profile, and made the same way for the same reason (see
//! [`super::jpeg`], which has the full argument).
//!
//! # The false split this recipe cannot avoid
//!
//! `IDAT` holds *deflate-compressed* pixels, not pixels. Running a PNG
//! through `optipng`, or re-saving it from a tool with a different zlib
//! level, produces byte-different `IDAT` for identical pixels — so the
//! payload hash splits. That is the accepted direction (a false split
//! costs a row; a false merge hides a file), but it does mean PNG
//! benefits less from this column than JPEG or MP3 do. Decompressing to
//! hash raw pixels would fix it and is what a future
//! `decoded_blake3` column would do.

use anyhow::Result;

use super::{be_u32, Plan, Range, Src};

/// Critical chunks plus `tRNS`.
pub const SCHEME: &str = "png.idat.v1";

const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
/// `len[4] type[4]` ahead of the data, `crc[4]` after it.
const CHUNK_OVERHEAD: u64 = 12;

/// Ancillary chunks that nonetheless change the rendered pixels.
const INCLUDED_ANCILLARY: &[&[u8; 4]] = &[b"tRNS"];

pub fn plan(src: &mut Src) -> Result<Option<Plan>> {
    let sig = src.read_upto(0, 8)?;
    anyhow::ensure!(sig == SIGNATURE, "not a PNG file");

    let mut ranges: Vec<Range> = Vec::new();
    let mut at = 8u64;
    while at + CHUNK_OVERHEAD <= src.len() {
        let hdr = src.read_at(at, 8)?;
        let len = u64::from(be_u32(&hdr, 0).ok_or_else(|| anyhow::anyhow!("short chunk"))?);
        let ctype: [u8; 4] = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let data_at = at + 8;
        let Some(next) = data_at.checked_add(len + 4).filter(|end| *end <= src.len()) else {
            // Truncated: keep what we have rather than losing the file.
            break;
        };

        // Uppercase fifth bit clear => critical.
        let critical = ctype[0].is_ascii_uppercase();
        if critical || INCLUDED_ANCILLARY.contains(&&ctype) {
            // Hash the type alongside the data so that relabelling a
            // chunk registers, and so two chunks with identical bodies
            // but different types cannot collide.
            ranges.push((at + 4, 4 + len));
        }
        if &ctype == b"IEND" {
            break;
        }
        at = next;
    }
    Ok(Plan::flat(SCHEME, ranges).non_empty())
}

#[cfg(test)]
mod tests {
    use super::super::testutil::src_of;
    use super::*;

    fn chunk(ctype: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut c = (data.len() as u32).to_be_bytes().to_vec();
        c.extend_from_slice(ctype);
        c.extend_from_slice(data);
        // The CRC is derivable from type+data, so it is not hashed and
        // does not need to be right here.
        c.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        c
    }

    const IHDR: &[u8] = b"\x00\x00\x00\x02\x00\x00\x00\x02\x08\x06\x00\x00\x00";
    const IDAT: &[u8] = b"\x78\x9c\x63\x60\x60\x60\x00\x00\x00\x04\x00\x01";

    fn png(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut out = SIGNATURE.to_vec();
        for c in chunks {
            out.extend_from_slice(c);
        }
        out
    }

    fn hash(bytes: &[u8]) -> String {
        let mut t = src_of(bytes);
        let p = plan(&mut t.src).unwrap().unwrap();
        super::super::hash_plan(&mut t.src, &p).unwrap()
    }

    #[test]
    fn text_exif_and_time_chunks_are_excluded() {
        let bare = png(&[
            chunk(b"IHDR", IHDR),
            chunk(b"IDAT", IDAT),
            chunk(b"IEND", b""),
        ]);
        let tagged = png(&[
            chunk(b"IHDR", IHDR),
            chunk(b"tEXt", b"Author\x00Jean-Luc Picard"),
            chunk(b"iTXt", b"Description\x00\x00\x00\x00\x00Bridge"),
            chunk(b"tIME", b"\x07\xd8\x0c\x1f\x0c\x00\x00"),
            chunk(b"eXIf", b"II\x2a\x00\x08\x00\x00\x00\x00\x00"),
            chunk(b"pHYs", b"\x00\x00\x0b\x13\x00\x00\x0b\x13\x01"),
            chunk(b"IDAT", IDAT),
            chunk(b"IEND", b""),
        ]);
        assert_ne!(bare, tagged);
        assert_eq!(hash(&bare), hash(&tagged));
    }

    #[test]
    fn colour_management_chunks_are_excluded_like_jpegs_icc_profile() {
        let bare = png(&[chunk(b"IHDR", IHDR), chunk(b"IDAT", IDAT)]);
        let managed = png(&[
            chunk(b"IHDR", IHDR),
            chunk(b"gAMA", b"\x00\x00\xb1\x8f"),
            chunk(b"sRGB", b"\x00"),
            chunk(b"iCCP", b"ICC\x00\x00\x78\x9c\x00"),
            chunk(b"IDAT", IDAT),
        ]);
        assert_eq!(hash(&bare), hash(&managed));
    }

    #[test]
    fn trns_is_included_because_it_changes_the_pixels() {
        let opaque = png(&[chunk(b"IHDR", IHDR), chunk(b"IDAT", IDAT)]);
        let transparent = png(&[
            chunk(b"IHDR", IHDR),
            chunk(b"tRNS", b"\x00\xff"),
            chunk(b"IDAT", IDAT),
        ]);
        assert_ne!(
            hash(&opaque),
            hash(&transparent),
            "dropping tRNS would be a false merge"
        );
    }

    #[test]
    fn changing_pixel_data_or_the_header_moves_the_hash() {
        let base = png(&[chunk(b"IHDR", IHDR), chunk(b"IDAT", IDAT)]);
        let mut other_idat = IDAT.to_vec();
        other_idat[4] ^= 0xff;
        assert_ne!(
            hash(&base),
            hash(&png(&[chunk(b"IHDR", IHDR), chunk(b"IDAT", &other_idat)]))
        );
        let mut other_ihdr = IHDR.to_vec();
        other_ihdr[3] = 0x04; // a different width
        assert_ne!(
            hash(&base),
            hash(&png(&[chunk(b"IHDR", &other_ihdr), chunk(b"IDAT", IDAT)]))
        );
    }

    #[test]
    fn splitting_idat_into_two_chunks_is_a_false_split_we_accept() {
        // Documented, not desired: the recipe hashes chunk framing, so
        // an encoder that emits one IDAT and one that emits two produce
        // different payload hashes for identical pixels.
        let one = png(&[chunk(b"IHDR", IHDR), chunk(b"IDAT", IDAT)]);
        let two = png(&[
            chunk(b"IHDR", IHDR),
            chunk(b"IDAT", &IDAT[..6]),
            chunk(b"IDAT", &IDAT[6..]),
        ]);
        assert_ne!(hash(&one), hash(&two));
    }

    #[test]
    fn a_truncated_file_keeps_the_chunks_that_are_whole() {
        let full = png(&[chunk(b"IHDR", IHDR), chunk(b"IDAT", IDAT)]);
        let cut = &full[..full.len() - 4];
        let mut t = src_of(cut);
        let p = plan(&mut t.src).unwrap().unwrap();
        // IHDR survived; the half-present IDAT did not.
        assert_eq!(p.total_bytes(), 4 + IHDR.len() as u64);
    }

    #[test]
    fn a_header_only_png_still_plans_because_ihdr_is_critical() {
        let mut t = src_of(&png(&[chunk(b"IHDR", IHDR)]));
        assert!(plan(&mut t.src).unwrap().is_some());
    }

    #[test]
    fn non_png_bytes_are_an_error() {
        let mut t = src_of(b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01");
        assert!(plan(&mut t.src).is_err());
    }
}
