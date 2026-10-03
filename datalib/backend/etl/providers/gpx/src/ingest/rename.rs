//! Which key a file's rows go under. A file keeps the key it was first
//! stored under across renames, so renaming a file, even while editing
//! it, rewrites only what the edit changed. Pure; `INGEST.md` §"Renames"
//! is the reader's guide.

use std::collections::HashSet;

/// A file that is gone this scan, and the points its rows named.
pub struct Vanished<'a> {
    pub path: &'a str,
    pub point_ids: &'a HashSet<String>,
}

/// The vanished file a new one is a renamed (and perhaps edited) copy
/// of: the one sharing the most points, if those are at least half of
/// the larger of the two. A file with no points matches nothing. Ties go
/// to the first in `vanished`, so the caller passes them in a fixed
/// order.
pub fn heir_of(point_ids: &HashSet<String>, vanished: &[Vanished<'_>]) -> Option<usize> {
    if point_ids.is_empty() {
        return None;
    }
    let mut best: Option<(usize, usize)> = None;
    for (i, v) in vanished.iter().enumerate() {
        let shared = point_ids.intersection(v.point_ids).count();
        let larger = point_ids.len().max(v.point_ids.len());
        if 2 * shared >= larger && best.is_none_or(|(_, s)| shared > s) {
            best = Some((i, shared));
        }
    }
    best.map(|(i, _)| i)
}

/// A key for a file seen for the first time: short, so a million member
/// rows do not each repeat a path, and not one any stored file holds.
/// Minted from the path and the content so that a file created at a path
/// whose old file was renamed away (and took its key along) gets another.
pub fn mint_file_key(path: &str, blake3: &str, in_use: &HashSet<String>) -> String {
    (0u64..)
        .map(|n| {
            let mut h = blake3::Hasher::new();
            h.update(path.as_bytes());
            h.update(b"\x1f");
            h.update(blake3.as_bytes());
            h.update(b"\x1f");
            h.update(&n.to_le_bytes());
            h.finalize().to_hex()[..16].to_string()
        })
        .find(|k| !in_use.contains(k))
        .expect("an unused key among 2^64 tries")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(range: std::ops::Range<u32>) -> HashSet<String> {
        range.map(|i| format!("p{i}")).collect()
    }

    #[test]
    fn a_renamed_and_edited_file_finds_its_old_self() {
        let old = ids(0..100);
        let other = ids(500..600);
        let vanished = [
            Vanished {
                path: "other.gpx",
                point_ids: &other,
            },
            Vanished {
                path: "old.gpx",
                point_ids: &old,
            },
        ];
        // Two points edited, one trimmed off the end.
        let mut new = ids(2..99);
        new.insert("edited-0".into());
        new.insert("edited-1".into());
        assert_eq!(heir_of(&new, &vanished), Some(1));
    }

    #[test]
    fn a_small_excerpt_of_a_big_file_is_a_new_file() {
        let old = ids(0..1000);
        let vanished = [Vanished {
            path: "week.gpx",
            point_ids: &old,
        }];
        assert_eq!(heir_of(&ids(0..10), &vanished), None);
    }

    #[test]
    fn a_file_without_points_matches_nothing() {
        let old = HashSet::new();
        let vanished = [Vanished {
            path: "empty.gpx",
            point_ids: &old,
        }];
        assert_eq!(heir_of(&HashSet::new(), &vanished), None);
    }

    #[test]
    fn a_minted_key_steps_around_one_in_use() {
        let first = mint_file_key("a.gpx", "00", &HashSet::new());
        assert_eq!(first.len(), 16);
        let second = mint_file_key("a.gpx", "00", &HashSet::from([first.clone()]));
        assert_ne!(first, second);
        assert_eq!(mint_file_key("a.gpx", "00", &HashSet::new()), first);
    }
}
