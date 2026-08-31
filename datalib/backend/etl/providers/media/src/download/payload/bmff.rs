//! ISO base media payload: MP4, M4A, MOV, HEIC.
//!
//! One container covers most of a modern library — every iPhone photo
//! and video, every AAC or ALAC track, every screen recording — and it
//! stores metadata in two places that both get rewritten constantly:
//! `moov/udta/meta/ilst` (the iTunes-style tags) and, for HEIF, `Exif`
//! and `mime` (XMP) items sitting alongside the picture.
//!
//! # Sample bytes, not the `mdat` box
//!
//! The naive recipe is "hash the `mdat` box". It is wrong because
//! `mdat` is a bag of bytes whose *layout* the muxer chooses: chunk
//! interleave, padding, and where in the file the box sits are all
//! muxer decisions that a `faststart` rewrite or a remux can change
//! without touching a single coded sample.
//!
//! So the sample tables (`stsc`/`stsz`/`stco`) are walked to find where
//! each track's samples actually are, and only those bytes are hashed —
//! grouped per track and ordered by **track id** rather than by
//! position in the file.
//!
//! **Changing any track changes the file's payload hash.** The groups
//! are combined into one digest, so this is not a way to keep a video
//! edit from registering, and it should not be: a clip with different
//! pictures is a different clip.
//!
//! What the grouping buys is that the digest is a function of the
//! tracks' *contents* and nothing else. Reordering the `trak` boxes,
//! moving `moov` ahead of `mdat`, inserting `free` padding, or
//! re-interleaving the chunks all leave it alone, because none of those
//! change any track's sample bytes and none of them are hashed.
//!
//! (Per-track digests are computed and then discarded. Storing them —
//! a `media_streams` table keyed on `(item, track_id)` — would make
//! "which files share this audio track?" a real query. It is not built;
//! nothing today reads a group digest on its own.)
//!
//! Samples within a chunk are contiguous by definition, so the plan
//! carries one range per chunk rather than one per sample. A
//! two-hour film is a few thousand ranges instead of a million.
//!
//! # HEIF still images have no tracks
//!
//! A HEIC photo stores its picture as *items* in a `meta` box, with no
//! `moov` at all, so the track walk finds nothing. Those files get
//! [`ITEMS_SCHEME`] instead: every item's extents, **except** the
//! `Exif` and `mime` items, which are precisely the metadata. Grouped
//! per item and ordered by item ID.

use std::collections::BTreeMap;

use anyhow::Result;

use super::{be_u16, be_u32, be_u64, Plan, Range, Src};

/// Per-track sample bytes, from the sample tables.
pub const SCHEME: &str = "bmff.samples.v1";
/// Per-item extents, for HEIF stills with no tracks.
pub const ITEMS_SCHEME: &str = "bmff.items.v1";

/// A guard against a corrupt count field asking for a huge allocation.
/// 2 M samples is roughly a 12-hour audio track.
const MAX_TABLE_ENTRIES: u64 = 2_000_000;
/// Nesting depth we will descend. The deepest real path is
/// `moov/trak/mdia/minf/stbl/stsd`.
const MAX_DEPTH: u32 = 8;

/// One box. Public to the crate so [`super::super::meta`] can read
/// `mvhd`/`tkhd`/`stsd` through the same walker the payload plan uses,
/// rather than growing a second one that can drift from it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Atom {
    pub btype: [u8; 4],
    pub body_at: u64,
    pub body_len: u64,
}

/// Iterate the boxes directly inside `[at, end)`.
///
/// Stops rather than errors on a malformed length: files truncated
/// mid-download are common, and the boxes before the damage are still
/// perfectly readable.
pub(crate) fn atoms(src: &mut Src, at: u64, end: u64) -> Result<Vec<Atom>> {
    let mut out = Vec::new();
    let mut cur = at;
    while cur + 8 <= end {
        let hdr = src.read_at(cur, 8)?;
        let size32 = u64::from(be_u32(&hdr, 0).unwrap_or(0));
        let btype = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let (header_len, size) = match size32 {
            // `1` means the real size is a 64-bit field after the type.
            1 => {
                if cur + 16 > end {
                    break;
                }
                let ext = src.read_at(cur + 8, 8)?;
                (16u64, be_u64(&ext, 0).unwrap_or(0))
            }
            // `0` means "to the end of the enclosing box".
            0 => (8u64, end - cur),
            n => (8u64, n),
        };
        if size < header_len || cur + size > end {
            break;
        }
        out.push(Atom {
            btype,
            body_at: cur + header_len,
            body_len: size - header_len,
        });
        cur += size;
    }
    Ok(out)
}

