//! RIFF payloads: WAV's `data` chunk and AVI's `movi` list.
//!
//! RIFF is the easiest case in this module and shows the shape of all
//! of them. The file is a flat sequence of `id[4] len[u32le] bytes`
//! chunks inside one outer `RIFF` chunk; the signal lives in exactly
//! one of them, and everything a tagger writes — `LIST INFO`, `id3 `,
//! Broadcast-Wave's `bext`, `JUNK` padding a DAW left behind — lives in
//! the others.
//!
//! What that buys, concretely: adding an artist tag to a WAV appends a
//! `LIST INFO` chunk and rewrites the outer size field, so `blake3`
//! moves. The samples did not, so `payload_blake3` holds.
//!
//! AVI is the same walk one level deeper. `movi` is a `LIST` rather
//! than a plain chunk, so its first four bytes are the list type rather
//! than data. Excluding the sibling `idx1` matters more than it looks:
//! it is a table of *file offsets*, so inserting any metadata chunk
//! ahead of `movi` rewrites every entry in it even though no frame
//! changed.

use anyhow::Result;

use super::{le_u32, Plan, Range, Src};

/// The `data` chunk of a WAVE file.
pub const WAV_SCHEME: &str = "wav.data.v1";
/// The `movi` list of an AVI file.
pub const AVI_SCHEME: &str = "avi.movi.v1";

/// Header bytes per chunk: `id[4] len[4]`.
const CHUNK_HEADER: u64 = 8;

/// A chunk found by [`chunks`]: where its data starts and how long it
/// is, with the padding byte already excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    pub id: [u8; 4],
    pub data_at: u64,
    pub data_len: u64,
}

/// Walk the chunks of the outer RIFF form, top level only.
///
/// Stops at the first structurally impossible chunk rather than
/// erroring: RIFF files in the wild routinely carry trailing garbage
/// after the last real chunk, and refusing the whole file over it would
/// throw away a payload we can compute perfectly well.
pub fn chunks(src: &mut Src) -> Result<Vec<Chunk>> {
    let head = src.read_upto(0, 12)?;
    anyhow::ensure!(
        head.len() == 12 && head.starts_with(b"RIFF"),
        "not a RIFF file"
    );

    // The outer size counts everything after the size field itself.
    // Trust the file length over it when they disagree — a truncated
    // download keeps its original header.
    let declared_end = 8u64.saturating_add(u64::from(
        le_u32(&head, 4).ok_or_else(|| anyhow::anyhow!("short RIFF header"))?,
    ));
    let end = declared_end.min(src.len());

    let mut out = Vec::new();
    let mut at = 12u64; // past `RIFF` + size + form type
    while at + CHUNK_HEADER <= end {
        let hdr = src.read_at(at, CHUNK_HEADER)?;
        let len = u64::from(le_u32(&hdr, 4).ok_or_else(|| anyhow::anyhow!("short chunk header"))?);
        let data_at = at + CHUNK_HEADER;
        if data_at + len > src.len() {
            break;
        }
        out.push(Chunk {
            id: [hdr[0], hdr[1], hdr[2], hdr[3]],
            data_at,
            data_len: len,
        });
        // Chunks are padded to an even length; the pad byte is not part
        // of the data and is not hashed.
        at = data_at + len + (len & 1);
    }
    Ok(out)
}

/// The sample bytes of a WAVE file: the `data` chunk and nothing else.
pub fn plan_wav(src: &mut Src) -> Result<Option<Plan>> {
    let found = chunks(src)?
        .into_iter()
        .find(|c| &c.id == b"data")
        .map(|c| (c.data_at, c.data_len));
    Ok(found.and_then(|r: Range| Plan::flat(WAV_SCHEME, vec![r]).non_empty()))
}

