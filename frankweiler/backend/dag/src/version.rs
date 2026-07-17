//! Default artifact versioning: a content hash over the tree.
//!
//! Steps that know a cheaper or more meaningful version (row-set
//! hash, dolt commit) report it in their [`crate::ArtifactState`];
//! this is the fallback for everyone else. Content (not mtime) so a
//! byte-identical rewrite doesn't cascade re-runs — that's the
//! "content-stable outputs" half of the contract doing its job.

use std::path::Path;

use anyhow::{Context, Result};

/// Hash the tree (or single file) at `path`. Deterministic: files are
/// visited in sorted path order; each contributes its root-relative
/// path and content. A missing path hashes to a distinguished
/// "absent" version so "not yet produced" compares unequal to every
/// real tree.
pub fn tree_version(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok("absent".to_string());
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
        assert_eq!(tree_version(&missing).unwrap(), "absent");
        std::fs::create_dir_all(&missing).unwrap();
        std::fs::write(missing.join("x"), "x").unwrap();
        assert_ne!(tree_version(&missing).unwrap(), "absent");
    }
}
