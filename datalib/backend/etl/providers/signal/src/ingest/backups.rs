//! A folder of Signal snapshots: which the store does not hold yet, and
//! which one HEAD should end on. Pure over the snapshots found and the
//! `ingested_backups` ledger; `fetch` acts on it. `INGEST.md` beside
//! this crate has the rules.

use std::path::{Path, PathBuf};

/// The ledger the plan reads, one row per snapshot read.
pub const LEDGER: &str = "ingested_backups";

const SNAPSHOT_PREFIX: &str = "signal-backup-";

/// A snapshot directory under the backup folder, with its fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The directory's name, `signal-backup-YYYY-MM-DD-HH-MM-SS`, which
    /// sorts in the order the snapshots were taken.
    pub name: String,
    pub dir: PathBuf,
    pub fingerprint: String,
}

/// A ledger row, as the plan needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub fingerprint: String,
    /// The name of the directory it was read from, when the row says.
    pub name: Option<String>,
    /// When it was last written to the store: the newest is what HEAD holds.
    pub ingested_at_utc: String,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// Every snapshot found, oldest first, whether or not the store has it.
    pub found: Vec<Found>,
    /// The ones to read now, oldest first.
    pub ingest: Vec<Found>,
}

/// What HEAD should end on, once the new snapshots are read.
#[derive(Debug, PartialEq, Eq)]
pub enum Top<'a> {
    /// The newest snapshot, which HEAD does not hold: read it again.
    Read(&'a Found),
    /// HEAD already holds the newest snapshot.
    Holds,
    /// The newest snapshot the store holds is no longer in the folder.
    Gone(String),
    /// No snapshot was found or is held.
    Nothing,
}

/// Whether a directory name is one Signal gives a snapshot.
pub fn is_snapshot_name(name: &str) -> bool {
    name.starts_with(SNAPSHOT_PREFIX)
}

pub fn name_of(dir: &Path) -> Option<String> {
    dir.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// Known by content, not by name: a snapshot copied or renamed is the
/// one the store already has.
pub fn plan(mut found: Vec<Found>, ledger: &[Held]) -> Plan {
    found.sort_by(|a, b| a.name.cmp(&b.name));
    let ingest = found
        .iter()
        .filter(|f| !ledger.iter().any(|h| h.fingerprint == f.fingerprint))
        .cloned()
        .collect();
    Plan { found, ingest }
}

/// Decided after this run's reads, from the ledger as they left it.
/// `failed` names the snapshots that would not read this run.
pub fn top<'a>(plan: &'a Plan, ledger: &[Held], failed: &[String]) -> Top<'a> {
    let newest_here = plan
        .found
        .iter()
        .filter(|f| !failed.contains(&f.name))
        .max_by(|a, b| a.name.cmp(&b.name));
    let newest_gone = ledger
        .iter()
        .filter(|h| !plan.found.iter().any(|f| f.fingerprint == h.fingerprint))
        .filter_map(|h| h.name.as_deref())
        .max();
    let on_head = ledger
        .iter()
        .max_by(|a, b| a.ingested_at_utc.cmp(&b.ingested_at_utc));
    match (newest_here, newest_gone) {
        (Some(here), Some(gone)) if gone > here.name.as_str() => Top::Gone(gone.to_string()),
        (None, Some(gone)) => Top::Gone(gone.to_string()),
        (Some(here), _) if on_head.is_some_and(|h| h.fingerprint == here.fingerprint) => Top::Holds,
        (Some(here), _) => Top::Read(here),
        (None, None) => Top::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(day: u32) -> Found {
        let name = format!("signal-backup-2364-04-{day:02}-01-00-00");
        Found {
            dir: PathBuf::from("/B").join(&name),
            fingerprint: format!("fp-{day}"),
            name,
        }
    }

    fn held(day: u32, ingested_at: &str) -> Held {
        Held {
            fingerprint: format!("fp-{day}"),
            name: Some(format!("signal-backup-2364-04-{day:02}-01-00-00")),
            ingested_at_utc: ingested_at.into(),
        }
    }

    fn days(fs: &[Found]) -> Vec<String> {
        fs.iter().map(|f| f.fingerprint.clone()).collect()
    }

    #[test]
    fn snapshots_the_store_lacks_are_read_oldest_first() {
        let p = plan(
            vec![found(12), found(10), found(11)],
            &[held(10, "2364-04-10T02:00:00Z")],
        );
        assert_eq!(days(&p.ingest), ["fp-11", "fp-12"]);
        assert_eq!(p.found.len(), 3);
    }

    /// A snapshot is known by its bytes: renamed, it is still held.
    #[test]
    fn a_renamed_snapshot_is_the_one_already_held() {
        let mut renamed = found(10);
        renamed.name = "signal-backup-2364-04-10-01-00-00 copy".into();
        let p = plan(vec![renamed], &[held(10, "2364-04-10T02:00:00Z")]);
        assert!(p.ingest.is_empty());
    }

    #[test]
    fn head_already_on_the_newest_reads_nothing_more() {
        let p = plan(vec![found(10), found(11)], &[]);
        let ledger = [
            held(10, "2364-04-12T00:00:00.000001Z"),
            held(11, "2364-04-12T00:00:00.000002Z"),
        ];
        assert_eq!(top(&p, &ledger, &[]), Top::Holds);
    }

    /// An older snapshot that turned up late was read last, so HEAD holds
    /// it: the newest is read again to put the present back on top.
    #[test]
    fn a_late_older_snapshot_puts_the_newest_back_on_top() {
        let p = plan(vec![found(10), found(11)], &[]);
        let ledger = [
            held(11, "2364-04-12T00:00:00Z"),
            held(10, "2364-04-13T00:00:00Z"),
        ];
        assert_eq!(top(&p, &ledger, &[]), Top::Read(&p.found[1]));
    }

    /// One that would not read is never what HEAD ends on.
    #[test]
    fn a_snapshot_that_failed_is_not_put_on_top() {
        let p = plan(vec![found(10), found(11)], &[]);
        let ledger = [held(10, "2364-04-12T00:00:00Z")];
        let failed = [found(11).name];
        assert_eq!(top(&p, &ledger, &failed), Top::Holds);
    }

    /// Reading an older snapshot on top would delete from HEAD what only
    /// the vanished newer one held, and look like data loss.
    #[test]
    fn the_newest_held_gone_from_the_folder_leaves_head_alone() {
        let p = plan(vec![found(10)], &[]);
        let ledger = [
            held(10, "2364-04-12T00:00:00Z"),
            held(11, "2364-04-13T00:00:00Z"),
        ];
        assert_eq!(top(&p, &ledger, &[]), Top::Gone(found(11).name));
    }

    #[test]
    fn an_older_held_snapshot_gone_from_the_folder_does_not_matter() {
        let p = plan(vec![found(11)], &[]);
        let ledger = [
            held(10, "2364-04-12T00:00:00Z"),
            held(11, "2364-04-13T00:00:00Z"),
        ];
        assert_eq!(top(&p, &ledger, &[]), Top::Holds);
    }

    #[test]
    fn an_empty_folder_and_ledger_is_nothing() {
        let p = plan(vec![], &[]);
        assert_eq!(top(&p, &[], &[]), Top::Nothing);
    }

    #[test]
    fn only_signal_snapshot_directories_count() {
        assert!(is_snapshot_name("signal-backup-2026-10-16-00-22-29"));
        assert!(!is_snapshot_name("files"));
        assert!(!is_snapshot_name(".DS_Store"));
    }
}
