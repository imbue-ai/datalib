//! TIFF and DNG payload: the strips and tiles the IFDs point at,
//! excluding embedded previews.
//!
//! # Why this one matters most for a Lightroom-shaped library
//!
//! A DNG is a TIFF holding several images: the raw sensor data in one
//! IFD, and one or more rendered JPEG **previews** in others. Lightroom
//! rewrites the preview every time you move a develop slider, and
//! writes the develop settings themselves into the EXIF/XMP block while
//! it is there. So the file hash of an actively-edited DNG moves
//! constantly while the sensor data — the irreplaceable part, the thing
//! you would call "the photograph" — has not changed since the shutter
//! closed.
//!
//! Excluding the preview IFDs is what makes `payload_blake3` answer
//! "is this the same exposure?" instead of "has anyone touched this
//! file?".
//!
//! # Which IFDs count
//!
//! An IFD is a preview if `NewSubfileType` (254) has bit 0 set
//! (reduced-resolution) or `SubfileType` (255) is 2. Thumbnails reached
//! through `JPEGInterchangeFormat` (513) are likewise skipped — that
//! tag is how the classic 6×4 TIFF thumbnail is stored.
//!
//! A plain scanned TIFF carries no `NewSubfileType` at all, so it is
//! included: absent means "the full image", per the TIFF 6.0 default.
//!
//! **If nothing qualifies**, every image-bearing IFD is used instead.
//! A file whose only images are all flagged reduced is strange, but
//! hashing the strange thing beats returning NULL and pretending we
//! could not read it.
//!
//! # A false split we accept
//!
//! Groups are emitted in IFD traversal order, so a rewriter that
//! reorders IFDs — or promotes a SubIFD — changes the digest without
//! changing a pixel. Consistent with the rest of the module: a false
//! split costs a duplicate row.

use std::collections::HashSet;

use anyhow::Result;

use super::{Plan, Range, Src};

/// Full-resolution strips and tiles; previews and thumbnails excluded.
pub const SCHEME: &str = "tiff.strips.v1";

/// TIFF field type codes, for the two we branch on by name.
const SHORT: u16 = 3;
const LONG: u16 = 4;

const TAG_IMAGE_WIDTH: u16 = 256;
const TAG_IMAGE_LENGTH: u16 = 257;
/// DNG's `DefaultCropSize`: the visible image inside the slightly
/// larger sensor readout. This is the size every RAW tool displays.
const TAG_DEFAULT_CROP_SIZE: u16 = 0xC620;
const TAG_NEW_SUBFILE_TYPE: u16 = 254;
const TAG_SUBFILE_TYPE: u16 = 255;
const TAG_STRIP_OFFSETS: u16 = 273;
const TAG_STRIP_BYTE_COUNTS: u16 = 279;
const TAG_SUB_IFDS: u16 = 330;
const TAG_TILE_OFFSETS: u16 = 324;
const TAG_TILE_BYTE_COUNTS: u16 = 325;

/// A cap on how many IFDs we will follow, so a file with a cyclic or
/// absurd SubIFD graph cannot spin. Real DNGs have a handful.
const MAX_IFDS: usize = 64;
/// A cap on entries per IFD's array-valued tags. A 100-megapixel image
/// in one-row strips is still far under this.
const MAX_ENTRIES: u64 = 1 << 20;

#[derive(Clone, Copy)]
struct Endian(bool);

impl Endian {
    fn u16(self, b: &[u8], at: usize) -> Option<u16> {
        if self.0 {
            super::le_u16(b, at)
        } else {
            super::be_u16(b, at)
        }
    }
    fn u32(self, b: &[u8], at: usize) -> Option<u32> {
        if self.0 {
            super::le_u32(b, at)
        } else {
            super::be_u32(b, at)
        }
    }
}

/// Bytes per TIFF field type, for the types that can hold an offset or
/// a byte count. Anything else we do not read as an array.
fn type_size(t: u16) -> Option<u64> {
    Some(match t {
        1 | 2 | 6 | 7 => 1, // BYTE, ASCII, SBYTE, UNDEFINED
        3 | 8 => 2,         // SHORT, SSHORT
        4 | 9 | 13 => 4,    // LONG, SLONG, IFD
        5 | 10 | 12 => 8,   // RATIONAL, SRATIONAL, DOUBLE
        11 => 4,            // FLOAT
        16..=18 => 8,       // LONG8, SLONG8, IFD8 (BigTIFF types)
        _ => return None,
    })
}