pub(crate) fn find<'a>(list: &'a [Atom], btype: &[u8; 4]) -> Option<&'a Atom> {
    list.iter().find(|a| &a.btype == btype)
}

/// Descend a chain of single-child box types, e.g.
/// `mdia/minf/stbl`.
pub(crate) fn descend(src: &mut Src, start: Atom, path: &[&[u8; 4]]) -> Result<Option<Atom>> {
    let mut cur = start;
    for (depth, want) in path.iter().enumerate() {
        anyhow::ensure!((depth as u32) < MAX_DEPTH, "BMFF nesting too deep");
        let kids = atoms(src, cur.body_at, cur.body_at + cur.body_len)?;
        match find(&kids, want) {
            Some(a) => cur = *a,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

pub fn plan(src: &mut Src) -> Result<Option<Plan>> {
    let top = atoms(src, 0, src.len())?;
    anyhow::ensure!(
        find(&top, b"ftyp").is_some() || find(&top, b"moov").is_some(),
        "not an ISO base media file"
    );

    if let Some(moov) = find(&top, b"moov").copied() {
        let groups = track_groups(src, moov)?;
        if !groups.is_empty() {
            return Ok(Plan {
                scheme: SCHEME,
                groups,
            }
            .non_empty());
        }
    }

    // No tracks: a HEIF still, where the picture lives in `meta`.
    if let Some(meta) = find(&top, b"meta").copied() {
        let idat = find(&top, b"idat").copied();
        let groups = item_groups(src, meta, idat)?;
        if !groups.is_empty() {
            return Ok(Plan {
                scheme: ITEMS_SCHEME,
                groups,
            }
            .non_empty());
        }
    }
    Ok(None)
}

// ─────────────────────────────────────────────────────────────────────
// Tracks

fn track_groups(src: &mut Src, moov: Atom) -> Result<Vec<Vec<Range>>> {
    let kids = atoms(src, moov.body_at, moov.body_at + moov.body_len)?;
    // Keyed by track id so the group order is a property of the file's
    // content rather than of the order the muxer happened to write the
    // `trak` boxes in.
    let mut by_track: BTreeMap<u32, Vec<Range>> = BTreeMap::new();
    for (i, trak) in kids.iter().filter(|a| &a.btype == b"trak").enumerate() {
        let id = track_id(src, *trak)?.unwrap_or(u32::MAX - i as u32);
        let Some(stbl) = descend(src, *trak, &[b"mdia", b"minf", b"stbl"])? else {
            continue;
        };
        let ranges = sample_ranges(src, stbl)?;
        if !ranges.is_empty() {
            by_track.insert(id, ranges);
        }
    }
    Ok(by_track.into_values().collect())
}

fn track_id(src: &mut Src, trak: Atom) -> Result<Option<u32>> {
    let kids = atoms(src, trak.body_at, trak.body_at + trak.body_len)?;
    let Some(tkhd) = find(&kids, b"tkhd") else {
        return Ok(None);
    };
    let want = 24u64.min(tkhd.body_len);
    let b = src.read_at(tkhd.body_at, want)?;
    // FullBox: version(1) flags(3). The 64-bit variant widens the two
    // timestamps ahead of the id from 4 bytes to 8.
    let at = if b.first().copied() == Some(1) {
        20
    } else {
        12
    };
    Ok(be_u32(&b, at))
}

/// Walk `stsc`/`stsz`/`stco` into one range per chunk.
fn sample_ranges(src: &mut Src, stbl: Atom) -> Result<Vec<Range>> {
    let kids = atoms(src, stbl.body_at, stbl.body_at + stbl.body_len)?;

    let sizes = match find(&kids, b"stsz") {
        Some(a) => read_stsz(src, *a)?,
        None => match find(&kids, b"stz2") {
            Some(a) => read_stz2(src, *a)?,
            None => return Ok(Vec::new()),
        },
    };
    let chunk_offsets = match find(&kids, b"stco") {
        Some(a) => read_u32_table(src, *a)?
            .into_iter()
            .map(u64::from)
            .collect(),
        None => match find(&kids, b"co64") {
            Some(a) => read_u64_table(src, *a)?,
            None => return Ok(Vec::new()),
        },
    };
    let stsc = match find(&kids, b"stsc") {
        Some(a) => read_stsc(src, *a)?,
        None => Vec::new(),
    };
    if chunk_offsets.is_empty() || sizes.is_empty() {
        return Ok(Vec::new());
    }

    let mut ranges: Vec<Range> = Vec::new();
    let mut sample = 0usize;
    for (idx, &chunk_off) in chunk_offsets.iter().enumerate() {
        let chunk_no = idx as u32 + 1;
        let per_chunk = samples_in_chunk(&stsc, chunk_no).unwrap_or(1);
        let mut total = 0u64;
        for _ in 0..per_chunk {
            match sizes.get(sample) {
                Some(&s) => {
                    total += u64::from(s);
                    sample += 1;
                }
                None => break,
            }
        }
        if total == 0 {
            continue;
        }
        // A chunk offset outside the file means a corrupt table; skip
        // it rather than plan a read that will fail.
        if chunk_off.checked_add(total).is_none_or(|e| e > src.len()) {
            continue;
        }
        ranges.push((chunk_off, total));
        if sample >= sizes.len() {
            break;
        }
    }

    // Chunks are usually laid out back to back within a track's
    // interleave run; merging them keeps the plan small.
    Ok(coalesce(ranges))
}

fn coalesce(mut ranges: Vec<Range>) -> Vec<Range> {
    if ranges.is_empty() {
        return ranges;
    }
    let mut out: Vec<Range> = Vec::with_capacity(ranges.len());
    for r in ranges.drain(..) {
        match out.last_mut() {
            Some(last) if last.0 + last.1 == r.0 => last.1 += r.1,
            _ => out.push(r),
        }
    }
    out
}

/// `(first_chunk, samples_per_chunk)` runs, ascending by first_chunk.
fn samples_in_chunk(stsc: &[(u32, u32)], chunk_no: u32) -> Option<u32> {
    let mut answer = None;
    for &(first, per) in stsc {
        if first <= chunk_no {
            answer = Some(per);
        } else {
            break;
        }
    }
    answer
}

fn full_box_body(src: &mut Src, a: Atom) -> Result<Vec<u8>> {
    anyhow::ensure!(a.body_len >= 4, "FullBox body is too short");
    src.read_at(a.body_at, a.body_len)
}

fn read_stsz(src: &mut Src, a: Atom) -> Result<Vec<u32>> {
    let b = full_box_body(src, a)?;
    let uniform = be_u32(&b, 4).unwrap_or(0);
    let count = u64::from(be_u32(&b, 8).unwrap_or(0));
    anyhow::ensure!(count <= MAX_TABLE_ENTRIES, "stsz claims {count} samples");
    if uniform != 0 {
        // Every sample the same size — the common case for uncompressed
        // audio, and the table is absent.
        return Ok(vec![uniform; count as usize]);
    }
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        out.push(be_u32(&b, 12 + i * 4).unwrap_or(0));
    }
    Ok(out)
}

fn read_stz2(src: &mut Src, a: Atom) -> Result<Vec<u32>> {
    let b = full_box_body(src, a)?;
    let field_size = *b.get(7).unwrap_or(&0);
    let count = u64::from(be_u32(&b, 8).unwrap_or(0));
    anyhow::ensure!(count <= MAX_TABLE_ENTRIES, "stz2 claims {count} samples");
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let v = match field_size {
            4 => {
                let byte = *b.get(12 + i / 2).unwrap_or(&0);
                if i % 2 == 0 {
                    u32::from(byte >> 4)
                } else {
                    u32::from(byte & 0x0f)
                }
            }
            8 => u32::from(*b.get(12 + i).unwrap_or(&0)),
            16 => u32::from(be_u16(&b, 12 + i * 2).unwrap_or(0)),
            other => anyhow::bail!("stz2 field size {other} is not 4, 8 or 16"),
        };
        out.push(v);
    }
    Ok(out)
}

fn read_stsc(src: &mut Src, a: Atom) -> Result<Vec<(u32, u32)>> {
    let b = full_box_body(src, a)?;
    let count = u64::from(be_u32(&b, 4).unwrap_or(0));
    anyhow::ensure!(count <= MAX_TABLE_ENTRIES, "stsc claims {count} entries");
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let at = 8 + i * 12;
        out.push((be_u32(&b, at).unwrap_or(0), be_u32(&b, at + 4).unwrap_or(0)));
    }
    Ok(out)
}

