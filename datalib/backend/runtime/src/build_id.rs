//! Which build this is: the datalib version, and the commit it came
//! from. The run store records the commit beside each run so the log
//! view can link a line back to the source that wrote it; every store
//! records both in `_datalib_meta` so a later build knows who wrote it.
//!
//! The commit is resolved at run time, never compiled in: a stamped
//! build costs a rebuild of everything downstream on every commit
//! (`.bazelrc` §stamp), and this is one string.

use std::path::{Path, PathBuf};

/// The workspace version, from this crate's `version` attr in
/// `BUILD.bazel` — rules_rust does not read `Cargo.toml`, so the attr
/// is a copy and `//datalib/backend:version_consistency_test` keeps it
/// equal to `[workspace.package].version`.
pub const DATALIB_VERSION: &str = env!("CARGO_PKG_VERSION");

/// A dev launcher sets this to `git rev-parse HEAD` of the checkout it
/// built from (`datalib/dev_runtime.sh`). Wins over the file: the
/// launcher knows which checkout, the binary does not.
pub const GIT_HASH_ENV: &str = "DATALIB_GIT_HASH";

/// A release tarball and the .app carry one beside the binaries, the
/// way `runtime.manifest` sits there (`docs/dev/runtime_fetch.md`).
pub const GIT_HASH_FILE: &str = "git-hash";

pub fn git_hash() -> Option<String> {
    std::env::var(GIT_HASH_ENV)
        .ok()
        .and_then(|s| usable(&s))
        .or_else(|| exe_dir().and_then(|d| from_file(&d.join(GIT_HASH_FILE))))
}

fn from_file(path: &Path) -> Option<String> {
    usable(&std::fs::read_to_string(path).ok()?)
}

fn exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    exe.parent().map(Path::to_path_buf)
}

/// A hex commit id and nothing else: it goes into a URL downstream.
fn usable(s: &str) -> Option<String> {
    let s = s.trim();
    let looks_like_a_sha = s.len() >= 7 && s.bytes().all(|b| b.is_ascii_hexdigit());
    looks_like_a_sha.then(|| s.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::{from_file, usable};

    #[test]
    fn only_a_sha_is_usable() {
        assert_eq!(usable("ae2d52f0").as_deref(), Some("ae2d52f0"));
        assert_eq!(usable(" AE2D52F0\n").as_deref(), Some("ae2d52f0"));
        assert_eq!(usable("unknown"), None);
        assert_eq!(usable(""), None);
        assert_eq!(usable("abc"), None, "too short to name a commit");
        assert_eq!(usable("v0.35.0"), None, "a tag is not a commit");
    }

    #[test]
    fn the_file_beside_the_binary_is_one_line() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("git-hash");
        assert_eq!(from_file(&path), None, "absent is absent, not an error");
        std::fs::write(&path, "ae2d52f0ae2d52f0ae2d52f0ae2d52f0ae2d52f0\n").unwrap();
        assert_eq!(
            from_file(&path).as_deref(),
            Some("ae2d52f0ae2d52f0ae2d52f0ae2d52f0ae2d52f0")
        );
    }
}