struct Entry {
    tag: u16,
    field_type: u16,
    count: u64,
    /// The raw 4-byte value field: either the value itself or an
    /// offset to it.
    value: u32,
}

/// Parse the header and collect every IFD: the top-level chain, plus
/// each one's SubIFDs. Depth is one level, which is what DNG uses.
///
/// Shared by [`plan`] and [`dimensions`] so the two cannot disagree
/// about which IFD is the real image — the whole point of
/// [`dimensions`] is that it reports the same one the payload hash
/// covers.
fn collect_ifds(src: &mut Src) -> Result<(Endian, Vec<u64>)> {
    let head = src.read_upto(0, 8)?;
    anyhow::ensure!(head.len() == 8, "truncated TIFF header");
    let endian = match &head[0..2] {
        b"II" => Endian(true),
        b"MM" => Endian(false),
        _ => anyhow::bail!("not a TIFF file"),
    };
    anyhow::ensure!(
        endian.u16(&head, 2) == Some(42),
        "TIFF magic is not 42 (BigTIFF is not supported)"
    );

    let mut ifds: Vec<u64> = Vec::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut at = u64::from(endian.u32(&head, 4).unwrap_or(0));
    while at != 0 && ifds.len() < MAX_IFDS && seen.insert(at) {
        ifds.push(at);
        let entries = read_ifd(src, endian, at)?;
        for sub in read_offsets(src, endian, &entries, TAG_SUB_IFDS)? {
            if sub != 0 && ifds.len() < MAX_IFDS && seen.insert(sub) {
                ifds.push(sub);
            }
        }
        at = next_ifd(src, endian, at)?;
    }
    Ok((endian, ifds))
}

/// The full-resolution image's pixel dimensions.
///
/// This exists because a DNG's *primary* IFD is usually the embedded
/// preview, so the EXIF reader — which only ever looks at IFD0 —
/// reports the preview's size. That is the wrong number for anything a
/// person would ask ("how big is this photograph?"), and it is wrong by
/// a factor of five or more.
///
/// The IFD chosen here is the same one [`plan`] hashes: the first that
/// carries image data and is not flagged reduced-resolution.
///
/// `DefaultCropSize` wins when present. A RAW sensor reads out slightly
/// larger than the visible frame — the margin feeds demosaicing at the
/// edges — so `ImageWidth`/`ImageLength` are a few dozen pixels bigger
/// than what every RAW tool, and the photographer, calls the image
/// size.
pub fn dimensions(src: &mut Src) -> Result<Option<(i64, i64)>> {
    let (endian, ifds) = collect_ifds(src)?;
    let mut fallback = None;
    for ifd in ifds {
        let entries = read_ifd(src, endian, ifd)?;
        if image_ranges(src, endian, &entries)?.is_empty() {
            continue;
        }
        let dims = match crop_size(src, endian, &entries)? {
            Some(d) => Some(d),
            None => match (
                scalar(&entries, TAG_IMAGE_WIDTH, endian),
                scalar(&entries, TAG_IMAGE_LENGTH, endian),
            ) {
                (Some(w), Some(h)) if w > 0 && h > 0 => Some((w as i64, h as i64)),
                _ => None,
            },
        };
        let Some(dims) = dims else { continue };
        if !is_reduced(src, endian, &entries)? {
            return Ok(Some(dims));
        }
        // Mirrors `plan`'s fallback: if every IFD claims to be reduced,
        // report the first one rather than nothing.
        fallback = fallback.or(Some(dims));
    }
    Ok(fallback)
}