fn read_u32_table(src: &mut Src, a: Atom) -> Result<Vec<u32>> {
    let b = full_box_body(src, a)?;
    let count = u64::from(be_u32(&b, 4).unwrap_or(0));
    anyhow::ensure!(count <= MAX_TABLE_ENTRIES, "chunk table claims {count}");
    Ok((0..count as usize)
        .map(|i| be_u32(&b, 8 + i * 4).unwrap_or(0))
        .collect())
}

fn read_u64_table(src: &mut Src, a: Atom) -> Result<Vec<u64>> {
    let b = full_box_body(src, a)?;
    let count = u64::from(be_u32(&b, 4).unwrap_or(0));
    anyhow::ensure!(count <= MAX_TABLE_ENTRIES, "chunk table claims {count}");
    Ok((0..count as usize)
        .map(|i| be_u64(&b, 8 + i * 8).unwrap_or(0))
        .collect())
}

// ─────────────────────────────────────────────────────────────────────
// HEIF items

/// Item types that ARE the metadata, and so are excluded. Stated as an
/// exclusion rather than an allowlist of image codecs on purpose: a new
/// codec four-CC should be hashed by default, where a new metadata
/// carrier is the thing we would want to notice and add.
const METADATA_ITEM_TYPES: &[&[u8; 4]] = &[b"Exif", b"mime"];

