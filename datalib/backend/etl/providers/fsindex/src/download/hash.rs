//! Content hashing for fsindex.
//!
//! Leaf hashing (files, symlink targets) and the `Blake3` digest type
//! now live in [`datalib_etl::fswalk`] — the `pdf` provider needs the
//! same mmap-threshold behavior, and two copies would drift. What
//! stays here is the part only fsindex has: the canonical directory
//! tree-hash defined in [`super::schema_raw`] §"Directory tree-hash
//! canonicalization."

use super::schema_raw::FileKind;

pub use datalib_etl::fswalk::{hash_file, hash_symlink_target, Blake3};

/// One immediate-child contribution to a directory's tree-hash.
pub struct TreeChild {
    pub name: Vec<u8>,
    pub kind: FileKind,
    pub blake3: Blake3,
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
