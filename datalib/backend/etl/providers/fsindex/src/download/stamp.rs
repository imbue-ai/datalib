//! Reuse-vs-rehash decision for a single previously-scanned entry.
//!
//! The rule itself — Unison's `dataClearlyUnchanged`, plus the
//! `nostamp` / `rescan` special cases — now lives in
//! [`datalib_etl::fswalk`], because the `pdf` provider needs the same
//! fast-rescan behavior and a second copy would drift. This module is
//! the fsindex-side adapter: it maps our
//! [`FileStatsRow`](super::schema_raw::FileStatsRow) cursor onto the
//! shared [`fswalk::StampCursor`] and delegates.
//!
//! See [`DOWNLOAD.md`](../../DOWNLOAD.md) §"The fast-rescan trick" for
//! why the cursor is encoded the way it is.

use datalib_etl::fswalk;

use super::schema_raw::{FileStatsRow, StampKind};

pub use datalib_etl::fswalk::{FreshStat, StampDecision};

/// Map fsindex's stored stamp discriminator onto the shared one. The
/// two enums are deliberately separate: `schema_raw::StampKind` is part
/// of fsindex's on-disk row shape, and shouldn't move just because the
/// comparison logic did.
fn shared_kind(k: StampKind) -> fswalk::StampKind {
    match k {
        StampKind::Inode => fswalk::StampKind::Inode,
        StampKind::NoStamp => fswalk::StampKind::NoStamp,
        StampKind::Rescan => fswalk::StampKind::Rescan,
    }
}

fn cursor_of(prev: &FileStatsRow) -> fswalk::StampCursor {
    fswalk::StampCursor {
        mtime_ns: prev.mtime_ns,
        size: prev.size,
        stamp_kind: shared_kind(prev.stamp_kind),
        inode: prev.inode,
        dev: prev.dev,
    }
}

pub fn decide(prev: Option<&FileStatsRow>, fresh: &FreshStat) -> StampDecision {
    let cursor = prev.map(cursor_of);
    fswalk::decide(cursor.as_ref(), fresh)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        stamp: StampKind,
        mtime: i64,
        size: i64,
        inode: Option<i64>,
        dev: Option<i64>,
    ) -> FileStatsRow {
        FileStatsRow {
            id: "x".into(),
            mtime_ns: mtime,
            size,
            stamp_kind: stamp,
            inode,
            dev,
            ctime_ns: None,
        }
    }
    fn stat(mtime: i64, size: i64, inode: Option<i64>, dev: Option<i64>) -> FreshStat {
        FreshStat {
            mtime_ns: mtime,
            size,
            inode,
            dev,
            ctime_ns: None,
        }
    }

    #[test]
    fn no_prev_means_rehash() {
        assert_eq!(decide(None, &stat(1, 1, None, None)), StampDecision::Rehash);
    }

    #[test]
    fn rescan_kind_forces_rehash_even_when_triple_matches() {
        let p = row(StampKind::Rescan, 1, 1, Some(7), Some(0));
        let f = stat(1, 1, Some(7), Some(0));
        assert_eq!(decide(Some(&p), &f), StampDecision::Rehash);
    }

    #[test]
    fn inode_match_reuses() {
        let p = row(StampKind::Inode, 1, 1, Some(7), Some(0));
        let f = stat(1, 1, Some(7), Some(0));
        assert_eq!(decide(Some(&p), &f), StampDecision::ReuseHash);
    }

    #[test]
    fn inode_mismatch_rehashes() {
        let p = row(StampKind::Inode, 1, 1, Some(7), Some(0));
        let f = stat(1, 1, Some(8), Some(0));
        assert_eq!(decide(Some(&p), &f), StampDecision::Rehash);
    }

    #[test]
    fn nostamp_ignores_inode() {
        let p = row(StampKind::NoStamp, 1, 1, None, None);
        let f = stat(1, 1, Some(99), Some(0));
        assert_eq!(decide(Some(&p), &f), StampDecision::ReuseHash);
    }
}