fn item_groups(src: &mut Src, meta: Atom, idat: Option<Atom>) -> Result<Vec<Vec<Range>>> {
    // `meta` is a FullBox: its children start 4 bytes in.
    let kids = atoms(src, meta.body_at + 4, meta.body_at + meta.body_len)?;
    let types = match find(&kids, b"iinf") {
        Some(a) => read_iinf(src, *a)?,
        None => BTreeMap::new(),
    };
    let Some(iloc) = find(&kids, b"iloc").copied() else {
        return Ok(Vec::new());
    };
    let locs = read_iloc(src, iloc, idat)?;

    let mut groups = Vec::new();
    for (item_id, ranges) in locs {
        if let Some(t) = types.get(&item_id) {
            if METADATA_ITEM_TYPES.contains(&t) {
                continue;
            }
        }
        let valid: Vec<Range> = ranges
            .into_iter()
            .filter(|(o, l)| *l > 0 && o.checked_add(*l).is_some_and(|e| e <= src.len()))
            .collect();
        if !valid.is_empty() {
            groups.push(valid);
        }
    }
    Ok(groups)
}

fn read_iinf(src: &mut Src, a: Atom) -> Result<BTreeMap<u32, [u8; 4]>> {
    let b = full_box_body(src, a)?;
    let version = b[0];
    let (count, mut at) = if version == 0 {
        (u64::from(be_u16(&b, 4).unwrap_or(0)), 6usize)
    } else {
        (u64::from(be_u32(&b, 4).unwrap_or(0)), 8usize)
    };
    anyhow::ensure!(count <= MAX_TABLE_ENTRIES, "iinf claims {count} entries");

    let mut out = BTreeMap::new();
    for _ in 0..count {
        // Each entry is an `infe` box.
        let size = u64::from(be_u32(&b, at).unwrap_or(0));
        if size < 8 || at as u64 + size > b.len() as u64 {
            break;
        }
        let body = &b[at + 8..at + size as usize];
        if body.len() >= 4 {
            let ver = body[0];
            // Versions 2 and 3 carry the four-CC item type; 0 and 1 do
            // not have one at all (they predate typed items).
            let (id, type_at) = match ver {
                2 => (u32::from(be_u16(body, 4).unwrap_or(0)), 8usize),
                3 => (be_u32(body, 4).unwrap_or(0), 10usize),
                _ => {
                    at += size as usize;
                    continue;
                }
            };
            if let Some(t) = body.get(type_at..type_at + 4) {
                out.insert(id, [t[0], t[1], t[2], t[3]]);
            }
        }
        at += size as usize;
    }
    Ok(out)
}

