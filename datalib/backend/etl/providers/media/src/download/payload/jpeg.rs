//! JPEG payload: the coding tables and the entropy-coded scan, with
//! every `APPn` and comment segment removed.
//!
//! For a photo library this is the highest-value recipe in the module,
//! because JPEG metadata is *enormous* and *constantly rewritten*. A
//! camera JPEG's `APP1` holds EXIF plus a full embedded thumbnail;
//! Lightroom, Photos and exiftool rewrite it to set a rating, a
//! keyword, a caption, a corrected timestamp, or a GPS fix — and some
//! of them regenerate the thumbnail while they are there. None of that
//! touches a single coefficient of the image.
//!
//! What is excluded:
//!
//! - `APP0`…`APP15` — JFIF, EXIF and its thumbnail, XMP, Photoshop IRB
//!   (where ratings and crops live), MPF, and the ICC profile.
//! - `COM` — comment segments.
//! - Anything after `EOI`. Phone cameras append a second image there
//!   (Apple's depth data, Samsung's motion photo); it is a passenger,
//!   not the picture.
//!
//! What is kept: `SOF` (dimensions and component layout), `DQT`, `DHT`,
//! `DRI`, the `SOS` headers and the entropy-coded data itself.
//!
//! # Why the ICC profile is excluded
//!
//! `APP2`'s ICC profile is the one genuinely arguable exclusion: it is
//! metadata by structure, but it changes how the image *renders*, so
//! dropping it means a file re-tagged from sRGB to Display P3 keeps its
//! payload hash — a false merge, the direction this module otherwise
//! refuses.
//!
//! It is excluded anyway, for consistency and for cost. Consistency:
//! carving one `APPn` out of the exclusion would make the recipe "all
//! APPn except APP2", and an ICC profile is routinely rewritten
//! byte-differently for the same colour space by different tools —
//! which would hand back exactly the churn we are removing. Cost: the
//! failure is two photographs of the same scene in two colour spaces
//! sharing a hint column value, which a human resolves in one look at
//! the grid. `blake3` still distinguishes them, and it is still the
//! key.

use anyhow::Result;

use super::{be_u16, Plan, Range, Src};

/// Coding tables and entropy-coded data; `APPn` and `COM` excluded.
pub const SCHEME: &str = "jpeg.scan.v1";

/// Chunk size for the entropy-data scan. Large enough that a normal
/// photo's scan is one or two reads, small enough to bound memory on a
/// pathological file.
const SCAN_CHUNK: u64 = 1024 * 1024;

pub fn plan(src: &mut Src) -> Result<Option<Plan>> {
    let soi = src.read_upto(0, 2)?;
    anyhow::ensure!(soi == [0xff, 0xd8], "not a JPEG file");

    let mut ranges: Vec<Range> = Vec::new();
    let mut at = 2u64;

    while at + 2 <= src.len() {
        let b = src.read_at(at, 2)?;
        if b[0] != 0xff {
            // Desynchronized. Everything collected so far is still a
            // valid description of what we read.
            break;
        }
        let marker = b[1];
        if marker == 0xff {
            // Fill byte; markers may be preceded by any number of them.
            at += 1;
            continue;
        }
        if marker == 0xd9 {
            break; // EOI. Whatever follows is a passenger.
        }
        // Standalone markers carry no length field.
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            at += 2;
            continue;
        }

        let Some(len) = be_u16(&src.read_upto(at + 2, 2)?, 0).map(u64::from) else {
            break;
        };
        anyhow::ensure!(len >= 2, "JPEG segment length {len} is impossible");
        let data_at = at + 4;
        let data_len = len - 2;
        if data_at + data_len > src.len() {
            break; // truncated segment
        }

        let excluded = (0xe0..=0xef).contains(&marker) || marker == 0xfe;
        if !excluded {
            // The marker byte and the segment body, but not the length
            // field — it is derivable from the body and would otherwise
            // be hashed twice over.
            ranges.push((at + 1, 1));
            if data_len > 0 {
                ranges.push((data_at, data_len));
            }
        }

        at = data_at + data_len;

        if marker == 0xda {
            // Entropy-coded data follows the SOS header, unframed, and
            // runs to the next real marker.
            let end = scan_end(src, at)?;
            if end > at {
                ranges.push((at, end - at));
            }
            at = end;
        }
    }

    Ok(Plan::flat(SCHEME, ranges).non_empty())
}

