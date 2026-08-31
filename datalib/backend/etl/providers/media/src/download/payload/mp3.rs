//! MP3 payload: the MPEG audio frames, with every tag and the VBR
//! header frame left out.
//!
//! This is the case the payload hash was worth building for. Music
//! libraries retag constantly — a rating, a play count, embedded
//! cover art swapped for a bigger one, iTunes normalizing a genre —
//! and every one of those rewrites the ID3v2 block at the front of the
//! file. The frames after it are untouched. So the file hash churns
//! while `payload_blake3` sits still, which is exactly the difference
//! between "how many files do I have" and "how many songs do I have".
//!
//! Three things are stripped:
//!
//! - **ID3v2 at the front.** Its size field is *syncsafe* (seven bits
//!   per byte), which is the one detail everyone gets wrong: reading it
//!   as a plain big-endian integer overshoots on any tag past 128 bytes
//!   and lands in the middle of the audio.
//! - **ID3v1 / APEv2 at the back**, in whatever order they were
//!   appended. Both are fixed-shape trailers, so this is a loop that
//!   peels whichever one is currently last.
//! - **The Xing / Info / VBRI header frame.** This is the subtle one.
//!   It is a *real, structurally valid MPEG frame* that decodes to
//!   silence and carries the VBR seek table, the encoder delay/padding
//!   for gapless playback, and the LAME extension's ReplayGain fields.
//!
//!   The seek table is the reason it churns: it is expressed in byte
//!   offsets and a total file size, so **any tag edit that changes the
//!   file's length invalidates it**, and any tool that notices rewrites
//!   it. Add cover art and the audio has not moved but this frame has.
//!   Gapless-analysis passes and `vbrfix`-style repairs rewrite it
//!   directly. Leaving it in would put a frequently-rewritten metadata
//!   block inside the "payload" and defeat the column for a large part
//!   of a real library.
//!
//!   Note what this does *not* cover: `mp3gain` applying gain is not a
//!   header edit. It rewrites the `global_gain` field in every frame's
//!   side information — that is the whole trick, a volume change with
//!   no re-encode — so it moves [`SCHEME`] as surely as a re-encode
//!   would. Only its undo/ReplayGain bookkeeping lands in tags and this
//!   frame.

use anyhow::Result;

use super::{be_u32, Plan, Src};

/// MPEG frames, tags and the VBR header frame excluded.
pub const SCHEME: &str = "mp3.frames.v1";

/// Byte offset of the APEv2 footer's flags field.
///
/// The footer is `preamble[8] version[4] tag_size[4] item_count[4]
/// flags[4] reserved[8]`. Reading flags at 16 instead lands on the item
/// count — which is how you end up stripping a phantom 32-byte header
/// off any tag whose item count happens to have its top bit set.
pub(crate) const APE_FLAGS_AT: usize = 20;

/// How far into the audio region to look for the first frame sync.
/// Encoders and broken splitters leave junk here; past this much of it
/// we would rather report nothing than hash from a false sync.
const SYNC_SEARCH: u64 = 64 * 1024;

pub fn plan(src: &mut Src) -> Result<Option<Plan>> {
    let start = audio_start(src)?;
    let end = audio_end(src, start)?;
    if end <= start {
        return Ok(None);
    }

    // Find the first real frame, then decide whether it is a VBR
    // header frame we should skip past.
    let Some((frame_at, header)) = first_frame(src, start, end)? else {
        return Ok(None);
    };
    let mut payload_at = frame_at;
    if is_vbr_header_frame(src, frame_at, &header)? {
        payload_at = frame_at + header.frame_len;
    }
    if payload_at >= end {
        return Ok(None);
    }
    Ok(Plan::flat(SCHEME, vec![(payload_at, end - payload_at)]).non_empty())
}

/// First byte after any ID3v2 tag.
///
/// Also used by [`super::flac`], where an ID3v2 tag ahead of the `fLaC`
/// magic is out of spec but common enough to handle.
pub fn audio_start(src: &mut Src) -> Result<u64> {
    let head = src.read_upto(0, 10)?;
    if head.len() < 10 || !head.starts_with(b"ID3") {
        return Ok(0);
    }
    let Some(size) = syncsafe_u32(&head[6..10]) else {
        return Ok(0);
    };
    let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
    Ok((10 + u64::from(size) + footer).min(src.len()))
}