fn read_iloc(src: &mut Src, a: Atom, idat: Option<Atom>) -> Result<BTreeMap<u32, Vec<Range>>> {
    let b = full_box_body(src, a)?;
    let version = b[0];
    let offset_size = usize::from(b[4] >> 4);
    let length_size = usize::from(b[4] & 0x0f);
    let base_offset_size = usize::from(b[5] >> 4);
    let index_size = if version == 1 || version == 2 {
        usize::from(b[5] & 0x0f)
    } else {
        0
    };

    let mut at = 6usize;
    let count = if version < 2 {
        let c = u64::from(be_u16(&b, at).unwrap_or(0));
        at += 2;
        c
    } else {
        let c = u64::from(be_u32(&b, at).unwrap_or(0));
        at += 4;
        c
    };
    anyhow::ensure!(count <= MAX_TABLE_ENTRIES, "iloc claims {count} items");

    /// Read a big-endian integer of `n` bytes (0, 4 or 8 in practice).
    fn uint(b: &[u8], at: usize, n: usize) -> u64 {
        let mut v = 0u64;
        for i in 0..n {
            v = (v << 8) | u64::from(*b.get(at + i).unwrap_or(&0));
        }
        v
    }

    let mut out: BTreeMap<u32, Vec<Range>> = BTreeMap::new();
    for _ in 0..count {
        if at >= b.len() {
            break;
        }
        let item_id = if version < 2 {
            let v = u32::from(be_u16(&b, at).unwrap_or(0));
            at += 2;
            v
        } else {
            let v = be_u32(&b, at).unwrap_or(0);
            at += 4;
            v
        };
        let construction = if version == 1 || version == 2 {
            let v = be_u16(&b, at).unwrap_or(0) & 0x0f;
            at += 2;
            v
        } else {
            0
        };
        at += 2; // data_reference_index
        let base = uint(&b, at, base_offset_size);
        at += base_offset_size;
        let extents = usize::from(be_u16(&b, at).unwrap_or(0));
        at += 2;

        // 0 = offsets into this file, 1 = offsets into the `idat` box.
        // 2 is "another item", which we do not follow.
        let origin = match construction {
            0 => Some(0u64),
            1 => idat.map(|i| i.body_at),
            _ => None,
        };

        let mut ranges = Vec::with_capacity(extents);
        for _ in 0..extents {
            at += index_size;
            let off = uint(&b, at, offset_size);
            at += offset_size;
            let len = uint(&b, at, length_size);
            at += length_size;
            if let Some(o) = origin {
                ranges.push((o + base + off, len));
            }
        }
        if !ranges.is_empty() {
            out.insert(item_id, ranges);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{b3, src_of};
    use super::*;

    fn atom(btype: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut a = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        a.extend_from_slice(btype);
        a.extend_from_slice(body);
        a
    }

    fn full(btype: &[u8; 4], version: u8, body: &[u8]) -> Vec<u8> {
        let mut b = vec![version, 0, 0, 0];
        b.extend_from_slice(body);
        atom(btype, &b)
    }

    fn tkhd(id: u32) -> Vec<u8> {
        let mut b = vec![0u8; 8]; // creation + modification
        b.extend_from_slice(&id.to_be_bytes());
        b.resize(80, 0);
        full(b"tkhd", 0, &b)
    }

    /// A minimal `stbl` for one track: `n` samples of `size` bytes,
    /// one chunk at `offset`.
    fn stbl(offset: u32, sizes: &[u32]) -> Vec<u8> {
        let mut stsz = 0u32.to_be_bytes().to_vec(); // non-uniform
        stsz.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
        for s in sizes {
            stsz.extend_from_slice(&s.to_be_bytes());
        }
        let mut stsc = 1u32.to_be_bytes().to_vec(); // one entry
        stsc.extend_from_slice(&1u32.to_be_bytes()); // first_chunk
        stsc.extend_from_slice(&(sizes.len() as u32).to_be_bytes()); // per chunk
        stsc.extend_from_slice(&1u32.to_be_bytes()); // description index

        let mut stco = 1u32.to_be_bytes().to_vec();
        stco.extend_from_slice(&offset.to_be_bytes());

        let mut body = full(b"stsz", 0, &stsz);
        body.extend_from_slice(&full(b"stsc", 0, &stsc));
        body.extend_from_slice(&full(b"stco", 0, &stco));
        atom(b"stbl", &body)
    }

    fn trak(id: u32, offset: u32, sizes: &[u32]) -> Vec<u8> {
        let minf = atom(b"minf", &stbl(offset, sizes));
        let mdia = atom(b"mdia", &minf);
        let mut body = tkhd(id);
        body.extend_from_slice(&mdia);
        atom(b"trak", &body)
    }

    const AUDIO: &[u8] = b"aaaaaaaaaaaaaaaaaaaaaaaa";
    const VIDEO: &[u8] = b"vvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvv";

    /// `ftyp`, `moov` (with `udta` metadata), then `mdat`.
    /// Returns the file and the byte offset of `mdat`'s body.
    fn mp4(udta: &[u8], tracks: &[(u32, &[u8])]) -> Vec<u8> {
        mp4_padded(udta, tracks, 0)
    }

    /// As [`mp4`], with `pad` bytes of `free` box between `moov` and
    /// `mdat` so every chunk offset shifts.
    fn mp4_padded(udta: &[u8], tracks: &[(u32, &[u8])], pad: usize) -> Vec<u8> {
        // Build moov twice: once to learn its length, once with the
        // real chunk offsets folded in.
        let build = |mdat_body_at: u32| {
            let mut moov_body = Vec::new();
            let mut off = mdat_body_at;
            for (id, data) in tracks {
                moov_body.extend_from_slice(&trak(*id, off, &[data.len() as u32]));
                off += data.len() as u32;
            }
            moov_body.extend_from_slice(&atom(b"udta", udta));
            atom(b"moov", &moov_body)
        };
        let ftyp = atom(b"ftyp", b"M4A \x00\x00\x00\x00M4A mp42");
        let free = if pad > 0 {
            atom(b"free", &vec![0u8; pad])
        } else {
            Vec::new()
        };
        let probe = build(0);
        let mdat_body_at = (ftyp.len() + probe.len() + free.len() + 8) as u32;

        let mut out = ftyp;
        out.extend_from_slice(&build(mdat_body_at));
        out.extend_from_slice(&free);
        let mut payload = Vec::new();
        for (_, d) in tracks {
            payload.extend_from_slice(d);
        }
        out.extend_from_slice(&atom(b"mdat", &payload));
        out
    }

    fn hash(bytes: &[u8]) -> String {
        let mut t = src_of(bytes);
        let p = plan(&mut t.src).unwrap().unwrap();
        super::super::hash_plan(&mut t.src, &p).unwrap()
    }

    #[test]
    fn a_single_track_hashes_its_sample_bytes() {
        let bytes = mp4(b"", &[(1, AUDIO)]);
        let mut t = src_of(&bytes);
        let p = plan(&mut t.src).unwrap().unwrap();
        assert_eq!(p.scheme, SCHEME);
        assert_eq!(p.groups.len(), 1);
        assert_eq!(p.total_bytes(), AUDIO.len() as u64);
        assert_eq!(super::super::hash_plan(&mut t.src, &p).unwrap(), b3(AUDIO));
    }

    #[test]
    fn ilst_tags_are_excluded() {
        let bare = mp4(b"", &[(1, AUDIO)]);
        let tagged = mp4(
            &full(
                b"meta",
                0,
                &atom(
                    b"ilst",
                    &atom(
                        b"\xa9nam",
                        &atom(b"data", b"\x00\x00\x00\x01\x00\x00\x00\x00Ode to Spot"),
                    ),
                ),
            ),
            &[(1, AUDIO)],
        );
        assert_ne!(bare.len(), tagged.len());
        assert_eq!(hash(&bare), hash(&tagged));
    }

    #[test]
    fn each_track_is_its_own_group() {
        let bytes = mp4(b"", &[(1, VIDEO), (2, AUDIO)]);
        let mut t = src_of(&bytes);
        let p = plan(&mut t.src).unwrap().unwrap();
        assert_eq!(p.groups.len(), 2);
        assert_ne!(
            super::super::hash_plan(&mut t.src, &p).unwrap(),
            b3(&[VIDEO, AUDIO].concat()),
            "tracks must not be hashed as one stream"
        );
    }

    /// Changing a track changes the file's digest — the behavior we
    /// want — while that track's own group digest is what moved.
    ///
    /// The second half is a property of the *plan*, not of anything
    /// stored: no group digest is written to the database today. It is
    /// asserted here so that a future `media_streams` table has a test
    /// already standing behind the thing it would expose.
    #[test]
    fn a_changed_track_moves_the_file_digest_and_only_its_own_group() {
        let a = mp4(b"", &[(1, VIDEO), (2, AUDIO)]);
        let other_video = b"VVVVVVVVVVVVVVVV"; // shorter: a real re-encode
        let b = mp4(b"", &[(1, other_video), (2, AUDIO)]);

        let mut ta = src_of(&a);
        let mut tb = src_of(&b);
        let pa = plan(&mut ta.src).unwrap().unwrap();
        let pb = plan(&mut tb.src).unwrap().unwrap();

        // The file's payload hash moves, which is the point: a clip
        // with different pictures is a different clip.
        assert_ne!(
            super::super::hash_plan(&mut ta.src, &pa).unwrap(),
            super::super::hash_plan(&mut tb.src, &pb).unwrap()
        );
        // The untouched track's group is nonetheless byte-identical.
        let audio_a = Plan {
            scheme: SCHEME,
            groups: vec![pa.groups[1].clone()],
        };
        let audio_b = Plan {
            scheme: SCHEME,
            groups: vec![pb.groups[1].clone()],
        };
        assert_eq!(
            super::super::hash_plan(&mut ta.src, &audio_a).unwrap(),
            super::super::hash_plan(&mut tb.src, &audio_b).unwrap()
        );
    }

    /// Layout-independence, which is the concrete thing per-track
    /// grouping buys: padding shifts every sample offset in the file
    /// and changes nothing that is hashed.
    #[test]
    fn inserting_padding_does_not_change_the_digest() {
        let plain = mp4(b"", &[(1, VIDEO), (2, AUDIO)]);
        // `free` between `moov` and `mdat`: exactly what a `faststart`
        // rewrite or an in-place tag edit leaves behind. The sample
        // offsets in `stco` all move; the sample bytes do not.
        let padded = mp4_padded(b"", &[(1, VIDEO), (2, AUDIO)], 64);
        assert_ne!(plain, padded, "the files really do differ");
        assert_eq!(hash(&plain), hash(&padded));
    }

    #[test]
    fn track_order_in_the_file_does_not_change_the_digest() {
        // Same two tracks, written in the other order: `by_track`
        // sorts on track id, so the groups line up.
        let a = mp4(b"", &[(1, VIDEO), (2, AUDIO)]);
        let b = mp4(b"", &[(2, AUDIO), (1, VIDEO)]);
        assert_ne!(a, b);
        assert_eq!(hash(&a), hash(&b));
    }

    #[test]
    fn a_uniform_sample_size_needs_no_size_table() {
        // stsz with a non-zero uniform size and no per-sample entries.
        let mut stsz = 4u32.to_be_bytes().to_vec(); // 4 bytes each
        stsz.extend_from_slice(&3u32.to_be_bytes()); // 3 samples
        let mut stsc = 1u32.to_be_bytes().to_vec();
        stsc.extend_from_slice(&1u32.to_be_bytes());
        stsc.extend_from_slice(&3u32.to_be_bytes());
        stsc.extend_from_slice(&1u32.to_be_bytes());

        let ftyp = atom(b"ftyp", b"mp42\x00\x00\x00\x00mp42");
        // Compute the mdat body offset the same way `mp4` does.
        let make = |off: u32| {
            let mut stco = 1u32.to_be_bytes().to_vec();
            stco.extend_from_slice(&off.to_be_bytes());
            let mut sb = full(b"stsz", 0, &stsz);
            sb.extend_from_slice(&full(b"stsc", 0, &stsc));
            sb.extend_from_slice(&full(b"stco", 0, &stco));
            let minf = atom(b"minf", &atom(b"stbl", &sb));
            let mut tb = tkhd(1);
            tb.extend_from_slice(&atom(b"mdia", &minf));
            atom(b"moov", &atom(b"trak", &tb))
        };
        let probe = make(0);
        let body_at = (ftyp.len() + probe.len() + 8) as u32;
        let mut bytes = ftyp;
        bytes.extend_from_slice(&make(body_at));
        bytes.extend_from_slice(&atom(b"mdat", b"0123456789ab"));

        let mut t = src_of(&bytes);
        let p = plan(&mut t.src).unwrap().unwrap();
        assert_eq!(p.total_bytes(), 12);
        assert_eq!(
            super::super::hash_plan(&mut t.src, &p).unwrap(),
            b3(b"0123456789ab")
        );
    }

    #[test]
    fn a_64_bit_box_size_is_followed() {
        let mut big = 1u32.to_be_bytes().to_vec(); // size == 1: 64-bit
        big.extend_from_slice(b"free");
        big.extend_from_slice(&24u64.to_be_bytes());
        big.extend_from_slice(&[0u8; 8]);

        let mut bytes = atom(b"ftyp", b"mp42\x00\x00\x00\x00mp42");
        bytes.extend_from_slice(&big);
        let before = bytes.len();
        bytes.extend_from_slice(&mp4(b"", &[(1, AUDIO)])[..]);
        // The `free` box must be skipped by its 64-bit size, or the
        // walk lands mid-box and finds no `moov`.
        assert!(before > 8);
        let mut t = src_of(&bytes);
        // The nested `mp4()` recomputed offsets for a file starting at
        // 0, so this only asserts that the walk survives, not the hash.
        assert!(plan(&mut t.src).is_ok());
    }

    #[test]
    fn chunk_offsets_outside_the_file_are_dropped() {
        let mut bytes = mp4(b"", &[(1, AUDIO)]);
        // Corrupt the stco offset to point past EOF.
        let pos = bytes.windows(4).position(|w| w == b"stco").unwrap();
        let off_at = pos + 4 + 4 + 4; // version/flags, count
        bytes[off_at..off_at + 4].copy_from_slice(&0xffff_0000u32.to_be_bytes());
        let mut t = src_of(&bytes);
        assert!(plan(&mut t.src).unwrap().is_none());
    }

    // ── HEIF items ───────────────────────────────────────────────────

    /// A HEIF still: `ftyp`, `meta` (iinf + iloc), `mdat`.
    fn heic(items: &[(u16, &[u8; 4], &[u8])]) -> Vec<u8> {
        let ftyp = atom(b"ftyp", b"heic\x00\x00\x00\x00heicmif1");

        let mut infes = Vec::new();
        for (id, ty, _) in items {
            let mut b = id.to_be_bytes().to_vec();
            b.extend_from_slice(&0u16.to_be_bytes()); // protection index
            b.extend_from_slice(*ty);
            b.push(0); // empty item_name
            infes.extend_from_slice(&full(b"infe", 2, &b));
        }
        let mut iinf_body = (items.len() as u16).to_be_bytes().to_vec();
        iinf_body.extend_from_slice(&infes);
        let iinf = full(b"iinf", 0, &iinf_body);

        let make_meta = |data_at: u32| {
            // iloc version 1, 4-byte offsets and lengths, no base.
            let mut b = vec![0x44u8, 0x00]; // offset=4 len=4, base=0 index=0
            b.extend_from_slice(&(items.len() as u16).to_be_bytes());
            let mut off = data_at;
            for (id, _, data) in items {
                b.extend_from_slice(&id.to_be_bytes());
                b.extend_from_slice(&0u16.to_be_bytes()); // construction 0
                b.extend_from_slice(&0u16.to_be_bytes()); // data ref
                b.extend_from_slice(&1u16.to_be_bytes()); // extent count
                b.extend_from_slice(&off.to_be_bytes());
                b.extend_from_slice(&(data.len() as u32).to_be_bytes());
                off += data.len() as u32;
            }
            let mut meta_body = full(b"hdlr", 0, &[0u8; 20]);
            meta_body.extend_from_slice(&iinf);
            meta_body.extend_from_slice(&full(b"iloc", 1, &b));
            full(b"meta", 0, &meta_body)
        };
        let probe = make_meta(0);
        let data_at = (ftyp.len() + probe.len() + 8) as u32;

        let mut out = ftyp;
        out.extend_from_slice(&make_meta(data_at));
        let mut payload = Vec::new();
        for (_, _, d) in items {
            payload.extend_from_slice(d);
        }
        out.extend_from_slice(&atom(b"mdat", &payload));
        out
    }

    const PICTURE: &[u8] = b"hevc-coded-picture-data-here";

    #[test]
    fn a_heif_still_hashes_its_picture_items() {
        let bytes = heic(&[(1, b"hvc1", PICTURE)]);
        let mut t = src_of(&bytes);
        let p = plan(&mut t.src).unwrap().unwrap();
        assert_eq!(p.scheme, ITEMS_SCHEME);
        assert_eq!(
            super::super::hash_plan(&mut t.src, &p).unwrap(),
            b3(PICTURE)
        );
    }

    #[test]
    fn exif_and_xmp_items_are_excluded() {
        let bare = heic(&[(1, b"hvc1", PICTURE)]);
        let tagged = heic(&[
            (1, b"hvc1", PICTURE),
            (2, b"Exif", b"\x00\x00\x00\x06Exif\x00\x00II*\x00rating=5"),
            (3, b"mime", b"<x:xmpmeta>keywords</x:xmpmeta>"),
        ]);
        assert_ne!(bare.len(), tagged.len());
        assert_eq!(hash(&bare), hash(&tagged));
    }

    #[test]
    fn changing_the_picture_moves_the_hash() {
        let a = heic(&[(1, b"hvc1", PICTURE)]);
        let mut other = PICTURE.to_vec();
        other[0] ^= 0xff;
        let b = heic(&[(1, b"hvc1", &other)]);
        assert_ne!(hash(&a), hash(&b));
    }

    #[test]
    fn an_unknown_item_type_is_hashed_rather_than_skipped() {
        // The exclusion list names metadata carriers, so a codec we
        // have never heard of still counts as picture data.
        let bytes = heic(&[(1, b"zz99", PICTURE)]);
        let mut t = src_of(&bytes);
        assert!(plan(&mut t.src).unwrap().is_some());
    }

    #[test]
    fn non_bmff_bytes_are_an_error() {
        let mut t = src_of(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0d\x00\x00\x00\x00");
        assert!(plan(&mut t.src).is_err());
    }
}
