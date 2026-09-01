//! Default artifact versioning: a content hash over the tree.
//!
//! Steps that know a cheaper or more meaningful version (row-set
//! hash, dolt commit) report it in their [`crate::ArtifactState`];
//! this is the fallback for everyone else. Content (not mtime) so a
//! byte-identical rewrite doesn't cascade re-runs — that's the
//! "content-stable outputs" half of the contract doing its job.
//!
//! [`tree_version`] has exactly one caller, and that is deliberate:
//! `resolve_outputs`, once a step has run and reported no version of
//! its own. The runner never hashes a tree on its own behalf. Versions
//! are a step's to report, and the runner cannot know what is cheap for
//! a given store — the raw stores are doltlite databases that can be
//! asked for a HEAD commit in milliseconds, which the runner
//! deliberately does not know. See [`UNKNOWN`] for what it uses instead
//! when a step it did not run has no recorded version.

use std::path::Path;

use anyhow::{Context, Result};

/// The version reported for an artifact that does not exist on disk.
/// A real version is a 64-character blake3 digest, so this six-letter
/// word cannot collide with one.
///
/// The scheduler does not special-case it: it is compared for equality
/// like any other version, which gives the right answer in both
/// directions. A path that was never produced and still isn't compares
/// equal to itself, so a consumer that already recorded it is not
/// dirtied; a path that existed and was deleted moves from a real
/// digest to this, which is a difference, so its consumers re-run.
pub const ABSENT: &str = "absent";

/// The version used for an artifact whose producer did not run this
/// pass and has no version recorded from an earlier one: the runner
/// genuinely does not know what the tree holds.
///
/// Distinct from [`ABSENT`], which is a claim about the disk —
/// "nothing was ever produced here". The runner cannot make that claim
/// about a step it skipped without reading the tree, and in the case
/// that motivated this (#225) it would have been false: the tree held
/// 3.4 GB. Recording that we don't know is the honest answer, and it is
/// the cheap one.
///
/// Like `ABSENT` it is compared for equality like any other version,
/// which gives the right answer in both directions. Two runs that both
/// know nothing about a tree agree, so a consumer that already recorded
/// this is not dirtied every run; and a real version — always
/// `<fingerprint>:<version>`, so always containing a colon — can never
/// collide with it, so a producer that later runs does dirty its
/// consumers.
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