/// First byte of the trailing tags, i.e. one past the last audio byte.
///
/// Peels repeatedly because both trailers can be present: a file
/// tagged by one tool and then another ends up with APEv2 sitting in
/// front of an ID3v1 that was already there.
fn audio_end(src: &mut Src, start: u64) -> Result<u64> {
    let mut end = src.len();
    loop {
        let mut peeled = false;

        // APEv2 footer: 32 bytes, the last of which end the tag.
        if end >= start + 32 {
            let foot = src.read_at(end - 32, 32)?;
            if foot.starts_with(b"APETAGEX") {
                let size = u64::from(super::le_u32(&foot, 12).unwrap_or(0));
                let flags = super::le_u32(&foot, APE_FLAGS_AT).unwrap_or(0);
                // Bit 31 says a 32-byte header precedes the tag body;
                // `size` counts the body plus this footer either way.
                // See `APE_FLAGS_AT` for why 20 and not 16.
                let header = if flags & 0x8000_0000 != 0 { 32 } else { 0 };
                let total = size.saturating_add(header);
                if total > 0 && end.saturating_sub(total) >= start {
                    end -= total;
                    peeled = true;
                }
            }
        }

        // ID3v1: a flat 128-byte record.
        if end >= start + 128 {
            let tag = src.read_at(end - 128, 3)?;
            if tag.as_slice() == b"TAG" {
                end -= 128;
                peeled = true;
            }
        }

        if !peeled {
            return Ok(end);
        }
    }
}

/// ID3v2 sizes store seven bits per byte so the encoding can never
/// contain a byte that looks like a frame sync.
fn syncsafe_u32(b: &[u8]) -> Option<u32> {
    let raw = be_u32(b, 0)?;
    if b.iter().any(|&x| x & 0x80 != 0) {
        // Not actually syncsafe. Some writers get this wrong; treating
        // the value as plain big-endian is the lesser evil, since the
        // alternative is refusing a file that plays fine everywhere.
        return Some(raw);
    }
    Some(
        (raw & 0x7f)
            | ((raw >> 1) & 0x0000_3f80)
            | ((raw >> 2) & 0x001f_c000)
            | ((raw >> 3) & 0x0fe0_0000),
    )
}

/// A decoded MPEG audio frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// 1, 2, or 25 for MPEG-2.5. (25 rather than 2.5 so this stays an
    /// integer; only the sample-rate table cares which.)
    pub version: u8,
    pub layer: u8,
    pub bitrate_kbps: u32,
    pub sample_rate_hz: u32,
    pub mono: bool,
    pub frame_len: u64,
}

const BITRATE_V1: [[u32; 15]; 3] = [
    // Layer I
    [
        32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0,
    ],
    // Layer II
    [
        32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
    ],
    // Layer III
    [
        32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
    ],
];

const BITRATE_V2: [[u32; 15]; 3] = [
    // Layer I
    [
        32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0,
    ],
    // Layers II and III share a table in MPEG-2 / 2.5.
    [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0],
    [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0],
];

const SAMPLE_RATE: [[u32; 3]; 3] = [
    [44100, 48000, 32000], // MPEG-1
    [22050, 24000, 16000], // MPEG-2
    [11025, 12000, 8000],  // MPEG-2.5
];

/// Decode a 4-byte frame header, or `None` if these bytes are not one.
///
/// Every reserved encoding is rejected rather than guessed at, because
/// the caller uses "did this decode?" as its sync test — a lenient
/// decoder here would happily lock onto a byte pair inside the audio.
pub fn decode_header(b: &[u8]) -> Option<FrameHeader> {
    if b.len() < 4 || b[0] != 0xff || (b[1] & 0xe0) != 0xe0 {
        return None;
    }
    let (version, ver_row) = match (b[1] >> 3) & 0x03 {
        0b11 => (1u8, 0usize),
        0b10 => (2u8, 1usize),
        0b00 => (25u8, 2usize),
        _ => return None, // reserved
    };
    let layer = match (b[1] >> 1) & 0x03 {
        0b11 => 1u8,
        0b10 => 2u8,
        0b01 => 3u8,
        _ => return None, // reserved
    };
    let bitrate_idx = (b[2] >> 4) as usize;
    if bitrate_idx == 0 || bitrate_idx == 15 {
        // 0 is "free format" (length not derivable from the header) and
        // 15 is invalid. Neither lets us walk to the next frame.
        return None;
    }
    let table = if version == 1 { BITRATE_V1 } else { BITRATE_V2 };
    let bitrate_kbps = table[layer as usize - 1][bitrate_idx - 1];
    if bitrate_kbps == 0 {
        return None;
    }
    let rate_idx = ((b[2] >> 2) & 0x03) as usize;
    if rate_idx == 3 {
        return None; // reserved
    }
    let sample_rate_hz = SAMPLE_RATE[ver_row][rate_idx];
    let padding = u64::from((b[2] >> 1) & 0x01);
    let mono = (b[3] >> 6) & 0x03 == 0b11;

    let bps = u64::from(bitrate_kbps) * 1000;
    let sr = u64::from(sample_rate_hz);
    let frame_len = match (layer, version) {
        (1, _) => (12 * bps / sr + padding) * 4,
        (_, 1) => 144 * bps / sr + padding,
        // MPEG-2 and 2.5 Layer III carry half as many samples per
        // frame, so the coefficient halves with them.
        (3, _) => 72 * bps / sr + padding,
        (_, _) => 144 * bps / sr + padding,
    };
    if frame_len < 4 {
        return None;
    }
    Some(FrameHeader {
        version,
        layer,
        bitrate_kbps,
        sample_rate_hz,
        mono,
        frame_len,
    })
}

