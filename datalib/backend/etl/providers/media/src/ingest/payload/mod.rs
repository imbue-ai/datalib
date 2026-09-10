//! The metadata-excluding payload hash: `media_items.payload_blake3`.

pub mod bmff;
pub mod flac;
pub mod jpeg;
pub mod mp3;
pub mod png;
pub mod riff;
pub mod tiff;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result};

use super::kind::Container;

/// A half-open byte range of the file, `[start, start + len)`.
pub type Range = (u64, u64);

/// What to hash, and under what name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Recipe name and version, e.g. `mp3.frames.v1`. Stored beside the
    /// digest so two rows are only ever compared under one recipe.
    pub scheme: &'static str,
    /// Ordered groups of ordered ranges. Most containers produce one
    /// group; the multi-stream ones (BMFF tracks, TIFF image IFDs)
    /// produce one per stream so that changing a video track does not
    /// disturb the audio track's identity.
    pub groups: Vec<Vec<Range>>,
}

impl Plan {
    pub fn flat(scheme: &'static str, ranges: Vec<Range>) -> Self {
        Self {
            scheme,
            groups: vec![ranges],
        }
    }

    pub fn total_bytes(&self) -> u64 {
        self.groups
            .iter()
            .flatten()
            .map(|(_, len)| *len)
            .sum::<u64>()
    }

    /// A plan that would hash nothing is not a plan. An empty result
    /// means the parse found no payload — a zero-length `data` chunk, a
    /// BMFF with no tracks and no items — and the honest record of that
    /// is NULL, per rule 1 in the module docs.
    pub fn non_empty(self) -> Option<Self> {
        if self.total_bytes() == 0 {
            None
        } else {
            Some(self)
        }
    }
}

/// The computed payload hash and the recipe that produced it.
#[derive(Debug, Clone)]
pub struct Payload {
    pub blake3: String,
    pub scheme: &'static str,
}

/// Bytes read at a time when streaming a range through the hasher.
const HASH_CHUNK: usize = 256 * 1024;

/// Structural reads (headers, box tables, IFD entries) are capped at
/// this to keep a corrupt length field from asking for a gigabyte.
const MAX_STRUCT_READ: u64 = 8 * 1024 * 1024;

/// A seekable byte source with the range-reading helpers every parser
/// here needs.
pub struct Src {
    file: File,
    len: u64,
}

impl Src {
    pub fn open(path: &Path) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("open for payload {}", path.display()))?;
        let len = file
            .metadata()
            .with_context(|| format!("stat for payload {}", path.display()))?
            .len();
        Ok(Self { file, len })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Read exactly `len` bytes at `at`. Short reads and out-of-range
    /// offsets are errors, not truncations — a parser that silently
    /// accepted a short read would emit a plan covering bytes that are
    /// not there.
    pub fn read_at(&mut self, at: u64, len: u64) -> Result<Vec<u8>> {
        anyhow::ensure!(
            len <= MAX_STRUCT_READ,
            "structural read of {len} bytes exceeds the {MAX_STRUCT_READ}-byte cap"
        );
        anyhow::ensure!(
            at.checked_add(len).is_some_and(|end| end <= self.len),
            "read {len} at {at} runs past end of file ({})",
            self.len
        );
        let mut buf = vec![0u8; len as usize];
        self.file.seek(SeekFrom::Start(at))?;
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// Read up to `len` bytes at `at`, stopping at end of file. For
    /// probing (magic bytes, a trailing tag footer) where running short
    /// is a normal answer rather than an error.
    pub fn read_upto(&mut self, at: u64, len: u64) -> Result<Vec<u8>> {
        if at >= self.len {
            return Ok(Vec::new());
        }
        let avail = (self.len - at).min(len);
        self.read_at(at, avail)
    }
}

pub fn compute(path: &Path, container: Container) -> Result<Option<Payload>> {
    let mut src = Src::open(path)?;
    if src.is_empty() {
        return Ok(None);
    }
    let plan = match plan_for(&mut src, container) {
        Ok(p) => p,
        Err(e) => {
            // Structure we could not follow. Worth seeing — a whole
            // corpus coming back NULL means a recipe is wrong — but not
            // worth failing the file's row over.
            tracing::debug!(
                path = %path.display(),
                container = container.as_str(),
                error = %e,
                "media_payload_plan_failed"
            );
            return Ok(None);
        }
    };
    let Some(plan) = plan else { return Ok(None) };
    let blake3 = hash_plan(&mut src, &plan)
        .with_context(|| format!("hash payload of {}", path.display()))?;
    Ok(Some(Payload {
        blake3,
        scheme: plan.scheme,
    }))
}

fn plan_for(src: &mut Src, container: Container) -> Result<Option<Plan>> {
    Ok(match container {
        Container::Wav => riff::plan_wav(src)?,
        Container::Avi => riff::plan_avi(src)?,
        Container::Mp3 => mp3::plan(src)?,
        Container::Flac => flac::plan(src)?,
        Container::Jpeg => jpeg::plan(src)?,
        Container::Png => png::plan(src)?,
        Container::Tiff => tiff::plan(src)?,
        Container::Bmff => bmff::plan(src)?,
        // Recognized, recorded, no recipe yet. Adding one is a pure
        // addition: the work list is
        // `SELECT … WHERE payload_blake3 IS NULL`.
        Container::Gif
        | Container::Webp
        | Container::Aiff
        | Container::Ogg
        | Container::Matroska
        | Container::Unknown => None,
    })
}

