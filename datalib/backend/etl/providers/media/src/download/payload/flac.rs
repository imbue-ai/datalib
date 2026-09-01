//! FLAC payload: the audio frames, past every metadata block.
//!
//! FLAC keeps its metadata in a clean prefix — a chain of typed blocks
//! between the `fLaC` magic and the first audio frame — so the split is
//! unusually crisp. `VORBIS_COMMENT` (the tags), `PICTURE` (cover art,
//! often the largest thing in the file), `SEEKTABLE`, `PADDING` and
//! `CUESHEET` all live there and all get skipped; the frames are
//! everything after.
//!
//! # The `STREAMINFO` MD5 we deliberately do not use
//!
//! FLAC files already carry an identity hash: `STREAMINFO` holds an MD5
//! of the *decoded* samples, which survives re-encoding at a different
//! compression level — something [`SCHEME`] does not. It is tempting.
//!
//! We do not use it, for one reason: it would put a different digest
//! algorithm, over a different input, into the same
//! `media_items.payload_blake3` column as every other container. Two
//! rows in one column have to mean the same kind of thing, or
//! `GROUP BY payload_blake3` quietly stops being a valid query. If we
//! ever want decoded-sample identity it belongs in its own column,
//! populated for the formats that can supply it — which is a real
//! follow-up, not a rejection.

use anyhow::Result;

use super::mp3::APE_FLAGS_AT;
use super::{be_u32, Plan, Src};

/// Audio frames, metadata blocks excluded.
pub const SCHEME: &str = "flac.frames.v1";

/// Header bytes per metadata block: `flags+type[1] len[3]`.
const BLOCK_HEADER: u64 = 4;

pub fn plan(src: &mut Src) -> Result<Option<Plan>> {
    // An ID3v2 tag ahead of the magic is out of spec but common —
    // enough taggers write one that refusing those files would be a
    // real hole.
    let base = super::mp3::audio_start(src)?;
    let magic = src.read_upto(base, 4)?;
    anyhow::ensure!(magic == b"fLaC", "not a FLAC stream");

    let mut at = base + 4;
    loop {
        let hdr = src.read_upto(at, BLOCK_HEADER)?;
        anyhow::ensure!(hdr.len() == 4, "truncated FLAC metadata block header");
        let last = hdr[0] & 0x80 != 0;
        // A 24-bit big-endian length, which is a 32-bit read with the
        // type byte masked off.
        let len = u64::from(be_u32(&hdr, 0).unwrap_or(0) & 0x00ff_ffff);
        at = at
            .checked_add(BLOCK_HEADER + len)
            .filter(|end| *end <= src.len())
            .ok_or_else(|| anyhow::anyhow!("FLAC metadata block runs past end of file"))?;
        if last {
            break;
        }
    }

    // Trailing ID3v1 / APEv2, same as MP3: some taggers append them to
    // FLAC too.
    let end = trailing_tag_start(src, at)?;
    if end <= at {
        return Ok(None);
    }
    Ok(Plan::flat(SCHEME, vec![(at, end - at)]).non_empty())
}