/// Locate the first frame at or after `start`.
///
/// A single valid header is not enough to call it a sync — random audio
/// contains plenty of byte pairs that decode. We require that the frame
/// this header describes is followed by *another* valid header, which
/// is what makes a false lock vanishingly unlikely.
fn first_frame(src: &mut Src, start: u64, end: u64) -> Result<Option<(u64, FrameHeader)>> {
    let window = (end - start).min(SYNC_SEARCH);
    let buf = src.read_upto(start, window)?;
    let mut i = 0usize;
    while i + 4 <= buf.len() {
        if let Some(h) = decode_header(&buf[i..]) {
            let at = start + i as u64;
            let next = at + h.frame_len;
            let confirmed = if next + 4 <= end {
                decode_header(&src.read_at(next, 4)?).is_some()
            } else {
                // The last frame in the file has nothing after it to
                // confirm against; accept it rather than losing a
                // one-frame file.
                next <= end
            };
            if confirmed {
                return Ok(Some((at, h)));
            }
        }
        i += 1;
    }
    Ok(None)
}

/// Byte offset of the Xing/Info tag inside a frame, per the spec: past
/// the header and the layer-III side information, whose size depends on
/// version and channel mode.
fn xing_offset(h: &FrameHeader) -> u64 {
    match (h.version, h.mono) {
        (1, true) => 4 + 17,
        (1, false) => 4 + 32,
        (_, true) => 4 + 9,
        (_, false) => 4 + 17,
    }
}