/// Find the end of an entropy-coded run starting at `from`.
///
/// Inside the run, `FF` is escaped as `FF 00`, and restart markers
/// `FF D0`…`FF D7` are part of the data. Any other `FF xx` is the next
/// real marker and ends the run. Getting this wrong in the lenient
/// direction (stopping at the first `FF`) truncates the image; getting
/// it wrong in the greedy direction (running to EOF) would swallow the
/// trailing `APPn` segments this recipe exists to exclude.
fn scan_end(src: &mut Src, from: u64) -> Result<u64> {
    let mut at = from;
    while at < src.len() {
        let buf = src.read_upto(at, SCAN_CHUNK)?;
        if buf.is_empty() {
            break;
        }
        let mut i = 0usize;
        while i + 1 < buf.len() {
            if buf[i] == 0xff {
                let next = buf[i + 1];
                if next != 0x00 && !(0xd0..=0xd7).contains(&next) {
                    return Ok(at + i as u64);
                }
                i += 2;
                continue;
            }
            i += 1;
        }
        // Re-examine the final byte with the next chunk's first, so a
        // marker straddling the boundary is not missed.
        let consumed = buf.len() as u64 - 1;
        if consumed == 0 {
            break;
        }
        at += consumed;
    }
    Ok(src.len())
}

#[cfg(test)]
mod tests {
    use super::super::testutil::src_of;
    use super::*;

    fn seg(marker: u8, body: &[u8]) -> Vec<u8> {
        let mut s = vec![0xff, marker];
        s.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        s.extend_from_slice(body);
        s
    }

    const DQT: &[u8] = &[0x00, 16, 11, 10, 16, 24, 40, 51, 61];
    const SOF0: &[u8] = &[0x08, 0x00, 0x40, 0x00, 0x40, 0x01, 0x01, 0x11, 0x00];
    const DHT: &[u8] = &[0x00, 0x01, 0x02, 0x03];
    const SOS_HDR: &[u8] = &[0x01, 0x01, 0x00, 0x00, 0x3f, 0x00];
    /// Entropy data containing a stuffed `FF 00` and a restart marker,
    /// both of which must stay inside the run.
    const ENTROPY: &[u8] = &[
        0xa1, 0xb2, 0xff, 0x00, 0xc3, 0xff, 0xd0, 0xd4, 0xe5, 0x11, 0x22,
    ];