/// The frame data of an AVI file: the `movi` list's contents, past its
/// four-byte list type.
pub fn plan_avi(src: &mut Src) -> Result<Option<Plan>> {
    for c in chunks(src)? {
        if &c.id != b"LIST" || c.data_len < 4 {
            continue;
        }
        let form = src.read_at(c.data_at, 4)?;
        if form.as_slice() == b"movi" {
            let r: Range = (c.data_at + 4, c.data_len - 4);
            return Ok(Plan::flat(AVI_SCHEME, vec![r]).non_empty());
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{b3, src_of};
    use super::*;

    /// Assemble a RIFF file from `(id, data)` pairs.
    fn riff(form: &[u8; 4], parts: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut body = form.to_vec();
        for (id, data) in parts {
            body.extend_from_slice(*id);
            body.extend_from_slice(&(data.len() as u32).to_le_bytes());
            body.extend_from_slice(data);
            if data.len() % 2 == 1 {
                body.push(0); // pad byte
            }
        }
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    const FMT: &[u8] = b"\x01\x00\x02\x00\x44\xac\x00\x00\x10\xb1\x02\x00\x04\x00\x10\x00";
    const SAMPLES: &[u8] = b"\x00\x01\x02\x03\x04\x05\x06\x07samples-go-here";

    #[test]
    fn wav_payload_is_exactly_the_data_chunk() {
        let bytes = riff(b"WAVE", &[(b"fmt ", FMT), (b"data", SAMPLES)]);
        let mut t = src_of(&bytes);
        let plan = plan_wav(&mut t.src).unwrap().unwrap();
        assert_eq!(plan.scheme, WAV_SCHEME);
        assert_eq!(plan.total_bytes(), SAMPLES.len() as u64);
        assert_eq!(
            super::super::hash_plan(&mut t.src, &plan).unwrap(),
            b3(SAMPLES),
            "a one-group plan must equal b3sum of the extracted chunk"
        );
    }

    /// The behavior the column exists for: tag it, and only the file
    /// hash moves.
    #[test]
    fn tagging_a_wav_leaves_the_payload_hash_alone() {
        let plain = riff(b"WAVE", &[(b"fmt ", FMT), (b"data", SAMPLES)]);
        let tagged = riff(
            b"WAVE",
            &[
                (b"fmt ", FMT),
                // A real tagger writes INFO *before* data as often as
                // after, which shifts every later offset.
                (b"LIST", b"INFOIART\x08\x00\x00\x00Picard\x00\x00"),
                (b"data", SAMPLES),
                (b"id3 ", b"ID3\x04\x00\x00\x00\x00\x00\x00"),
            ],
        );
        assert_ne!(plain, tagged, "the files really do differ");

        let mut a = src_of(&plain);
        let mut b = src_of(&tagged);
        let pa = plan_wav(&mut a.src).unwrap().unwrap();
        let pb = plan_wav(&mut b.src).unwrap().unwrap();
        assert_eq!(
            super::super::hash_plan(&mut a.src, &pa).unwrap(),
            super::super::hash_plan(&mut b.src, &pb).unwrap()
        );
    }

    #[test]
    fn changing_one_sample_does_move_the_payload_hash() {
        let mut edited = SAMPLES.to_vec();
        edited[0] ^= 0xff;
        let mut a = src_of(&riff(b"WAVE", &[(b"data", SAMPLES)]));
        let mut b = src_of(&riff(b"WAVE", &[(b"data", &edited)]));
        let pa = plan_wav(&mut a.src).unwrap().unwrap();
        let pb = plan_wav(&mut b.src).unwrap().unwrap();
        assert_ne!(
            super::super::hash_plan(&mut a.src, &pa).unwrap(),
            super::super::hash_plan(&mut b.src, &pb).unwrap()
        );
    }

    #[test]
    fn odd_length_chunks_are_padded_but_the_pad_is_not_hashed() {
        let odd: &[u8] = b"abc";
        let bytes = riff(b"WAVE", &[(b"junk", odd), (b"data", SAMPLES)]);
        let mut t = src_of(&bytes);
        // The walk has to skip the pad byte or it desynchronizes and
        // never finds `data`.
        let plan = plan_wav(&mut t.src).unwrap().unwrap();
        assert_eq!(plan.total_bytes(), SAMPLES.len() as u64);
    }

    #[test]
    fn avi_payload_is_movi_without_its_list_type() {
        let mut movi = b"movi".to_vec();
        movi.extend_from_slice(b"00dc\x08\x00\x00\x00frame-01");
        let bytes = riff(
            b"AVI ",
            &[
                (b"LIST", b"hdrlavih\x04\x00\x00\x00\x00\x00\x00\x00"),
                (b"LIST", &movi),
                // The index is offsets, not frames: excluded.
                (
                    b"idx1",
                    b"00dc\x10\x00\x00\x00\x04\x00\x00\x00\x08\x00\x00\x00",
                ),
            ],
        );
        let mut t = src_of(&bytes);
        let plan = plan_avi(&mut t.src).unwrap().unwrap();
        assert_eq!(plan.scheme, AVI_SCHEME);
        assert_eq!(
            super::super::hash_plan(&mut t.src, &plan).unwrap(),
            b3(b"00dc\x08\x00\x00\x00frame-01")
        );
    }

    #[test]
    fn a_wav_with_no_data_chunk_plans_nothing() {
        let mut t = src_of(&riff(b"WAVE", &[(b"fmt ", FMT)]));
        assert!(plan_wav(&mut t.src).unwrap().is_none());
    }

    #[test]
    fn an_empty_data_chunk_plans_nothing_rather_than_hashing_zero_bytes() {
        let mut t = src_of(&riff(b"WAVE", &[(b"data", b"")]));
        assert!(plan_wav(&mut t.src).unwrap().is_none());
    }

    #[test]
    fn trailing_garbage_stops_the_walk_instead_of_failing_the_file() {
        let mut bytes = riff(b"WAVE", &[(b"data", SAMPLES)]);
        // A chunk header claiming far more bytes than remain.
        bytes.extend_from_slice(b"junk\xff\xff\xff\x7f");
        let mut t = src_of(&bytes);
        let plan = plan_wav(&mut t.src).unwrap().unwrap();
        assert_eq!(plan.total_bytes(), SAMPLES.len() as u64);
    }

    #[test]
    fn a_truncated_file_keeps_the_chunks_that_are_fully_present() {
        let full = riff(b"WAVE", &[(b"fmt ", FMT), (b"data", SAMPLES)]);
        // Cut into the middle of `data`; its header still claims the
        // full length, so it must not be reported.
        let mut t = src_of(&full[..full.len() - 5]);
        assert!(plan_wav(&mut t.src).unwrap().is_none());
    }

    #[test]
    fn non_riff_bytes_are_an_error_the_caller_turns_into_null() {
        let mut t = src_of(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0d");
        assert!(plan_wav(&mut t.src).is_err());
    }
}
