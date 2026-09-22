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
    git_hash_and_origin().map(|(hash, _)| hash)
}

/// Which of the two places the commit was read from, for the boot line
/// that says so — a dev binary run by hand gets it from the file
/// `//datalib/backend:bin` stages, a launcher's from the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHashOrigin {
    Env,
    FileBesideBinary,
}

impl GitHashOrigin {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Env => "the DATALIB_GIT_HASH environment variable",
            Self::FileBesideBinary => "the git-hash file beside the binary",
        }
    }
}

/// What a process should say when it has neither.
pub const NO_GIT_HASH_ADVICE: &str = "no commit known for this build: set DATALIB_GIT_HASH, \
     or build //datalib/backend:bin and run the binary from there; \
     the log's source links will be plain text";

pub fn git_hash_and_origin() -> Option<(String, GitHashOrigin)> {
    let env = std::env::var(GIT_HASH_ENV).ok();
    let file = exe_dir().and_then(|d| std::fs::read_to_string(d.join(GIT_HASH_FILE)).ok());
    resolve(env.as_deref(), file.as_deref())
}

/// The environment wins when it names a commit; a value that is not
/// one ("unknown", empty) is the same as none, whichever place it is in.
fn resolve(env: Option<&str>, file: Option<&str>) -> Option<(String, GitHashOrigin)> {
    if let Some(hash) = env.and_then(usable) {
        return Some((hash, GitHashOrigin::Env));
    }
    file.and_then(usable)
        .map(|hash| (hash, GitHashOrigin::FileBesideBinary))
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
    use super::{resolve, usable, GitHashOrigin};

    #[test]
    fn only_a_sha_is_usable() {
        assert_eq!(usable("ae2d52f0").as_deref(), Some("ae2d52f0"));
        assert_eq!(usable(" AE2D52F0\n").as_deref(), Some("ae2d52f0"));
        assert_eq!(usable("unknown"), None);
        assert_eq!(usable(""), None);
        assert_eq!(usable("abc"), None, "too short to name a commit");
        assert_eq!(usable("v0.35.0"), None, "a tag is not a commit");
    }

    /// The launcher's variable beats the staged file, and a file whose
    /// content is not a commit — what the workspace status writes
    /// outside a checkout — counts as absent.
    #[test]
    fn the_environment_wins_and_unknown_is_absent() {
        let file = "ae2d52f0ae2d52f0ae2d52f0ae2d52f0ae2d52f0\n";
        assert_eq!(
            resolve(None, Some(file)),
            Some((
                "ae2d52f0ae2d52f0ae2d52f0ae2d52f0ae2d52f0".into(),
                GitHashOrigin::FileBesideBinary
            ))
        );
        assert_eq!(
            resolve(Some("0fc29cb0fc29cb"), Some(file)),
            Some(("0fc29cb0fc29cb".into(), GitHashOrigin::Env))
        );
        assert_eq!(resolve(Some("unknown"), Some("unknown\n")), None);
        assert_eq!(resolve(None, None), None);
    }
}
