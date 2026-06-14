//! Reuse-vs-rehash decision for a single previously-scanned entry.
//!
//! Pure functions, no I/O. Mirrors Unison's `dataClearlyUnchanged`
//! logic from `/Users/thad/src/unison/src/fpcache.ml:243` — see
//! [`EXTRACT.md`](../../EXTRACT.md) §"The fast-rescan trick" for the
//! framework-side description of why we encode the cursor this way.
//!
//! The decision compares the previously-stored
//! [`FileStatsRow`](super::schema_raw::FileStatsRow) against a fresh
//! stat. It does NOT depend on what `stamp_kind` the walker would
//! assign to the new row — the new row's `stamp_kind` is a platform
//! decision made by the walker; reuse-vs-rehash compares only against
//! whatever was stored last time.

use super::schema_raw::{FileStatsRow, StampKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StampDecision {
    ReuseHash,
    Rehash,
}

/// Fresh stat result for one entry. Filled in by the walker from
/// `std::fs::Metadata` + platform-specific syscall bits.
#[derive(Debug, Clone, Copy)]
pub struct FreshStat {
    pub mtime_ns: i64,
    pub size: i64,
    pub inode: Option<i64>,
    pub dev: Option<i64>,
    pub ctime_ns: Option<i64>,
}

pub fn decide(prev: Option<&FileStatsRow>, fresh: &FreshStat) -> StampDecision {
    let Some(prev) = prev else {
        return StampDecision::Rehash;
    };
    if matches!(prev.stamp_kind, StampKind::Rescan) {
        return StampDecision::Rehash;
    }
    if prev.mtime_ns != fresh.mtime_ns || prev.size != fresh.size {
        return StampDecision::Rehash;
    }
    if matches!(prev.stamp_kind, StampKind::Inode)
        && (prev.inode != fresh.inode || prev.dev != fresh.dev)
    {
        return StampDecision::Rehash;
    }
    StampDecision::ReuseHash
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
