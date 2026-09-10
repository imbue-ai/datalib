//! What a directory tree weighs, by stat calls alone.
//!
//! No file is ever opened, so nothing here reads content or computes a
//! digest. That is what makes it cheap enough for the usage monitor to run
//! every few seconds while a sync is in flight. `datalib_etl`'s `fsscan`
//! also walks trees, but it walks them to *hash* them and keeps a cursor
//! of what a consumer has ingested; the two answer different questions.
//!
//! Symlinks are not followed: a cycle would never return, and a shared
//! target would be counted twice. A directory that cannot be read counts
//! as empty rather than failing the walk — a size is worth reporting with
//! a hole in it.

use std::path::Path;

/// One directory and everything under it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TreeSize {
    pub bytes: u64,
    /// Files only. A directory is a container here, not an item.
    pub files: u64,
}

pub fn measure(dir: &Path) -> TreeSize {
    measure_subtrees(dir, &mut |_, _| {})
}

/// [`measure`], plus each directory beneath `dir` handed its own subtotal.
///
/// One walk answers for the root and for every subtree at once. That is the
/// whole point of the callback: the trees a caller asks about are all under
/// one root, and walking once per tree would re-read everything they share.
///
/// `subtree` is called after a directory's subtotal is complete, with the
/// directory's slash-separated path relative to `dir`. `dir` itself is not
/// reported — its total is the return value.
pub fn measure_subtrees<F>(dir: &Path, subtree: &mut F) -> TreeSize
where
    F: FnMut(&str, TreeSize),
{
    fn walk<F: FnMut(&str, TreeSize)>(dir: &Path, rel: &str, subtree: &mut F) -> TreeSize {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return TreeSize::default();
        };
        let mut total = TreeSize::default();
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = path.symlink_metadata() else {
                continue;
            };
            if meta.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let child = if rel.is_empty() {
                    name.into_owned()
                } else {
                    format!("{rel}/{name}")
                };
                let sub = walk(&path, &child, subtree);
                total.bytes += sub.bytes;
                total.files += sub.files;
            } else {
                total.bytes += meta.len();
                total.files += 1;
            }
        }
        if !rel.is_empty() {
            subtree(rel, total);
        }
        total
    }
    walk(dir, "", subtree)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::create_dir_all(root.join("c")).unwrap();
        std::fs::write(root.join("a/one"), vec![0u8; 10]).unwrap();
        std::fs::write(root.join("a/b/two"), vec![0u8; 20]).unwrap();
        std::fs::write(root.join("c/three"), vec![0u8; 30]).unwrap();
        std::fs::write(root.join("four"), vec![0u8; 40]).unwrap();
        td
    }

    #[test]
    fn totals_every_file_under_the_root() {
        let td = fixture();
        assert_eq!(
            measure(td.path()),
            TreeSize {
                bytes: 100,
                files: 4
            }
        );
    }

    /// Subtotals nest: a subtree's bytes are also its parent's, and the
    /// parent's are the root's. One walk, every answer.
    #[test]
    fn each_subtree_gets_its_own_nested_subtotal() {
        let td = fixture();
        let mut seen: std::collections::BTreeMap<String, TreeSize> = Default::default();
        let total = measure_subtrees(td.path(), &mut |rel, size| {
            seen.insert(rel.to_string(), size);
        });

        assert_eq!(total.bytes, 100);
        assert_eq!(seen["a"].bytes, 30);
        assert_eq!(seen["a/b"].bytes, 20);
        assert_eq!(seen["c"].bytes, 30);
        assert_eq!(seen.keys().collect::<Vec<_>>(), ["a", "a/b", "c"]);
    }

    /// A symlink contributes its own link size and is never traversed,
    /// so a loop terminates and a shared target is counted once.
    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed() {
        let td = fixture();
        std::os::unix::fs::symlink(td.path().join("a"), td.path().join("c/loop")).unwrap();
        let m = measure(td.path());
        assert_eq!(m.files, 5, "the link itself counts as one entry");
        assert!(
            m.bytes < 200,
            "but its target's 30 bytes are not re-counted"
        );
    }

    #[test]
    fn an_unreadable_or_missing_directory_is_empty_rather_than_fatal() {
        let td = tempfile::tempdir().unwrap();
        assert_eq!(measure(&td.path().join("nope")), TreeSize::default());
    }
}