pub fn hash_plan(src: &mut Src, plan: &Plan) -> Result<String> {
    if plan.groups.len() == 1 {
        let mut h = blake3::Hasher::new();
        feed(src, &plan.groups[0], &mut h)?;
        return Ok(hex(h.finalize().as_bytes()));
    }
    let mut outer = blake3::Hasher::new();
    for group in &plan.groups {
        let mut inner = blake3::Hasher::new();
        feed(src, group, &mut inner)?;
        outer.update(inner.finalize().as_bytes());
    }
    Ok(hex(outer.finalize().as_bytes()))
}

fn feed(src: &mut Src, ranges: &[Range], h: &mut blake3::Hasher) -> Result<()> {
    let mut buf = vec![0u8; HASH_CHUNK];
    for &(start, len) in ranges {
        anyhow::ensure!(
            start.checked_add(len).is_some_and(|e| e <= src.len),
            "payload range {start}+{len} runs past end of file ({})",
            src.len
        );
        src.file.seek(SeekFrom::Start(start))?;
        let mut left = len;
        while left > 0 {
            let want = left.min(HASH_CHUNK as u64) as usize;
            src.file.read_exact(&mut buf[..want])?;
            h.update(&buf[..want]);
            left -= want as u64;
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// Shared integer decoding. Every container here is built out of these.

pub(crate) fn be_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

pub(crate) fn be_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

pub(crate) fn be_u64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

pub(crate) fn le_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

pub(crate) fn le_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::Src;

    /// A [`Src`] over an in-memory blob, via a temp file. The parsers
    /// are written against a seekable file rather than a slice, so
    /// their tests need one too.
    pub struct TempSrc {
        pub src: Src,
        _dir: tempfile::TempDir,
    }

    pub fn src_of(bytes: &[u8]) -> TempSrc {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.bin");
        std::fs::write(&p, bytes).unwrap();
        TempSrc {
            src: Src::open(&p).unwrap(),
            _dir: dir,
        }
    }

    pub fn b3(bytes: &[u8]) -> String {
        super::hex(blake3::hash(bytes).as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{b3, src_of};
    use super::*;

    #[test]
    fn one_group_hashes_its_bytes_directly() {
        let mut t = src_of(b"0123456789");
        let plan = Plan::flat("t.v1", vec![(2, 4)]);
        assert_eq!(hash_plan(&mut t.src, &plan).unwrap(), b3(b"2345"));
    }

    #[test]
    fn multiple_ranges_in_a_group_concatenate_in_order() {
        let mut t = src_of(b"0123456789");
        let plan = Plan::flat("t.v1", vec![(0, 2), (8, 2)]);
        assert_eq!(hash_plan(&mut t.src, &plan).unwrap(), b3(b"0189"));
    }

    #[test]
    fn groups_are_digested_separately_then_combined() {
        let mut t = src_of(b"0123456789");
        let plan = Plan {
            scheme: "t.v1",
            groups: vec![vec![(0, 2)], vec![(8, 2)]],
        };
        let mut outer = blake3::Hasher::new();
        outer.update(blake3::hash(b"01").as_bytes());
        outer.update(blake3::hash(b"89").as_bytes());
        assert_eq!(
            hash_plan(&mut t.src, &plan).unwrap(),
            hex(outer.finalize().as_bytes())
        );
        // …and that is NOT the same as hashing the concatenation, which
        // is the whole point: a change confined to one group leaves the
        // other group's digest untouched.
        assert_ne!(hash_plan(&mut t.src, &plan).unwrap(), b3(b"0189"));
    }

    #[test]
    fn a_group_swap_changes_the_digest() {
        let mut t = src_of(b"0123456789");
        let a = Plan {
            scheme: "t.v1",
            groups: vec![vec![(0, 2)], vec![(8, 2)]],
        };
        let b = Plan {
            scheme: "t.v1",
            groups: vec![vec![(8, 2)], vec![(0, 2)]],
        };
        assert_ne!(
            hash_plan(&mut t.src, &a).unwrap(),
            hash_plan(&mut t.src, &b).unwrap()
        );
    }

    #[test]
    fn ranges_past_end_of_file_are_refused_not_truncated() {
        let mut t = src_of(b"0123");
        let plan = Plan::flat("t.v1", vec![(2, 99)]);
        assert!(hash_plan(&mut t.src, &plan).is_err());
    }

    #[test]
    fn an_empty_plan_is_not_a_plan() {
        assert!(Plan::flat("t.v1", vec![]).non_empty().is_none());
        assert!(Plan::flat("t.v1", vec![(0, 0)]).non_empty().is_none());
        assert!(Plan::flat("t.v1", vec![(0, 1)]).non_empty().is_some());
    }

    #[test]
    fn read_at_refuses_short_reads() {
        let mut t = src_of(b"0123");
        assert!(t.src.read_at(0, 4).is_ok());
        assert!(t.src.read_at(0, 5).is_err());
        assert!(t.src.read_at(4, 1).is_err());
        // …but probing reads are allowed to come up short.
        assert_eq!(t.src.read_upto(2, 99).unwrap(), b"23");
        assert!(t.src.read_upto(9, 4).unwrap().is_empty());
    }

    #[test]
    fn unparsed_containers_plan_nothing_rather_than_falling_back() {
        let mut t = src_of(b"GIF89a and then some bytes");
        // The fallback we are explicitly NOT doing is "hash the whole
        // file", so the absence here is the assertion.
        assert!(plan_for(&mut t.src, Container::Gif).unwrap().is_none());
        assert!(plan_for(&mut t.src, Container::Unknown).unwrap().is_none());
        assert!(plan_for(&mut t.src, Container::Matroska).unwrap().is_none());
    }
}
