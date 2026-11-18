//! What earlier builds wrote into a render step's folder beside its store,
//! now that a document is spelled from the store alone: a `.md` per
//! document, the `blobs/` folders an older layout kept beside one, and the
//! time-series `plots/` pages. Render clears them before each run, so a
//! root keeps no copy nothing reads; the store, its lock and `CACHEDIR.TAG`
//! stay. Best-effort: a file that will not go is logged and left.

use std::path::{Path, PathBuf};

/// A path under a render folder that an earlier build wrote.
fn is_old(path: &Path, is_dir: bool) -> bool {
    if is_dir {
        matches!(
            path.file_name().and_then(|n| n.to_str()),
            Some("blobs" | "plots")
        )
    } else {
        path.extension().is_some_and(|e| e == "md")
    }
}

/// Clears `render_root` of what [`is_old`] names, and of the folders that
/// leaves empty. Says how many entries went.
pub(crate) fn clear(render_root: &Path) -> usize {
    let mut removed = 0;
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut pending = vec![render_root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let gone = match (kind.is_dir(), is_old(&path, kind.is_dir())) {
                (true, true) => std::fs::remove_dir_all(&path),
                (false, true) => std::fs::remove_file(&path),
                (true, false) => {
                    dirs.push(path.clone());
                    pending.push(path);
                    continue;
                }
                (false, false) => continue,
            };
            match gone {
                Ok(()) => removed += 1,
                Err(e) => tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "could not delete a file an earlier build rendered",
                ),
            }
        }
    }
    // Deepest first, so a folder emptied by its children goes too.
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for dir in dirs {
        let _ = std::fs::remove_dir(&dir);
    }
    if removed > 0 {
        tracing::info!(
            folder = %render_root.display(),
            removed,
            "deleted the markdown files an earlier build rendered; documents live in the render store",
        );
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }

    /// Every path under `root`, folders included, relative and sorted.
    fn listing(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                out.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
                if path.is_dir() {
                    pending.push(path);
                }
            }
        }
        out.sort();
        out
    }

    /// Every layout an earlier build left goes — a document's file in its
    /// shard folder, one under `unindexed/`, one in a folder of its own
    /// with `blobs/` beside it, the `plots/` pages — and the store, its
    /// lock and `CACHEDIR.TAG` stay. A second pass finds nothing.
    #[test]
    fn old_documents_go_and_the_store_stays() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        for kept in [
            "indexed_markdown.doltlite_db",
            "indexed_markdown.doltlite_db.lock",
            "CACHEDIR.TAG",
        ] {
            touch(root, kept);
        }
        for old in [
            "55/00000000-0000-85ee-bff7-b3a1f5972355.md",
            "unindexed/55/00000000-0000-85ee-bff7-b3a1f5972356.md",
            "md-1/all.md",
            "md-1/blobs/photo.png",
            "plots/pm25.html",
        ] {
            touch(root, old);
        }

        assert_eq!(clear(root), 5);
        assert_eq!(
            listing(root),
            vec![
                "CACHEDIR.TAG",
                "indexed_markdown.doltlite_db",
                "indexed_markdown.doltlite_db.lock",
            ]
        );
        assert_eq!(clear(root), 0);
    }

    /// A render folder that does not exist yet is nothing to clear.
    #[test]
    fn a_missing_folder_clears_nothing() {
        let td = tempfile::tempdir().unwrap();
        assert_eq!(clear(&td.path().join("absent")), 0);
    }
}
