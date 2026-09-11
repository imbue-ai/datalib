//! The one glob dialect a SQLite-mirroring source's table and column
//! filters speak. Shared by the config crates that validate the
//! patterns and the engine that applies them.

/// Minimal glob match: `*` (any run, including empty) and `?` (exactly
/// one character). Everything else is literal, and matching is
/// case-sensitive — SQLite identifiers here come straight out of
/// `sqlite_master`, so the user sees exactly what they must type.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Classic two-pointer backtracking matcher: O(len(p) * len(t)) worst
    // case, O(n) on patterns without adjacent stars, and no allocation.
    let (mut pi, mut ti) = (0usize, 0usize);
    // Where to resume if the current `*` guess turns out to be too short.
    let (mut star, mut star_ti) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(s) = star {
            // Backtrack: let the star swallow one more character.
            pi = s + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    // Trailing stars in the pattern match the empty remainder.
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_literals_and_wildcards() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
        assert!(glob_match("Adobe_images", "Adobe_images"));
        assert!(!glob_match("Adobe_images", "Adobe_imageProperties"));
        assert!(glob_match("Ag*", "AgLibraryFile"));
        assert!(!glob_match("Ag*", "Adobe_images"));
        assert!(glob_match("*.xmp", "Adobe_AdditionalMetadata.xmp"));
        assert!(glob_match(
            "AgMetadataSearchIndex.*SearchIndex",
            "AgMetadataSearchIndex.exifSearchIndex"
        ));
        assert!(!glob_match(
            "AgMetadataSearchIndex.*SearchIndex",
            "AgMetadataSearchIndex.image"
        ));
        assert!(glob_match("?g*", "AgLibraryFile"));
        assert!(!glob_match("?g*", "Adobe_images"));
        // The backtracking case: a star that must give up its first guess.
        assert!(glob_match("*Oz*Ids", "AgLibraryImageOzAssetIds"));
        assert!(!glob_match("*Oz*Ids", "AgLibraryImageOzAsset"));
        assert!(glob_match("Z_RT_*_node", "Z_RT_Asset_boundedByRect_node"));
    }
}
