//! Default artifact versioning: a content hash over the tree.
//!
//! The fallback, not the norm: a step reports its own content-derived
//! version, and [`tree_version`] is only reached for a step that just ran and
//! reported none. Content rather than mtime, so a byte-identical rewrite
//! doesn't cascade re-runs. See the crate README.

use std::path::Path;

use anyhow::{Context, Result};

/// The version reported for an artifact that does not exist on disk. A real
/// version is a 64-character blake3 digest, so this cannot collide with one.
///
/// Compared for equality like any other version, which is right in both
/// directions: a tree that was never produced compares equal to itself, so a
/// consumer is not dirtied, and one that was deleted moves to a different
/// string, so its consumers re-run.
pub const ABSENT: &str = "absent";

/// The version for an artifact whose producer did not run this pass and has
/// no version recorded from an earlier one: the runner genuinely does not
/// know what the tree holds.
///
/// Also compared for equality, so two runs that both know nothing agree. A
/// real version always contains a colon (`<fingerprint>:<version>`), so it
/// can never collide with this.
pub const UNKNOWN: &str = "unknown";

/// Hash the tree (or single file) at `path`. Deterministic: files are
/// visited in sorted path order; each contributes its root-relative
/// path and content. A missing path hashes to a distinguished
/// "absent" version so "not yet produced" compares unequal to every
/// real tree.
pub fn tree_version(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(ABSENT.to_string());
    }
    let mut hasher = blake3::Hasher::new();
    if path.is_file() {
        hash_file(&mut hasher, Path::new(""), path)?;
    } else {
        let mut entries: Vec<_> = walkdir::WalkDir::new(path)
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("walk {}", path.display()))?;
        entries.sort_by(|a, b| a.path().cmp(b.path()));
        for e in entries {
            if e.file_type().is_file() {
                let rel = e.path().strip_prefix(path).unwrap_or(e.path());
                hash_file(&mut hasher, rel, e.path())?;
            }
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_file(hasher: &mut blake3::Hasher, rel: &Path, abs: &Path) -> Result<()> {
    hasher.update(rel.to_string_lossy().as_bytes());
    hasher.update(&[0]);
    let bytes = std::fs::read(abs).with_context(|| format!("read {}", abs.display()))?;
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(&bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_across_rewrites_sensitive_to_content() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join("out");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.md"), "hello").unwrap();
        std::fs::write(dir.join("sub/b.md"), "world").unwrap();

        let v1 = tree_version(&dir).unwrap();
        // Byte-identical rewrite (new mtime) → same version.
        std::fs::write(dir.join("a.md"), "hello").unwrap();
        assert_eq!(tree_version(&dir).unwrap(), v1);
        // Content change → different version.
        std::fs::write(dir.join("a.md"), "hello!").unwrap();
        assert_ne!(tree_version(&dir).unwrap(), v1);
    }

    #[test]
    fn absent_is_distinguished() {
        let td = tempfile::tempdir().unwrap();
        let missing = td.path().join("nope");
        assert_eq!(tree_version(&missing).unwrap(), ABSENT);
        std::fs::create_dir_all(&missing).unwrap();
        std::fs::write(missing.join("x"), "x").unwrap();
        assert_ne!(tree_version(&missing).unwrap(), ABSENT);
    }
}
