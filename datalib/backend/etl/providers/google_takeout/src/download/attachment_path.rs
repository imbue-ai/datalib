//! Resolve a Takeout attachment's referenced filename to its on-disk
//! path, tolerating Google's silent truncation of long names.
//!
//! Google Takeout records the *full* attachment filename in Chat's
//! `messages.json` (`export_name`) but truncates the file actually
//! written to disk to a byte cap, preserving the extension. The on-disk
//! stem is therefore a prefix of the referenced stem, e.g.
//!
//! ```text
//! referenced: File-84f97831-a19d-4d1b-89f0-dfc9cbd19ca4-1_all_148157.jpeg
//! on disk:    File-84f97831-a19d-4d1b-89f0-dfc9cbd19ca4-1_al.jpeg
//! ```
//!
//! so an exact `dir.join(name)` misses for any longish filename. See
//! issue #64.
//!
//! [`resolve`] tries the exact name first, then falls back to the
//! **unique longest same-extension prefix** in the directory. Requiring
//! the extension to match is what keeps this safe: it anchors the match
//! to a real attachment file rather than, say, a sibling `.json`/`.html`
//! whose stem happens to be a prefix. Picking the *longest* prefix
//! ignores coincidental short prefixes; a tie at the longest length
//! (only reachable via case-folding extension collisions on a
//! case-sensitive filesystem) degrades to [`Resolved::Missing`] rather
//! than guessing.
//!
//! Note: Google Voice's MMS `src` is extension-less, so this resolver is
//! deliberately NOT used there — an extension-less prefix match would
//! grab the conversation's own `.html` transcript. Voice truncation needs
//! a media-extension-restricted matcher validated against real data;
//! tracked as a follow-up on #64.

use std::path::{Path, PathBuf};

/// Outcome of resolving a referenced attachment name against a directory.
#[derive(Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The file exists under its exact referenced name.
    Exact(PathBuf),
    /// Matched a truncated on-disk name via the unique longest
    /// same-extension prefix.
    Truncated(PathBuf),
    /// No exact match, and no unique same-extension prefix match.
    Missing,
}

/// Split `name` into `(stem, ext)` on the last `.`. A name with no `.`
/// (or a leading-dot dotfile) has an empty extension.
fn split_ext(name: &str) -> (&str, &str) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, ext),
        _ => (name, ""),
    }
}

/// Resolve `referenced` (the full name from the export) to an on-disk
/// path under `dir`, tolerating truncation. See module docs.
pub fn resolve(dir: &Path, referenced: &str) -> Resolved {
    let exact = dir.join(referenced);
    if exact.is_file() {
        return Resolved::Exact(exact);
    }

    let (want_stem, want_ext) = split_ext(referenced);
    let want_ext = want_ext.to_ascii_lowercase();

    let Ok(rd) = std::fs::read_dir(dir) else {
        return Resolved::Missing;
    };

    // Candidates: files with the same extension whose stem is a prefix of
    // the referenced stem — i.e. the referenced name truncated down to
    // this on-disk name. Byte-wise prefix so non-ASCII names work.
    let mut candidates: Vec<(usize, PathBuf)> = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let (stem, ext) = split_ext(name);
        if ext.to_ascii_lowercase() != want_ext {
            continue;
        }
        if want_stem.as_bytes().starts_with(stem.as_bytes()) {
            candidates.push((stem.len(), p));
        }
    }

    if candidates.is_empty() {
        return Resolved::Missing;
    }
    // The real truncated file is the longest prefix. A tie at the max
    // length (only possible via case-folded extension collisions) is
    // unsafe to pick from, so fall through to Missing.
    let max_len = candidates.iter().map(|(l, _)| *l).max().unwrap();
    let mut top: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|(l, _)| *l == max_len)
        .map(|(_, p)| p)
        .collect();
    if top.len() == 1 {
        Resolved::Truncated(top.pop().unwrap())
    } else {
        Resolved::Missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, b"x").unwrap();
        p
    }

    #[test]
    fn exact_match_wins() {
        let d = tempdir().unwrap();
        let p = touch(d.path(), "course-laid-in.txt");
        assert_eq!(resolve(d.path(), "course-laid-in.txt"), Resolved::Exact(p));
    }

    #[test]
    fn truncated_prefix_same_extension() {
        let d = tempdir().unwrap();
        // On-disk name is the referenced name truncated, extension kept.
        let p = touch(
            d.path(),
            "File-84f97831-a19d-4d1b-89f0-dfc9cbd19ca4-1_al.jpeg",
        );
        let got = resolve(
            d.path(),
            "File-84f97831-a19d-4d1b-89f0-dfc9cbd19ca4-1_all_148157.jpeg",
        );
        assert_eq!(got, Resolved::Truncated(p));
    }

    #[test]
    fn different_extension_does_not_match() {
        let d = tempdir().unwrap();
        touch(
            d.path(),
            "File-84f97831-a19d-4d1b-89f0-dfc9cbd19ca4-1_al.png",
        );
        let got = resolve(
            d.path(),
            "File-84f97831-a19d-4d1b-89f0-dfc9cbd19ca4-1_all_148157.jpeg",
        );
        assert_eq!(got, Resolved::Missing);
    }

    #[test]
    fn sibling_with_other_extension_is_not_grabbed() {
        // A `.html`/`.json` sibling whose stem is a prefix of the
        // reference must NOT be matched (the Voice false-positive that
        // kept this resolver extension-anchored).
        let d = tempdir().unwrap();
        touch(d.path(), "Wes - Text - 2019.html");
        let got = resolve(d.path(), "Wes - Text - 2019-1-1.jpg");
        assert_eq!(got, Resolved::Missing);
    }

    #[test]
    fn coincidental_short_prefix_is_outranked_by_real_truncation() {
        let d = tempdir().unwrap();
        // A short, differently-named .jpeg that happens to share a tiny
        // prefix must NOT win over the real (longer) truncated file.
        touch(d.path(), "File-8.jpeg");
        let real = touch(
            d.path(),
            "File-84f97831-a19d-4d1b-89f0-dfc9cbd19ca4-1_al.jpeg",
        );
        let got = resolve(
            d.path(),
            "File-84f97831-a19d-4d1b-89f0-dfc9cbd19ca4-1_all_148157.jpeg",
        );
        assert_eq!(got, Resolved::Truncated(real));
    }

    #[test]
    fn longest_same_extension_prefix_is_unique() {
        // Two same-extension prefixes; the longer (real truncation) wins,
        // the shorter is outranked — no ambiguity for a fixed extension.
        let d = tempdir().unwrap();
        touch(d.path(), "File-abcd.jpeg");
        let real = touch(d.path(), "File-abcde.jpeg");
        assert_eq!(
            resolve(d.path(), "File-abcde_148157.jpeg"),
            Resolved::Truncated(real),
        );
    }

    #[test]
    fn missing_when_no_candidate() {
        let d = tempdir().unwrap();
        touch(d.path(), "unrelated.png");
        assert_eq!(resolve(d.path(), "File-whatever.jpeg"), Resolved::Missing);
    }
}
