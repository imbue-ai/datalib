//! Content hashing for fsindex.
//!
//! Files: blake3 of the file bytes.
//! Symlinks: blake3 of the link-target bytes (so a retarget registers
//! as a content change).
//! Directories: blake3 of the canonical tree encoding spelled out in
//! [`super::schema_raw`] §"Directory tree-hash canonicalization."
//!
//! All hashes are the raw 32-byte blake3 digest (`[u8; 32]`), stored
//! in the db as a 32-byte BLOB. We do NOT carry the 64-char hex form:
//! hex doubles the per-row hash bytes both in the `files` table and in
//! its `blake3` index, which is a meaningful cost at the design scale.

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result};

use super::schema_raw::FileKind;

/// The raw 32-byte blake3 digest. Stored as a BLOB; rendered as hex
/// only for human-facing output (e.g. test snapshots, ad-hoc queries
/// via `hex(blake3)`).
pub type Blake3 = [u8; 32];

/// One immediate-child contribution to a directory's tree-hash.
pub struct TreeChild {
    pub name: Vec<u8>,
    pub kind: FileKind,
    pub blake3: Blake3,
}

/// Files larger than this use `Hasher::update_mmap`, smaller files
/// stream via `update_reader`. blake3 upstream guidance: mmap wins
/// for large files because it lets the kernel page-in lazily and
/// avoids one userspace copy; for tiny files the mmap setup
/// overhead dominates and streaming is faster. 16 MiB is the
/// threshold blake3's own b3sum CLI uses.
const MMAP_THRESHOLD: u64 = 16 * 1024 * 1024;

/// Hash file bytes. Streams via `update_reader` for files under
/// [`MMAP_THRESHOLD`], mmaps via `update_mmap` above it.
pub fn hash_file(path: &Path, size: u64) -> Result<Blake3> {
    let mut hasher = blake3::Hasher::new();
    if size >= MMAP_THRESHOLD {
        hasher
            .update_mmap(path)
            .with_context(|| format!("mmap-hash {}", path.display()))?;
    } else {
        let f = File::open(path).with_context(|| format!("open for hash {}", path.display()))?;
        hasher
            .update_reader(f)
            .with_context(|| format!("hash {}", path.display()))?;
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Hash a symlink's target bytes. Targets that point at moved data
/// register as content changes because the bytes hash differently.
pub fn hash_symlink_target(target: &[u8]) -> Blake3 {
    *blake3::hash(target).as_bytes()
}

/// Canonical directory tree-hash per the schema doc.
///
/// Each child contributes `name || 0x00 || kind_tag || child_blake3
/// (32 raw bytes) || 0x0a`, children sorted by lexical byte order of
/// `name`. The whole concatenation is hashed with blake3. Empty
/// children list hashes the empty string (well-defined).
pub fn hash_tree(children: &[TreeChild]) -> Blake3 {
    let mut sorted: Vec<&TreeChild> = children.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut buf: Vec<u8> = Vec::new();
    for c in sorted {
        buf.extend_from_slice(&c.name);
        buf.push(0x00);
        let tag = match c.kind {
            FileKind::File => b'F',
            FileKind::Dir => b'D',
            FileKind::Symlink => b'L',
        };
        buf.push(tag);
        buf.extend_from_slice(&c.blake3);
        buf.push(0x0a);
    }
    *blake3::hash(&buf).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_dir_has_well_defined_hash() {
        let h = hash_tree(&[]);
        assert_eq!(h, *blake3::hash(b"").as_bytes());
    }

    #[test]
    fn children_sort_by_name_bytes() {
        let mk = |name: &[u8], byte: u8| TreeChild {
            name: name.to_vec(),
            kind: FileKind::File,
            blake3: [byte; 32],
        };
        let h1 = hash_tree(&[mk(b"a", 0x00), mk(b"b", 0x11)]);
        let h2 = hash_tree(&[mk(b"b", 0x11), mk(b"a", 0x00)]);
        assert_eq!(h1, h2);
    }
}
