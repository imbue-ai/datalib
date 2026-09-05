//! Build-time version stamps surfaced to runtime.

/// SHA of the commit this binary was built from, or `"unknown"` when the
/// build environment couldn't supply one. Stamped onto every feedback row
/// so we can correlate filed feedback to the exact code that produced the
/// surface the user was complaining about.
pub fn git_hash() -> &'static str {
    option_env!("DATALIB_GIT_HASH").unwrap_or("unknown")
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
