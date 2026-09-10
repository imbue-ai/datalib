//! Content hashing for fsindex.

use super::schema_raw::FileKind;

pub use datalib_etl::fswalk::{hash_file, hash_symlink_target, Blake3};

/// One immediate-child contribution to a directory's tree-hash.
pub struct TreeChild {
    pub name: Vec<u8>,
    pub kind: FileKind,
    pub blake3: Blake3,
}

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