    fn jpeg(parts: &[Vec<u8>]) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8];
        for p in parts {
            out.extend_from_slice(p);
        }
        out.extend_from_slice(&[0xff, 0xd9]);
        out
    }

    fn baseline(extra_app: &[Vec<u8>]) -> Vec<u8> {
        let mut parts: Vec<Vec<u8>> = extra_app.to_vec();
        parts.push(seg(0xdb, DQT));
        parts.push(seg(0xc0, SOF0));
        parts.push(seg(0xc4, DHT));
        parts.push(seg(0xda, SOS_HDR));
        parts.push(ENTROPY.to_vec());
        jpeg(&parts)
    }

    fn hash(bytes: &[u8]) -> String {
        let mut t = src_of(bytes);
        let p = plan(&mut t.src).unwrap().unwrap();
        super::super::hash_plan(&mut t.src, &p).unwrap()
    }

    #[test]
    fn exif_xmp_and_icc_are_all_excluded() {
        let bare = baseline(&[]);
        let mut exif = b"Exif\x00\x00II\x2a\x00\x08\x00\x00\x00".to_vec();
        exif.extend_from_slice(&[0x77; 4096]); // the embedded thumbnail
        let decorated = baseline(&[
            seg(0xe0, b"JFIF\x00\x01\x02\x00\x00\x01\x00\x01\x00\x00"),
            seg(0xe1, &exif),
            seg(0xe1, b"http://ns.adobe.com/xap/1.0/\x00<x:xmpmeta/>"),
            seg(0xe2, b"ICC_PROFILE\x00\x01\x01somewhere-a-profile"),
            seg(0xed, b"Photoshop 3.0\x008BIM\x04\x04rating"),
            seg(0xfe, b"Created with a replicator"),
        ]);
        assert!(decorated.len() > bare.len() + 4000);
        assert_eq!(hash(&bare), hash(&decorated));
    }

    #[test]
    fn rewriting_exif_in_place_does_not_move_the_hash() {
        // The everyday case: same file, one keyword added.
        let before = baseline(&[seg(0xe1, b"Exif\x00\x00II\x2a\x00rating=3")]);
        let after = baseline(&[seg(0xe1, b"Exif\x00\x00II\x2a\x00rating=5;kw=holodeck")]);
        assert_ne!(before, after);
        assert_eq!(hash(&before), hash(&after));
    }

    #[test]
    fn entropy_data_is_included_and_a_change_in_it_shows() {
        let base = baseline(&[]);
        let mut other = ENTROPY.to_vec();
        other[0] ^= 0xff;
        let changed = jpeg(&[
            seg(0xdb, DQT),
            seg(0xc0, SOF0),
            seg(0xc4, DHT),
            seg(0xda, SOS_HDR),
            other,
        ]);
        assert_ne!(hash(&base), hash(&changed));
    }

    #[test]
    fn stuffed_bytes_and_restart_markers_stay_inside_the_scan() {
        // If `scan_end` stopped at the first 0xFF it would cut the run
        // after two bytes, and this file would hash the same as one
        // whose entropy data differs past that point.
        let mut truncated_tail = ENTROPY.to_vec();
        truncated_tail[8] ^= 0xff; // a byte *after* the FF00 and FFD0
        let a = baseline(&[]);
        let b = jpeg(&[
            seg(0xdb, DQT),
            seg(0xc0, SOF0),
            seg(0xc4, DHT),
            seg(0xda, SOS_HDR),
            truncated_tail,
        ]);
        assert_ne!(hash(&a), hash(&b));
    }

    #[test]
    fn quantization_and_huffman_tables_are_included() {
        let base = baseline(&[]);
        let mut other_dqt = DQT.to_vec();
        other_dqt[1] = 32; // a different quality
        let changed = jpeg(&[
            seg(0xdb, &other_dqt),
            seg(0xc0, SOF0),
            seg(0xc4, DHT),
            seg(0xda, SOS_HDR),
            ENTROPY.to_vec(),
        ]);
        assert_ne!(hash(&base), hash(&changed));
    }

    #[test]
    fn dimensions_change_the_hash_via_sof() {
        let base = baseline(&[]);
        let mut other_sof = SOF0.to_vec();
        other_sof[2] = 0x01; // a different height
        let changed = jpeg(&[
            seg(0xdb, DQT),
            seg(0xc0, SOF0.to_vec().as_slice()),
            seg(0xc0, &other_sof),
            seg(0xda, SOS_HDR),
            ENTROPY.to_vec(),
        ]);
        assert_ne!(hash(&base), hash(&changed));
    }

    #[test]
    fn a_progressive_jpegs_several_scans_are_all_included() {
        let one = jpeg(&[
            seg(0xdb, DQT),
            seg(0xc2, SOF0), // SOF2: progressive
            seg(0xc4, DHT),
            seg(0xda, SOS_HDR),
            ENTROPY.to_vec(),
        ]);
        let two = jpeg(&[
            seg(0xdb, DQT),
            seg(0xc2, SOF0),
            seg(0xc4, DHT),
            seg(0xda, SOS_HDR),
            ENTROPY.to_vec(),
            seg(0xda, SOS_HDR),
            vec![0x99, 0x88, 0x77],
        ]);
        assert_ne!(hash(&one), hash(&two), "the second scan must be hashed");
    }

    #[test]
    fn data_appended_after_eoi_is_ignored() {
        let plain = baseline(&[]);
        let mut with_trailer = plain.clone();
        // An Apple depth-map style second image after EOI.
        with_trailer.extend_from_slice(&[0xff, 0xd8]);
        with_trailer.extend_from_slice(&seg(0xe1, b"Exif\x00\x00second image"));
        with_trailer.extend_from_slice(&[0xff, 0xd9]);
        assert_eq!(hash(&plain), hash(&with_trailer));
    }

    #[test]
    fn fill_bytes_before_a_marker_are_tolerated() {
        let mut padded = vec![0xff, 0xd8];
        padded.extend_from_slice(&seg(0xdb, DQT));
        padded.push(0xff); // fill
        padded.push(0xff); // fill
        padded.extend_from_slice(&seg(0xc0, SOF0));
        padded.extend_from_slice(&seg(0xda, SOS_HDR));
        padded.extend_from_slice(ENTROPY);
        padded.extend_from_slice(&[0xff, 0xd9]);

        let unpadded = jpeg(&[
            seg(0xdb, DQT),
            seg(0xc0, SOF0),
            seg(0xda, SOS_HDR),
            ENTROPY.to_vec(),
        ]);
        assert_eq!(hash(&padded), hash(&unpadded));
    }

    #[test]
    fn a_metadata_only_jpeg_plans_nothing() {
        let mut t = src_of(&jpeg(&[seg(0xe1, b"Exif\x00\x00II\x2a\x00")]));
        assert!(plan(&mut t.src).unwrap().is_none());
    }

    #[test]
    fn non_jpeg_bytes_are_an_error() {
        let mut t = src_of(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0d");
        assert!(plan(&mut t.src).is_err());
    }
}