/// Is the frame at `at` a VBR header frame rather than audio?
///
/// Checked at the two spec-defined offsets rather than by scanning the
/// frame for the magic. A scan would false-positive on audio that
/// happens to contain the bytes `Info`, and the cost of that is
/// silently dropping the first real frame of the song from the hash.
fn is_vbr_header_frame(src: &mut Src, at: u64, h: &FrameHeader) -> Result<bool> {
    let want = xing_offset(h);
    if want + 4 <= h.frame_len {
        let magic = src.read_upto(at + want, 4)?;
        if magic == b"Xing" || magic == b"Info" {
            return Ok(true);
        }
    }
    // Fraunhofer's VBRI sits at a fixed 32 bytes past the header
    // instead, and only ever in MPEG-1 files.
    if 36 + 4 <= h.frame_len {
        let magic = src.read_upto(at + 36, 4)?;
        if magic == b"VBRI" {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{b3, src_of};
    use super::*;

    /// A 4-byte MPEG-1 Layer III header: 128 kbps, 44.1 kHz, joint
    /// stereo, no padding. 417 bytes per frame.
    const H: [u8; 4] = [0xff, 0xfb, 0x90, 0x44];
    const FRAME_LEN: usize = 417;

    fn frame(fill: u8) -> Vec<u8> {
        let mut f = H.to_vec();
        f.resize(FRAME_LEN, fill);
        f
    }

    /// A frame carrying the Xing magic at the spec offset for MPEG-1
    /// stereo (36 bytes in).
    fn xing_frame() -> Vec<u8> {
        let mut f = frame(0x00);
        f[36..40].copy_from_slice(b"Xing");
        f
    }

    fn id3v2(payload_len: usize) -> Vec<u8> {
        let mut t = b"ID3\x04\x00\x00".to_vec();
        // Syncsafe: seven bits per byte.
        let n = payload_len as u32;
        t.push(((n >> 21) & 0x7f) as u8);
        t.push(((n >> 14) & 0x7f) as u8);
        t.push(((n >> 7) & 0x7f) as u8);
        t.push((n & 0x7f) as u8);
        t.resize(10 + payload_len, 0x00);
        t
    }

    #[test]
    fn header_decoding_matches_the_spec_tables() {
        let h = decode_header(&H).unwrap();
        assert_eq!(h.version, 1);
        assert_eq!(h.layer, 3);
        assert_eq!(h.bitrate_kbps, 128);
        assert_eq!(h.sample_rate_hz, 44100);
        assert_eq!(h.frame_len, FRAME_LEN as u64);
    }

    #[test]
    fn padding_bit_adds_exactly_one_byte() {
        let mut padded = H;
        padded[2] |= 0x02;
        assert_eq!(
            decode_header(&padded).unwrap().frame_len,
            FRAME_LEN as u64 + 1
        );
    }

    #[test]
    fn mpeg2_layer3_uses_the_halved_coefficient() {
        // MPEG-2, Layer III, 64 kbps, 22.05 kHz => 72*64000/22050
        // truncates to 208. The spec's floor is load-bearing: rounding
        // up here would desynchronize the frame walk.
        let h = decode_header(&[0xff, 0xf3, 0x80, 0x44]).unwrap();
        assert_eq!(h.version, 2);
        assert_eq!(h.layer, 3);
        assert_eq!(h.bitrate_kbps, 64);
        assert_eq!(h.sample_rate_hz, 22050);
        assert_eq!(h.frame_len, 208);
    }

    #[test]
    fn reserved_and_free_format_encodings_are_rejected() {
        assert!(decode_header(&[0xff, 0xfb, 0x00, 0x44]).is_none(), "free");
        assert!(
            decode_header(&[0xff, 0xfb, 0xf0, 0x44]).is_none(),
            "bad bitrate"
        );
        assert!(
            decode_header(&[0xff, 0xfb, 0x9c, 0x44]).is_none(),
            "bad rate"
        );
        assert!(
            decode_header(&[0xff, 0xf9, 0x90, 0x44]).is_none(),
            "layer 0"
        );
        assert!(decode_header(&[0xff, 0xeb, 0x90, 0x44]).is_none(), "ver 1");
        assert!(
            decode_header(&[0x00, 0x00, 0x00, 0x00]).is_none(),
            "no sync"
        );
    }

    #[test]
    fn syncsafe_sizes_are_decoded_seven_bits_per_byte() {
        // 0x00 0x00 0x02 0x01 => (2<<7)|1 = 257, not 0x0201 = 513.
        assert_eq!(syncsafe_u32(&[0x00, 0x00, 0x02, 0x01]), Some(257));
        assert_eq!(syncsafe_u32(&[0x00, 0x00, 0x00, 0x7f]), Some(127));
        assert_eq!(syncsafe_u32(&[0x00, 0x00, 0x01, 0x00]), Some(128));
    }

    #[test]
    fn payload_is_the_frames_only() {
        let audio = [frame(0xa1), frame(0xa2)].concat();
        let mut t = src_of(&audio);
        let plan = plan(&mut t.src).unwrap().unwrap();
        assert_eq!(plan.scheme, SCHEME);
        assert_eq!(
            super::super::hash_plan(&mut t.src, &plan).unwrap(),
            b3(&audio)
        );
    }

    /// The point of the whole module: retagging must not move it.
    #[test]
    fn tags_front_and_back_do_not_move_the_payload_hash() {
        let audio = [frame(0xa1), frame(0xa2)].concat();

        let bare = audio.clone();
        let mut tagged = id3v2(600); // a cover-art-sized tag
        tagged.extend_from_slice(&audio);
        let mut v1 = b"TAG".to_vec();
        v1.resize(128, 0x20);
        tagged.extend_from_slice(&v1);

        assert_ne!(bare, tagged);
        let mut a = src_of(&bare);
        let mut b = src_of(&tagged);
        let pa = plan(&mut a.src).unwrap().unwrap();
        let pb = plan(&mut b.src).unwrap().unwrap();
        assert_eq!(
            super::super::hash_plan(&mut a.src, &pa).unwrap(),
            super::super::hash_plan(&mut b.src, &pb).unwrap()
        );
    }

    #[test]
    fn an_apev2_trailer_is_peeled_too_and_stacks_with_id3v1() {
        let audio = [frame(0xa1), frame(0xa2)].concat();
        let mut tagged = audio.clone();
        // APEv2 body + 32-byte footer, no header flag.
        let body = vec![0x5au8; 40];
        tagged.extend_from_slice(&body);
        let mut foot = b"APETAGEX".to_vec();
        foot.extend_from_slice(&2000u32.to_le_bytes()); // version
        foot.extend_from_slice(&((body.len() + 32) as u32).to_le_bytes()); // tag size
                                                                           // The item count sits between size and flags. Omitting it is
                                                                           // what makes a hand-built footer 28 bytes and silently shifts
                                                                           // `flags` onto the reserved block.
        foot.extend_from_slice(&1u32.to_le_bytes()); // item count
        foot.extend_from_slice(&0u32.to_le_bytes()); // flags: no header
        foot.extend_from_slice(&[0u8; 8]); // reserved
        assert_eq!(foot.len(), 32);
        tagged.extend_from_slice(&foot);
        // …and an ID3v1 appended after that, as a second tool would.
        let mut v1 = b"TAG".to_vec();
        v1.resize(128, 0x20);
        tagged.extend_from_slice(&v1);

        let mut a = src_of(&audio);
        let mut b = src_of(&tagged);
        let pa = plan(&mut a.src).unwrap().unwrap();
        let pb = plan(&mut b.src).unwrap().unwrap();
        assert_eq!(
            super::super::hash_plan(&mut a.src, &pa).unwrap(),
            super::super::hash_plan(&mut b.src, &pb).unwrap()
        );
    }

    /// A rewritten VBR header frame is the case a naive "skip the tags"
    /// implementation gets wrong.
    #[test]
    fn the_xing_frame_is_excluded_so_rewriting_it_is_invisible() {
        let audio = [frame(0xa1), frame(0xa2)].concat();

        let mut with_xing = xing_frame();
        with_xing.extend_from_slice(&audio);

        // A gapless-analysis pass rewriting the LAME extension, or a
        // tagger refreshing the seek table after the tag size moved.
        let mut rewritten = xing_frame();
        rewritten[120] = 0x7f;
        rewritten.extend_from_slice(&audio);

        assert_ne!(with_xing, rewritten);
        let mut a = src_of(&with_xing);
        let mut b = src_of(&rewritten);
        let pa = plan(&mut a.src).unwrap().unwrap();
        let pb = plan(&mut b.src).unwrap().unwrap();
        assert_eq!(
            super::super::hash_plan(&mut a.src, &pa).unwrap(),
            super::super::hash_plan(&mut b.src, &pb).unwrap()
        );
        // …and it equals the same file with no Xing frame at all.
        let mut c = src_of(&audio);
        let pc = plan(&mut c.src).unwrap().unwrap();
        assert_eq!(
            super::super::hash_plan(&mut a.src, &pa).unwrap(),
            super::super::hash_plan(&mut c.src, &pc).unwrap()
        );
    }

    #[test]
    fn a_frame_whose_audio_contains_the_word_info_is_not_mistaken_for_a_header() {
        // `Info` well away from the spec offset (36) must not count.
        let mut f = frame(0x33);
        f[200..204].copy_from_slice(b"Info");
        let audio = [f.clone(), frame(0xa2)].concat();
        let mut t = src_of(&audio);
        let plan = plan(&mut t.src).unwrap().unwrap();
        assert_eq!(
            plan.total_bytes(),
            audio.len() as u64,
            "no frame should have been skipped"
        );
    }

    #[test]
    fn changing_one_audio_byte_moves_the_hash() {
        let a_bytes = [frame(0xa1), frame(0xa2)].concat();
        let mut b_bytes = a_bytes.clone();
        b_bytes[500] ^= 0xff;
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
    fn junk_before_the_first_sync_is_skipped() {
        let audio = [frame(0xa1), frame(0xa2)].concat();
        let mut with_junk = vec![0x00u8; 91];
        with_junk.extend_from_slice(&audio);
        let mut t = src_of(&with_junk);
        let plan = plan(&mut t.src).unwrap().unwrap();
        assert_eq!(
            super::super::hash_plan(&mut t.src, &plan).unwrap(),
            b3(&audio)
        );
    }

    #[test]
    fn a_file_with_no_frames_plans_nothing() {
        let mut t = src_of(&vec![0x00u8; 4096]);
        assert!(plan(&mut t.src).unwrap().is_none());
        let mut t = src_of(&id3v2(100));
        assert!(plan(&mut t.src).unwrap().is_none());
    }
}