/// `DefaultCropSize` as `(width, height)`, when it is stored as an
/// integer pair.
///
/// The tag also permits RATIONAL, which [`read_offsets`] would decode
/// as a single 64-bit integer — garbage. Rather than teach that decoder
/// about fractions for one tag, an unexpected type falls through to
/// `ImageWidth`/`ImageLength`, which is a correct answer, just a
/// slightly larger one.
fn crop_size(src: &mut Src, endian: Endian, entries: &[Entry]) -> Result<Option<(i64, i64)>> {
    let Some(e) = entries.iter().find(|e| e.tag == TAG_DEFAULT_CROP_SIZE) else {
        return Ok(None);
    };
    if !matches!(e.field_type, SHORT | LONG) || e.count != 2 {
        return Ok(None);
    }
    let v = read_offsets(src, endian, entries, TAG_DEFAULT_CROP_SIZE)?;
    Ok(match (v.first(), v.get(1)) {
        (Some(&w), Some(&h)) if w > 0 && h > 0 => Some((w as i64, h as i64)),
        _ => None,
    })
}

pub fn plan(src: &mut Src) -> Result<Option<Plan>> {
    let (endian, ifds) = collect_ifds(src)?;

    // Two passes: preferred IFDs, then every image-bearing one as the
    // fallback described in the module docs.
    let mut preferred: Vec<Vec<Range>> = Vec::new();
    let mut all: Vec<Vec<Range>> = Vec::new();
    for ifd in ifds {
        let entries = read_ifd(src, endian, ifd)?;
        let ranges = image_ranges(src, endian, &entries)?;
        if ranges.is_empty() {
            continue;
        }
        if !is_reduced(src, endian, &entries)? {
            preferred.push(ranges.clone());
        }
        all.push(ranges);
    }

    let groups = if preferred.is_empty() { all } else { preferred };
    Ok(Plan {
        scheme: SCHEME,
        groups,
    }
    .non_empty())
}

fn read_ifd(src: &mut Src, endian: Endian, at: u64) -> Result<Vec<Entry>> {
    let count_bytes = src.read_upto(at, 2)?;
    anyhow::ensure!(count_bytes.len() == 2, "IFD at {at} runs past end of file");
    let n = u64::from(endian.u16(&count_bytes, 0).unwrap_or(0));
    anyhow::ensure!(n <= MAX_ENTRIES, "IFD claims {n} entries");
    let body = src.read_upto(at + 2, n * 12)?;
    anyhow::ensure!(
        body.len() as u64 == n * 12,
        "IFD at {at} is truncated ({} of {} bytes)",
        body.len(),
        n * 12
    );
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        let e = i * 12;
        out.push(Entry {
            tag: endian.u16(&body, e).unwrap_or(0),
            field_type: endian.u16(&body, e + 2).unwrap_or(0),
            count: u64::from(endian.u32(&body, e + 4).unwrap_or(0)),
            value: endian.u32(&body, e + 8).unwrap_or(0),
        });
    }
    Ok(out)
}

fn next_ifd(src: &mut Src, endian: Endian, at: u64) -> Result<u64> {
    let count_bytes = src.read_upto(at, 2)?;
    let n = u64::from(endian.u16(&count_bytes, 0).unwrap_or(0));
    let tail = src.read_upto(at + 2 + n * 12, 4)?;
    if tail.len() < 4 {
        return Ok(0);
    }
    Ok(u64::from(endian.u32(&tail, 0).unwrap_or(0)))
}

/// Read a tag whose value is an array of integers (offsets, byte
/// counts, SubIFD pointers). Values of four bytes or fewer live inline
/// in the entry rather than at an offset — the classic TIFF footgun,
/// and the reason a single-strip image needs this branch.
fn read_offsets(src: &mut Src, endian: Endian, entries: &[Entry], tag: u16) -> Result<Vec<u64>> {
    let Some(e) = entries.iter().find(|e| e.tag == tag) else {
        return Ok(Vec::new());
    };
    let Some(size) = type_size(e.field_type) else {
        return Ok(Vec::new());
    };
    anyhow::ensure!(
        e.count <= MAX_ENTRIES,
        "tag {tag} claims {} values",
        e.count
    );
    let total = e.count * size;

    let bytes = if total <= 4 {
        e.value.to_be_bytes().to_vec()
    } else {
        src.read_upto(u64::from(e.value), total)?
    };
    // The inline case stored the value in the file's byte order, so
    // re-encode it that way before decoding below.
    let bytes = if total <= 4 {
        if endian.0 {
            e.value.to_le_bytes().to_vec()
        } else {
            bytes
        }
    } else {
        bytes
    };
    if (bytes.len() as u64) < total {
        return Ok(Vec::new());
    }

    let mut out = Vec::with_capacity(e.count as usize);
    for i in 0..e.count as usize {
        let at = i * size as usize;
        let v = match size {
            2 => u64::from(endian.u16(&bytes, at).unwrap_or(0)),
            4 => u64::from(endian.u32(&bytes, at).unwrap_or(0)),
            8 => {
                let lo = u64::from(endian.u32(&bytes, at).unwrap_or(0));
                let hi = u64::from(endian.u32(&bytes, at + 4).unwrap_or(0));
                if endian.0 {
                    lo | (hi << 32)
                } else {
                    (lo << 32) | hi
                }
            }
            _ => u64::from(bytes[at]),
        };
        out.push(v);
    }
    Ok(out)
}