fn trailing_tag_start(src: &mut Src, floor: u64) -> Result<u64> {
    let mut end = src.len();
    if end >= floor + 128 && src.read_at(end - 128, 3)?.as_slice() == b"TAG" {
        end -= 128;
    }
    if end >= floor + 32 {
        let foot = src.read_at(end - 32, 32)?;
        if foot.starts_with(b"APETAGEX") {
            let size = u64::from(super::le_u32(&foot, 12).unwrap_or(0));
            let flags = super::le_u32(&foot, APE_FLAGS_AT).unwrap_or(0);
            let header = if flags & 0x8000_0000 != 0 { 32 } else { 0 };
            let total = size.saturating_add(header);
            if total > 0 && end.saturating_sub(total) >= floor {
                end -= total;
            }
        }
    }
    Ok(end)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{b3, src_of};
    use super::*;

    fn block(kind: u8, last: bool, body: &[u8]) -> Vec<u8> {
        let mut b = vec![if last { kind | 0x80 } else { kind }];
        let n = body.len() as u32;
        b.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
        b.extend_from_slice(body);
        b
    }

    const STREAMINFO: &[u8] = &[0u8; 34];
    const FRAMES: &[u8] = b"\xff\xf8\x69\x18audio-frames-go-here-and-here";

    fn flac(blocks: &[Vec<u8>]) -> Vec<u8> {
        let mut out = b"fLaC".to_vec();
        for b in blocks {
            out.extend_from_slice(b);
        }
        out.extend_from_slice(FRAMES);
        out
    }

    #[test]
    fn payload_is_the_frames_after_the_last_metadata_block() {
        let bytes = flac(&[block(0, true, STREAMINFO)]);
        let mut t = src_of(&bytes);
        let plan = plan(&mut t.src).unwrap().unwrap();
        assert_eq!(plan.scheme, SCHEME);
        assert_eq!(
            super::super::hash_plan(&mut t.src, &plan).unwrap(),
            b3(FRAMES)
        );
    }

    #[test]
    fn tags_seektable_and_cover_art_are_all_excluded() {
        let bare = flac(&[block(0, true, STREAMINFO)]);
        let decorated = flac(&[
            block(0, false, STREAMINFO),
            block(3, false, &[0u8; 90]), // SEEKTABLE
            block(4, false, b"\x20\x00\x00\x00reference libFLAC"), // VORBIS_COMMENT
            block(6, false, &[0xcc; 4096]), // PICTURE: cover art
            block(1, true, &[0u8; 512]), // PADDING
        ]);
        assert_ne!(bare, decorated);

        let mut a = src_of(&bare);
        let mut b = src_of(&decorated);
        let pa = plan(&mut a.src).unwrap().unwrap();
        let pb = plan(&mut b.src).unwrap().unwrap();
        assert_eq!(
            super::super::hash_plan(&mut a.src, &pa).unwrap(),
            super::super::hash_plan(&mut b.src, &pb).unwrap()
        );
    }

    #[test]
    fn an_id3v2_tag_ahead_of_the_magic_is_tolerated() {
        let mut prefixed = b"ID3\x04\x00\x00\x00\x00\x01\x00".to_vec();
        prefixed.resize(10 + 128, 0);
        prefixed.extend_from_slice(&flac(&[block(0, true, STREAMINFO)]));
        let mut t = src_of(&prefixed);
        let plan = plan(&mut t.src).unwrap().unwrap();
        assert_eq!(
            super::super::hash_plan(&mut t.src, &plan).unwrap(),
            b3(FRAMES)
        );
    }

    #[test]
    fn an_appended_id3v1_is_peeled() {
        let mut bytes = flac(&[block(0, true, STREAMINFO)]);
        let mut v1 = b"TAG".to_vec();
        v1.resize(128, 0x20);
        bytes.extend_from_slice(&v1);
        let mut t = src_of(&bytes);
        let plan = plan(&mut t.src).unwrap().unwrap();
        assert_eq!(
            super::super::hash_plan(&mut t.src, &plan).unwrap(),
            b3(FRAMES)
        );
    }

    #[test]
    fn changing_a_frame_byte_moves_the_hash() {
        let a_bytes = flac(&[block(0, true, STREAMINFO)]);
        let mut b_bytes = a_bytes.clone();
        let n = b_bytes.len();
        b_bytes[n - 3] ^= 0xff;
        let mut a = src_of(&a_bytes);
        let mut b = src_of(&b_bytes);
        let pa = plan(&mut a.src).unwrap().unwrap();
        let pb = plan(&mut b.src).unwrap().unwrap();
        assert_ne!(
            super::super::hash_plan(&mut a.src, &pa).unwrap(),
            super::super::hash_plan(&mut b.src, &pb).unwrap()
        );
    }

    #[test]
    fn a_metadata_block_running_past_eof_is_an_error_not_a_guess() {
        let mut bytes = b"fLaC".to_vec();
        bytes.extend_from_slice(&[0x80, 0xff, 0xff, 0xff]); // last block, 16 MiB
        let mut t = src_of(&bytes);
        assert!(plan(&mut t.src).is_err());
    }

    #[test]
    fn a_file_with_no_frames_plans_nothing() {
        let mut bytes = b"fLaC".to_vec();
        bytes.extend_from_slice(&block(0, true, STREAMINFO));
        let mut t = src_of(&bytes);
        assert!(plan(&mut t.src).unwrap().is_none());
    }

    #[test]
    fn non_flac_bytes_are_an_error() {
        let mut t = src_of(b"RIFF\x00\x00\x00\x00WAVEfmt ");
        assert!(plan(&mut t.src).is_err());
    }
}
