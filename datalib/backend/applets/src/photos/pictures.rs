//! Where a photo's pictures are, decided from values: which of the
//! source's own previews answers a size, the file that holds it, the
//! bytes inside an older catalog's `.lrprev`, and what a file's first
//! bytes say it is. No I/O; `mod.rs` reads what this names.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use strum::{EnumString, IntoStaticStr, VariantArray};

/// The smallest long edge a thumbnail is drawn from.
pub const THUMB_LONG_EDGE: i64 = 320;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Deserialize, EnumString, IntoStaticStr, VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Size {
    /// The smallest preview at least [`THUMB_LONG_EDGE`] across.
    Thumb,
    /// The largest preview of the current render.
    Full,
}

impl Size {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// One size of a render, as `previews.db`'s `PyramidLevel` lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level {
    pub level: i64,
    pub long_edge: i64,
}

/// A catalog's previews: `<name> Previews.lrdata` beside `<name>.lrcat`.
pub fn previews_dir(catalog: &Path) -> Option<PathBuf> {
    let stem = catalog.file_stem()?.to_str()?;
    Some(catalog.with_file_name(format!("{stem} Previews.lrdata")))
}

/// Lightroom's file uuids are upper-case; its digests lower-case hex.
pub fn valid_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'A'..=b'F').contains(&b),
        })
}

pub fn valid_digest(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn pick(levels: &[Level], size: Size) -> Option<Level> {
    let largest = levels.iter().max_by_key(|l| l.long_edge).copied();
    match size {
        Size::Full => largest,
        Size::Thumb => levels
            .iter()
            .filter(|l| l.long_edge >= THUMB_LONG_EDGE)
            .min_by_key(|l| l.long_edge)
            .copied()
            .or(largest),
    }
}

/// `<a>/<abcd>/<uuid>-<digest>`, the stem both preview formats share.
/// `None` for a uuid or digest not shaped as Lightroom writes them, so
/// nothing from a database becomes a path unchecked.
fn stem(dir: &Path, uuid: &str, digest: &str) -> Option<PathBuf> {
    (valid_uuid(uuid) && valid_digest(digest)).then(|| {
        dir.join(&uuid[..1])
            .join(&uuid[..4])
            .join(format!("{uuid}-{digest}"))
    })
}

/// A newer catalog's previews: one JPEG per size, named for its long edge.
pub fn loose_file(dir: &Path, uuid: &str, digest: &str, long_edge: i64) -> Option<PathBuf> {
    let stem = stem(dir, uuid, digest)?;
    let name = format!("{}_{long_edge}", stem.file_name()?.to_str()?);
    Some(stem.with_file_name(name))
}

/// An older catalog's previews: every size in one `.lrprev`.
pub fn lrprev_file(dir: &Path, uuid: &str, digest: &str) -> Option<PathBuf> {
    let stem = stem(dir, uuid, digest)?;
    let name = format!("{}.lrprev", stem.file_name()?.to_str()?);
    Some(stem.with_file_name(name))
}

/// The JPEG of `level` inside an `.lrprev`: a run of sections, each a
/// 32-byte header — `AgHg`, the header's length (u16), a version and a
/// kind byte, the data's length and the padding's (u64 each, big-endian),
/// and a name such as `level_3` — then the data, then the padding.
pub fn lrprev_level(bytes: &[u8], level: i64) -> Option<&[u8]> {
    let want = format!("level_{level}");
    let mut at = 0usize;
    while at < bytes.len() {
        let head = bytes.get(at..at + 24)?;
        if &head[..4] != b"AgHg" {
            return None;
        }
        let head_len = u16::from_be_bytes([head[4], head[5]]) as usize;
        let data_len = usize::try_from(u64::from_be_bytes(head[8..16].try_into().ok()?)).ok()?;
        let pad_len = usize::try_from(u64::from_be_bytes(head[16..24].try_into().ok()?)).ok()?;
        let name = bytes.get(at + 24..at + head_len)?;
        let name = name.split(|b| *b == 0).next()?;
        let data_at = at.checked_add(head_len)?;
        let data = bytes.get(data_at..data_at.checked_add(data_len)?)?;
        if name == want.as_bytes() {
            return Some(data);
        }
        at = data_at.checked_add(data_len)?.checked_add(pad_len)?;
    }
    None
}

/// What a file's first bytes say it is, for the types a photo library
/// holds; `None` for anything else.
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if head.starts_with(b"GIF8") {
        return Some("image/gif");
    }
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        return Some(match &head[8..12] {
            b"qt  " => "video/quicktime",
            b"heic" | b"heix" | b"mif1" | b"msf1" => "image/heic",
            _ => "video/mp4",
        });
    }
    None
}

/// Whether a browser draws it. A RAW file, a TIFF or a HEIC (outside
/// Safari) it does not, so an original of one is refused with a reason.
pub fn drawable(content_type: &str) -> bool {
    matches!(
        content_type,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "video/mp4" | "video/quicktime"
    )
}

/// Where the original may be: its absolute path, then its path relative
/// to the catalog, for a library that moved with its catalog.
pub fn original_candidates(
    absolute: Option<&str>,
    relative_to_catalog: Option<&str>,
    catalog: Option<&Path>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = absolute.map(PathBuf::from).into_iter().collect();
    if let (Some(rel), Some(dir)) = (relative_to_catalog, catalog.and_then(Path::parent)) {
        out.push(dir.join(rel));
    }
    out
}

