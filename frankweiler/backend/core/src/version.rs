//! Build-time version stamps surfaced to runtime.
//!
//! `git_hash()` returns the commit SHA the binary was built from, sourced
//! from the `FRANKWEILER_GIT_HASH` rustc env var. For cargo builds that
//! var is set by `build.rs`; for Bazel builds it will be set via the
//! workspace status stamp (`tools/workspace_status.sh`) once the stamp
//! file is wired into `rust_library.rustc_env_files`. Until then, Bazel
//! builds report the literal string `"unknown"` — the same fallback used
//! when the build happens outside a git checkout altogether.

/// SHA of the commit this binary was built from, or `"unknown"` when the
/// build environment couldn't supply one. Stamped onto every feedback row
/// so we can correlate filed feedback to the exact code that produced the
/// surface the user was complaining about.
pub fn git_hash() -> &'static str {
    option_env!("FRANKWEILER_GIT_HASH").unwrap_or("unknown")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_hash_is_non_empty() {
        let h = git_hash();
        assert!(!h.is_empty());
        // Either a real SHA or the documented fallback.
        assert!(h == "unknown" || h.len() >= 7);
    }
}
