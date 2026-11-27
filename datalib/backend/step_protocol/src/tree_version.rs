//! A content version for an output that is a plain tree of files, for a
//! step with no better one (a store's commit is better: it is free).

use std::io;
use std::path::{Path, PathBuf};

/// A version of the files under `dir`: the same string for the same
/// relative paths holding the same bytes, every run and every build.
/// Reads every file, so it costs as much as the tree is big. A missing
/// `dir` is an empty tree. Symbolic links are skipped.
///
/// The hash is 128-bit FNV-1a, which is stable and dependency-free but
/// not cryptographic: fine for "did my own output change?", not for
/// telling apart files someone crafted to collide.
pub fn tree_version(dir: &Path) -> io::Result<String> {
    let mut files = Vec::new();
    match collect(dir, &mut files) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        other => other?,
    }
    files.sort();
    let mut hash = Fnv::new();
    for file in &files {
        let rel = file.strip_prefix(dir).expect("collected under dir");
        let name = rel.to_string_lossy();
        let bytes = std::fs::read(file)?;
        hash.update(&(name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    Ok(format!("{:032x}", hash.0))
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(&entry.path(), files)?;
        } else if kind.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

struct Fnv(u128);

impl Fnv {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;

    fn new() -> Fnv {
        Fnv(Fnv::OFFSET)
    }

    fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u128::from(b);
            self.0 = self.0.wrapping_mul(Fnv::PRIME);
        }
    }
}