/// One byte range of a file, from a `Range: bytes=a-b` header; `None` for
/// no range or one this does not serve (several, or a suffix past the
/// start), which is answered with the whole file.
pub fn byte_range(header: Option<&str>, len: u64) -> Option<(u64, u64)> {
    let spec = header?.strip_prefix("bytes=")?;
    if spec.contains(',') || len == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            (len.saturating_sub(n), len - 1)
        }
        (a, "") => (a.parse().ok()?, len - 1),
        (a, b) => (a.parse().ok()?, b.parse::<u64>().ok()?.min(len - 1)),
    };
    (start <= end && start < len).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "90A53874-ACC1-45EE-A843-81BFFE893F0A";
    const DIGEST: &str = "3219a2e16918837c891ac13b27c31f04";

    #[test]
    fn strum_and_serde_agree() {
        for s in Size::VARIANTS {
            let via_serde: Size = serde_json::from_value(serde_json::json!(s.as_str())).unwrap();
            assert_eq!(via_serde, *s);
            assert_eq!(s.as_str().parse::<Size>().unwrap(), *s);
        }
    }

    #[test]
    fn previews_sit_beside_their_catalog() {
        assert_eq!(
            previews_dir(Path::new("/Pictures/Ship Log.lrcat")),
            Some(PathBuf::from("/Pictures/Ship Log Previews.lrdata"))
        );
    }

    /// The file names catalog 03 holds, and nothing a database row could
    /// smuggle a path through.
    #[test]
    fn a_preview_is_named_for_its_file_render_and_size() {
        let dir = Path::new("/p");
        assert_eq!(
            loose_file(dir, UUID, DIGEST, 1512),
            Some(PathBuf::from(format!("/p/9/90A5/{UUID}-{DIGEST}_1512")))
        );
        assert_eq!(
            lrprev_file(dir, UUID, DIGEST),
            Some(PathBuf::from(format!("/p/9/90A5/{UUID}-{DIGEST}.lrprev")))
        );
        assert_eq!(loose_file(dir, "../../etc/passwd", DIGEST, 1), None);
        assert_eq!(loose_file(dir, UUID, "../x", 1), None);
        assert_eq!(loose_file(dir, &UUID.to_lowercase(), DIGEST, 1), None);
    }

    #[test]
    fn a_thumbnail_is_the_smallest_size_big_enough() {
        let levels = [
            Level {
                level: 1,
                long_edge: 320,
            },
            Level {
                level: 2,
                long_edge: 756,
            },
            Level {
                level: 3,
                long_edge: 1512,
            },
        ];
        assert_eq!(pick(&levels, Size::Thumb).map(|l| l.level), Some(1));
        assert_eq!(pick(&levels, Size::Full).map(|l| l.level), Some(3));
        let small = [Level {
            level: 1,
            long_edge: 112,
        }];
        assert_eq!(pick(&small, Size::Thumb).map(|l| l.level), Some(1));
        assert_eq!(pick(&[], Size::Full), None);
    }

    fn section(name: &str, data: &[u8], pad: usize) -> Vec<u8> {
        let mut out = b"AgHg".to_vec();
        out.extend(32u16.to_be_bytes());
        out.extend([1, 0]);
        out.extend((data.len() as u64).to_be_bytes());
        out.extend((pad as u64).to_be_bytes());
        let mut field = name.as_bytes().to_vec();
        field.resize(8, 0);
        out.extend(field);
        out.extend(data);
        out.extend(vec![0; pad]);
        out
    }

    /// The layout of a real `.lrprev` from 2020: a `header` section of
    /// Lua text, then one JPEG section per level, each padded.
    #[test]
    fn reads_one_level_out_of_an_lrprev() {
        let mut file = section("header", b"pyramid = {}", 11);
        file.extend(section("level_1", b"\xFF\xD8\xFFsmall", 6));
        file.extend(section("level_2", b"\xFF\xD8\xFFlarger", 0));
        assert_eq!(lrprev_level(&file, 2), Some(&b"\xFF\xD8\xFFlarger"[..]));
        assert_eq!(lrprev_level(&file, 1), Some(&b"\xFF\xD8\xFFsmall"[..]));
        assert_eq!(lrprev_level(&file, 3), None);
        assert_eq!(lrprev_level(&file[..40], 2), None, "a cut file is no level");
        assert_eq!(lrprev_level(b"not a preview at all", 1), None);
    }

    #[test]
    fn the_type_comes_from_the_bytes() {
        assert_eq!(sniff(b"\xFF\xD8\xFF\xE0"), Some("image/jpeg"));
        assert_eq!(sniff(b"\0\0\0\x18ftypqt  \0\0"), Some("video/quicktime"));
        assert_eq!(sniff(b"\0\0\0\x18ftypheic\0\0"), Some("image/heic"));
        assert!(!drawable("image/heic"));
        assert_eq!(sniff(b"II*\0"), None, "a TIFF or a RAW is not drawn");
    }

    #[test]
    fn an_original_moved_with_its_catalog_is_found_beside_it() {
        assert_eq!(
            original_candidates(
                Some("/old/disk/a.jpg"),
                Some("../../photos/a.jpg"),
                Some(Path::new("/lib/cat/c.lrcat"))
            ),
            [
                PathBuf::from("/old/disk/a.jpg"),
                PathBuf::from("/lib/cat/../../photos/a.jpg")
            ]
        );
    }

    #[test]
    fn a_single_byte_range_is_served_and_others_are_not() {
        assert_eq!(byte_range(Some("bytes=0-99"), 1000), Some((0, 99)));
        assert_eq!(byte_range(Some("bytes=900-"), 1000), Some((900, 999)));
        assert_eq!(byte_range(Some("bytes=-100"), 1000), Some((900, 999)));
        assert_eq!(byte_range(Some("bytes=0-5000"), 1000), Some((0, 999)));
        assert_eq!(byte_range(Some("bytes=0-1,5-6"), 1000), None);
        assert_eq!(byte_range(Some("bytes=2000-"), 1000), None);
        assert_eq!(byte_range(None, 1000), None);
    }
}