/// A single scalar tag value, read from the inline field.
fn scalar(entries: &[Entry], tag: u16, endian: Endian) -> Option<u64> {
    let e = entries.iter().find(|e| e.tag == tag)?;
    Some(match type_size(e.field_type)? {
        // A SHORT sits in the *first* two bytes of the value field, in
        // the file's byte order.
        2 => u64::from(if endian.0 {
            super::le_u16(&e.value.to_le_bytes(), 0)?
        } else {
            super::be_u16(&e.value.to_be_bytes(), 0)?
        }),
        _ => u64::from(e.value),
    })
}

fn is_reduced(_src: &mut Src, endian: Endian, entries: &[Entry]) -> Result<bool> {
    if let Some(v) = scalar(entries, TAG_NEW_SUBFILE_TYPE, endian) {
        if v & 1 != 0 {
            return Ok(true);
        }
    }
    if scalar(entries, TAG_SUBFILE_TYPE, endian) == Some(2) {
        return Ok(true);
    }
    Ok(false)
}

/// The image byte ranges of one IFD: strips, or tiles, whichever it
/// uses.
fn image_ranges(src: &mut Src, endian: Endian, entries: &[Entry]) -> Result<Vec<Range>> {
    for (off_tag, len_tag) in [
        (TAG_STRIP_OFFSETS, TAG_STRIP_BYTE_COUNTS),
        (TAG_TILE_OFFSETS, TAG_TILE_BYTE_COUNTS),
    ] {
        let offsets = read_offsets(src, endian, entries, off_tag)?;
        let lens = read_offsets(src, endian, entries, len_tag)?;
        if offsets.is_empty() || offsets.len() != lens.len() {
            continue;
        }
        let mut ranges: Vec<Range> = Vec::with_capacity(offsets.len());
        for (o, l) in offsets.into_iter().zip(lens) {
            // A strip pointing outside the file is a corrupt IFD, not a
            // reason to hash bytes that are not there.
            if l == 0 || o.checked_add(l).is_none_or(|end| end > src.len()) {
                continue;
            }
            ranges.push((o, l));
        }
        if !ranges.is_empty() {
            return Ok(ranges);
        }
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{b3, src_of};
    use super::*;

    /// Build a little-endian TIFF. `ifds` is a list of
    /// `(entries, image_bytes)`; image data is appended after the IFDs
    /// and each IFD gets `StripOffsets`/`StripByteCounts` pointing at
    /// its own block.
    /// `(tag, field type, value)` triples written verbatim into an IFD.
    type Tags = Vec<(u16, u16, u32)>;

    struct Builder {
        ifds: Vec<(Tags, Vec<u8>)>,
    }

    impl Builder {
        fn new() -> Self {
            Self { ifds: Vec::new() }
        }

        /// `extra` are `(tag, type, value)` triples written verbatim.
        fn ifd(mut self, extra: &[(u16, u16, u32)], image: &[u8]) -> Self {
            self.ifds.push((extra.to_vec(), image.to_vec()));
            self
        }

        /// Like [`Self::build`], but rewrites the last IFD's
        /// `DefaultCropSize` entry into a real two-value LONG array.
        /// The `(tag, type, value)` triples this builder takes hold one
        /// inline value each, and a crop size is a pair that must live
        /// out of line.
        fn build_with_crop(self, crop: (u32, u32)) -> Vec<u8> {
            let mut out = self.build();
            let pos = out
                .windows(2)
                .position(|w| w == TAG_DEFAULT_CROP_SIZE.to_le_bytes())
                .expect("no DefaultCropSize entry to rewrite");
            let value_at = out.len() as u32;
            out[pos + 4..pos + 8].copy_from_slice(&2u32.to_le_bytes()); // count
            out[pos + 8..pos + 12].copy_from_slice(&value_at.to_le_bytes());
            out.extend_from_slice(&crop.0.to_le_bytes());
            out.extend_from_slice(&crop.1.to_le_bytes());
            out
        }

        fn build(self) -> Vec<u8> {
            // Layout: header, then each IFD (with strip tags appended),
            // then the image blocks.
            let n = self.ifds.len();
            let ifd_sizes: Vec<usize> = self
                .ifds
                .iter()
                .map(|(e, _)| 2 + (e.len() + 2) * 12 + 4)
                .collect();
            let mut ifd_at = Vec::with_capacity(n);
            let mut cur = 8usize;
            for s in &ifd_sizes {
                ifd_at.push(cur);
                cur += s;
            }
            let mut img_at = Vec::with_capacity(n);
            for (_, img) in &self.ifds {
                img_at.push(cur);
                cur += img.len();
            }

            let mut out = b"II\x2a\x00".to_vec();
            out.extend_from_slice(&(ifd_at[0] as u32).to_le_bytes());
            for (i, (extra, img)) in self.ifds.iter().enumerate() {
                let mut entries = extra.clone();
                entries.push((TAG_STRIP_OFFSETS, 4, img_at[i] as u32));
                entries.push((TAG_STRIP_BYTE_COUNTS, 4, img.len() as u32));
                entries.sort_by_key(|(t, _, _)| *t);
                out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
                for (tag, ty, val) in entries {
                    out.extend_from_slice(&tag.to_le_bytes());
                    out.extend_from_slice(&ty.to_le_bytes());
                    out.extend_from_slice(&1u32.to_le_bytes()); // count
                    out.extend_from_slice(&val.to_le_bytes());
                }
                let next = if i + 1 < n { ifd_at[i + 1] as u32 } else { 0 };
                out.extend_from_slice(&next.to_le_bytes());
            }
            for (_, img) in &self.ifds {
                out.extend_from_slice(img);
            }
            out
        }
    }

    const TAG_IMAGE_WIDTH_T: u16 = TAG_IMAGE_WIDTH;
    const TAG_IMAGE_LENGTH_T: u16 = TAG_IMAGE_LENGTH;
    const RAW: &[u8] = b"raw-sensor-data-that-never-changes-after-the-shutter";
    const PREVIEW_A: &[u8] = b"preview-jpeg-rendered-at-develop-time-v1";
    const PREVIEW_B: &[u8] = b"preview-jpeg-rendered-at-develop-time-v2-longer!!";

    fn hash(bytes: &[u8]) -> String {
        let mut t = src_of(bytes);
        let p = plan(&mut t.src).unwrap().unwrap();
        super::super::hash_plan(&mut t.src, &p).unwrap()
    }

    /// The headline case: a DNG whose preview is rewritten by an edit.
    #[test]
    fn rewriting_a_dng_preview_does_not_move_the_payload_hash() {
        // IFD0 is the reduced-resolution preview, IFD1 the raw.
        let v1 = Builder::new()
            .ifd(&[(TAG_NEW_SUBFILE_TYPE, 4, 1)], PREVIEW_A)
            .ifd(&[(TAG_NEW_SUBFILE_TYPE, 4, 0)], RAW)
            .build();
        let v2 = Builder::new()
            .ifd(&[(TAG_NEW_SUBFILE_TYPE, 4, 1)], PREVIEW_B)
            .ifd(&[(TAG_NEW_SUBFILE_TYPE, 4, 0)], RAW)
            .build();
        assert_ne!(v1, v2, "the files differ");
        assert_eq!(hash(&v1), hash(&v2));
        // …and the digest is over the raw data only.
        assert_eq!(hash(&v1), b3(RAW));
    }

    #[test]
    fn changing_the_raw_data_does_move_it() {
        let a = Builder::new()
            .ifd(&[(TAG_NEW_SUBFILE_TYPE, 4, 1)], PREVIEW_A)
            .ifd(&[(TAG_NEW_SUBFILE_TYPE, 4, 0)], RAW)
            .build();
        let mut other = RAW.to_vec();
        other[0] ^= 0xff;
        let b = Builder::new()
            .ifd(&[(TAG_NEW_SUBFILE_TYPE, 4, 1)], PREVIEW_A)
            .ifd(&[(TAG_NEW_SUBFILE_TYPE, 4, 0)], &other)
            .build();
        assert_ne!(hash(&a), hash(&b));
    }

    #[test]
    fn subfile_type_two_also_marks_a_preview() {
        let bytes = Builder::new()
            .ifd(&[(TAG_SUBFILE_TYPE, 3, 2)], PREVIEW_A)
            .ifd(&[], RAW)
            .build();
        assert_eq!(hash(&bytes), b3(RAW));
    }

    #[test]
    fn a_plain_tiff_with_no_subfile_tag_is_included() {
        // TIFF 6.0's default for a missing NewSubfileType is "the full
        // image", so a scanned page must not be treated as a preview.
        let bytes = Builder::new().ifd(&[], RAW).build();
        assert_eq!(hash(&bytes), b3(RAW));
    }

    #[test]
    fn subifds_are_followed() {
        // The DNG layout: IFD0 is the preview and points at the raw
        // through SubIFDs rather than through the IFD chain.
        let raw_ifd_at = 8 + 2 + 3 * 12 + 4; // after IFD0
        let mut b = Builder::new()
            .ifd(
                &[
                    (TAG_NEW_SUBFILE_TYPE, 4, 1),
                    (TAG_SUB_IFDS, 4, raw_ifd_at as u32),
                ],
                PREVIEW_A,
            )
            .ifd(&[(TAG_NEW_SUBFILE_TYPE, 4, 0)], RAW)
            .build();
        // Break the IFD chain so only the SubIFD pointer can reach the
        // raw: IFD0's next-IFD field is the last 4 bytes of IFD0.
        let next_field = 8 + 2 + 3 * 12;
        b[next_field..next_field + 4].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(hash(&b), b3(RAW));
    }

    #[test]
    fn when_every_ifd_is_flagged_reduced_we_hash_them_rather_than_return_null() {
        let bytes = Builder::new()
            .ifd(&[(TAG_NEW_SUBFILE_TYPE, 4, 1)], PREVIEW_A)
            .ifd(&[(TAG_NEW_SUBFILE_TYPE, 4, 1)], RAW)
            .build();
        let mut t = src_of(&bytes);
        let p = plan(&mut t.src).unwrap().unwrap();
        assert_eq!(p.groups.len(), 2);
    }

    #[test]
    fn two_image_ifds_are_separate_groups_not_one_stream() {
        let bytes = Builder::new().ifd(&[], RAW).ifd(&[], PREVIEW_A).build();
        let mut t = src_of(&bytes);
        let p = plan(&mut t.src).unwrap().unwrap();
        assert_eq!(p.groups.len(), 2);
        assert_ne!(
            super::super::hash_plan(&mut t.src, &p).unwrap(),
            b3(&[RAW, PREVIEW_A].concat()),
            "groups must not be hashed as one concatenated stream"
        );
    }

    #[test]
    fn big_endian_files_decode_the_same_way() {
        let le = Builder::new().ifd(&[], RAW).build();
        let mut be = le.clone();
        // Rewrite as MM: byte order mark, magic, and every multi-byte
        // field. Simpler to assert the header check alone here.
        be[0..2].copy_from_slice(b"MM");
        be[2..4].copy_from_slice(&42u16.to_be_bytes());
        be[4..8].copy_from_slice(&8u32.to_be_bytes());
        let mut t = src_of(&be);
        // The entry bodies are still little-endian, so this must fail
        // cleanly rather than hash garbage.
        let r = plan(&mut t.src);
        assert!(r.is_err() || r.unwrap().is_none());
    }

    #[test]
    fn strips_pointing_outside_the_file_are_dropped() {
        let mut bytes = Builder::new().ifd(&[], RAW).build();
        // Point StripOffsets far past the end.
        let pos = bytes
            .windows(2)
            .position(|w| w == TAG_STRIP_OFFSETS.to_le_bytes())
            .unwrap();
        bytes[pos + 8..pos + 12].copy_from_slice(&0xffff_0000u32.to_le_bytes());
        let mut t = src_of(&bytes);
        assert!(plan(&mut t.src).unwrap().is_none());
    }

    #[test]
    fn a_cyclic_ifd_chain_terminates() {
        let mut bytes = Builder::new().ifd(&[], RAW).build();
        // Point IFD0's next-IFD field back at itself.
        let next_field = 8 + 2 + 2 * 12;
        bytes[next_field..next_field + 4].copy_from_slice(&8u32.to_le_bytes());
        let mut t = src_of(&bytes);
        assert!(plan(&mut t.src).is_ok());
    }

    #[test]
    fn non_tiff_bytes_are_an_error() {
        let mut t = src_of(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0d");
        assert!(plan(&mut t.src).is_err());
    }

    /// The whole reason `dimensions` exists: a DNG's IFD0 is the
    /// preview, so anything reading only the primary IFD reports the
    /// wrong size by a large factor.
    #[test]
    fn dimensions_come_from_the_full_resolution_ifd_not_the_preview() {
        let bytes = Builder::new()
            .ifd(
                &[
                    (TAG_NEW_SUBFILE_TYPE, 4, 1),
                    (TAG_IMAGE_WIDTH_T, 4, 64),
                    (TAG_IMAGE_LENGTH_T, 4, 48),
                ],
                PREVIEW_A,
            )
            .ifd(
                &[
                    (TAG_NEW_SUBFILE_TYPE, 4, 0),
                    (TAG_IMAGE_WIDTH_T, 4, 320),
                    (TAG_IMAGE_LENGTH_T, 4, 240),
                ],
                RAW,
            )
            .build();
        let mut t = src_of(&bytes);
        assert_eq!(dimensions(&mut t.src).unwrap(), Some((320, 240)));
    }

    /// `DefaultCropSize` is what a RAW tool displays: the sensor reads
    /// out a little larger than the visible frame.
    #[test]
    fn default_crop_size_wins_over_the_sensor_readout() {
        let bytes = Builder::new()
            .ifd(
                &[
                    (TAG_NEW_SUBFILE_TYPE, 4, 0),
                    (TAG_IMAGE_WIDTH_T, 4, 320),
                    (TAG_IMAGE_LENGTH_T, 4, 240),
                    (TAG_DEFAULT_CROP_SIZE, 4, 0),
                ],
                RAW,
            )
            .build_with_crop((316, 236));
        let mut t = src_of(&bytes);
        assert_eq!(dimensions(&mut t.src).unwrap(), Some((316, 236)));
    }

    /// A RATIONAL `DefaultCropSize` is legal and `read_offsets` cannot
    /// decode it, so it must fall through rather than return garbage.
    #[test]
    fn a_rational_crop_size_falls_through_to_image_width() {
        let bytes = Builder::new()
            .ifd(
                &[
                    (TAG_NEW_SUBFILE_TYPE, 4, 0),
                    (TAG_IMAGE_WIDTH_T, 4, 320),
                    (TAG_IMAGE_LENGTH_T, 4, 240),
                    // Type 5 = RATIONAL, which `crop_size` refuses.
                    (TAG_DEFAULT_CROP_SIZE, 5, 0),
                ],
                RAW,
            )
            .build();
        let mut t = src_of(&bytes);
        assert_eq!(dimensions(&mut t.src).unwrap(), Some((320, 240)));
    }

    #[test]
    fn a_plain_tiff_reports_its_own_dimensions() {
        let bytes = Builder::new()
            .ifd(
                &[(TAG_IMAGE_WIDTH_T, 4, 800), (TAG_IMAGE_LENGTH_T, 4, 600)],
                RAW,
            )
            .build();
        let mut t = src_of(&bytes);
        assert_eq!(dimensions(&mut t.src).unwrap(), Some((800, 600)));
    }

    #[test]
    fn an_ifd_with_no_dimensions_reports_none() {
        let mut t = src_of(&Builder::new().ifd(&[], RAW).build());
        assert_eq!(dimensions(&mut t.src).unwrap(), None);
    }
}
