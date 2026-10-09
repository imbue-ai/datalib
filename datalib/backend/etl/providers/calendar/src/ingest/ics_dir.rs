//! A directory of `.ics` files: a Google Takeout `Calendar/` folder, an
//! Apple Calendar export. Each file is one calendar and the whole of
//! it, so an event a re-read file no longer carries was deleted.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_files::file_checkpoint;
use datalib_etl_files::fingerprint_cache::FingerprintCache;
use datalib_etl_files::fsscan;
use datalib_problems::{Outcome, Problem, Reason};
use tracing::warn;

use super::db::RawDb;
use super::schema_raw::{AccountRow, CalendarRow, IcsObjectRow};
use super::FetchSummary;
use crate::ical;

pub struct FetchOptions {
    pub db: RawDb,
    pub input_path: PathBuf,
    /// Host-wide fingerprint cache, so an unchanged file costs a `stat`.
    pub cache: FingerprintCache,
    pub progress: Progress,
    pub control: DownloadControl,
}

const CHECKPOINT_SCOPE: &str = "calendar/ics";
const ACCOUNT_ID: &str = "ics";

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    run_problems::collecting(&pool, &stop, |found| read_folder(opts, found)).await
}

async fn read_folder(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let db = &opts.db;
    db.upsert_account(&AccountRow {
        id: ACCOUNT_ID.into(),
        method: "ics".into(),
        server_url: Some(format!("file://{}", opts.input_path.display())),
        principal_href: None,
        login: None,
    })
    .await?;

    let scan = fsscan::scan(
        &opts.cache,
        &opts.input_path,
        &fsscan::ScanOptions::default(),
        |p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("ics")),
    )
    .await?;
    let mut summary = FetchSummary {
        calendars: scan.files.len(),
        errors: scan.errors.len(),
        ..Default::default()
    };
    for e in &scan.errors {
        warn!(event = "calendar_ics_walk_error", path = %e.path.display(), error = %e.error, "an entry of the ics directory could not be walked");
    }

    let prev = file_checkpoint::load_cursor(db.pool(), CHECKPOINT_SCOPE).await?;
    let changes = scan.changes_since(&prev);
    summary.files_skipped = changes.unchanged;
    opts.progress.set_length(Some(scan.files.len() as u64));
    opts.progress.inc(changes.unchanged as u64);
    let mut read: BTreeSet<&str> = BTreeSet::new();
    scan.report_problems(&found, "files");
    for f in changes.needs_reading_by_path() {
        if opts.control.stop.requested() {
            break;
        }
        opts.progress
            .set_message(&format!("reading {}", f.path.display()));
        match ingest_one(db, &scan.given_resolved, &f.path, &mut summary).await {
            // Stamped only after a clean read, so a crash mid-file leaves
            // no cursor and the next run reads it again.
            Ok(without_uid) => {
                let mut tx = db.pool().begin().await.context("begin ics stamp tx")?;
                file_checkpoint::record_file_with_problem(
                    &mut tx,
                    CHECKPOINT_SCOPE,
                    f,
                    no_uid_problem(without_uid),
                )
                .await?;
                tx.commit().await.context("commit ics stamp tx")?;
                read.insert(f.rel.as_str());
            }
            // Not stamped, so the next run reads it again.
            Err(e) => {
                summary.errors += 1;
                found.listing(&format!("ics {}", f.rel), format!("{e:#}"));
            }
        }
        opts.progress.inc(1);
    }

    // A file is the whole of its calendar, so a file that is gone takes
    // its calendar with it.
    for rel in changes.gone_by_path(&read) {
        let calendar_id = calendar_id(&scan.given_resolved, &scan.root.join(rel));
        summary.events_deleted += db
            .delete_file_calendar(&calendar_id, CHECKPOINT_SCOPE, rel)
            .await?;
        summary.files_removed += 1;
    }
    Ok(summary)
}

/// The `file:` row of a file whose events have no `UID`: nothing tells
/// one apart from the next run's, so they are not stored.
fn no_uid_problem(without_uid: usize) -> Option<(Outcome, Problem)> {
    (without_uid > 0).then(|| {
        (
            Outcome::Dropped,
            Problem::record(
                Reason::NoIdentity,
                &format!("{without_uid} events have no UID, so they cannot be stored"),
            ),
        )
    })
}

/// Store one file's calendar; returns how many of its events had no
/// `UID` and were left out.
async fn ingest_one(
    db: &RawDb,
    root: &Path,
    file: &Path,
    summary: &mut FetchSummary,
) -> Result<usize> {
    let body = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let split = ical::split_file(&body);
    ensure_whole_calendar(&body, &split)?;
    let calendar_id = calendar_id(root, file);
    let name = split.calendar_name.clone().or_else(|| {
        file.file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
    });
    db.upsert_calendars(&[CalendarRow {
        id: calendar_id.clone(),
        account_id: ACCOUNT_ID.into(),
        href: Some(relative(root, file)),
        display_name: name,
        description: None,
        color: None,
        time_zone: split.time_zone.clone(),
    }])
    .await?;
    summary.errors += split.events_without_uid;

    let existing = db.ics_uids(&calendar_id).await?;
    let rows: Vec<IcsObjectRow> = split
        .events
        .iter()
        .map(|e| IcsObjectRow::new(&calendar_id, &e.uid, None, None, &e.ics))
        .collect();
    for r in &rows {
        if existing.contains(&r.uid) {
            summary.events_updated += 1;
        } else {
            summary.events_new += 1;
        }
    }
    db.upsert_ics_objects(&rows).await?;
    let kept: std::collections::HashSet<&str> = rows.iter().map(|r| r.uid.as_str()).collect();
    let mut gone: Vec<String> = existing
        .iter()
        .filter(|u| !kept.contains(u.as_str()))
        .cloned()
        .collect();
    gone.sort();
    summary.events_deleted += gone.len();
    db.delete_ics_uids(&calendar_id, &gone).await?;
    Ok(split.events_without_uid)
}

/// A file stands for its whole calendar only when it holds a `VCALENDAR`
/// that closes, and an event it lists can be told apart from the rest.
/// A well-formed calendar with no events is an empty calendar.
fn ensure_whole_calendar(body: &str, split: &ical::SplitFile) -> Result<()> {
    let lines = ical::unfold(body);
    let marks = |mark: &str| {
        lines
            .iter()
            .filter(|l| l.trim().eq_ignore_ascii_case(mark))
            .count()
    };
    let (begun, ended) = (marks("BEGIN:VCALENDAR"), marks("END:VCALENDAR"));
    if begun == 0 {
        bail!("the file holds no calendar (BEGIN:VCALENDAR … END:VCALENDAR)");
    }
    if begun != ended {
        bail!("the file ends before its END:VCALENDAR: a copy cut off part-way");
    }
    if split.events.is_empty() && split.events_without_uid > 0 {
        bail!(
            "none of the file's {} events has a UID, so it cannot say which stored events it still holds",
            split.events_without_uid
        );
    }
    Ok(())
}

/// The file's path under the configured directory, without `.ics`:
/// `Bridge`, `2370/Away team`.
fn calendar_id(root: &Path, file: &Path) -> String {
    let rel = relative(root, file);
    rel.strip_suffix(".ics")
        .or_else(|| rel.strip_suffix(".ICS"))
        .unwrap_or(&rel)
        .to_string()
}

fn relative(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .ok()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty())
        .or_else(|| file.file_name().and_then(|s| s.to_str()))
        .unwrap_or("calendar.ics")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ics(uid: &str) -> String {
        format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
             DTSTART:23640101T090000Z\r\nSUMMARY:{uid}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        )
    }

    struct Env {
        input: tempfile::TempDir,
        input_path: PathBuf,
        _store: tempfile::TempDir,
        _cache_dir: tempfile::TempDir,
        db: RawDb,
        cache: FingerprintCache,
    }

    impl Env {
        async fn new() -> Self {
            let input = tempfile::tempdir().unwrap();
            let store = tempfile::tempdir().unwrap();
            let cache_dir = tempfile::tempdir().unwrap();
            let db = RawDb::open(&store.path().join("c.doltlite_db"))
                .await
                .unwrap();
            let cache = FingerprintCache::open(&cache_dir.path().join("fp.sqlite"))
                .await
                .unwrap();
            Self {
                input_path: input.path().to_path_buf(),
                input,
                _store: store,
                _cache_dir: cache_dir,
                db,
                cache,
            }
        }

        async fn fetch(&self) -> FetchSummary {
            fetch(FetchOptions {
                db: self.db.clone(),
                input_path: self.input_path.clone(),
                cache: self.cache.clone(),
                progress: Progress::default(),
                control: DownloadControl::default(),
            })
            .await
            .unwrap()
        }

        async fn calendars(&self) -> Vec<String> {
            sqlx::query_scalar("SELECT id FROM calendars ORDER BY id")
                .fetch_all(self.db.pool())
                .await
                .unwrap()
        }

        async fn problems(&self) -> Vec<(String, String)> {
            sqlx::query_as("SELECT scope_key, reason FROM problems ORDER BY scope_key")
                .fetch_all(self.db.pool())
                .await
                .unwrap()
        }

        async fn uids(&self) -> Vec<String> {
            sqlx::query_scalar("SELECT uid FROM ics_objects ORDER BY uid")
                .fetch_all(self.db.pool())
                .await
                .unwrap()
        }
    }

    /// #898: deleting a whole `.ics` file left its events in the store.
    #[tokio::test]
    async fn a_deleted_file_takes_its_calendar_with_it() {
        let e = Env::new().await;
        std::fs::write(e.input.path().join("Bridge.ics"), ics("red-alert")).unwrap();
        std::fs::write(e.input.path().join("Holodeck.ics"), ics("dixon-hill")).unwrap();
        e.fetch().await;
        assert_eq!(e.uids().await, vec!["dixon-hill", "red-alert"]);

        std::fs::remove_file(e.input.path().join("Holodeck.ics")).unwrap();
        let second = e.fetch().await;
        assert_eq!(second.events_deleted, 1);
        assert_eq!(second.files_removed, 1);
        assert_eq!(e.uids().await, vec!["red-alert"]);
        assert_eq!(e.calendars().await, vec!["Bridge"]);
        e.db.close().await;
    }

    /// Through a symlinked input, so the calendar is keyed against the
    /// resolved path the scan reports, not the spelling configured.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_moved_file_keeps_its_events_under_the_new_path() {
        let mut e = Env::new().await;
        let real = e.input.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = e.input.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        e.input_path = link;
        std::fs::write(real.join("Holodeck.ics"), ics("dixon-hill")).unwrap();
        e.fetch().await;

        std::fs::create_dir(real.join("Deck 11")).unwrap();
        std::fs::rename(real.join("Holodeck.ics"), real.join("Deck 11/Holodeck.ics")).unwrap();
        e.fetch().await;
        assert_eq!(e.uids().await, vec!["dixon-hill"]);
        assert_eq!(e.calendars().await, vec!["Deck 11/Holodeck"]);
        e.db.close().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_walk_error_deletes_nothing() {
        let e = Env::new().await;
        std::fs::write(e.input.path().join("Bridge.ics"), ics("red-alert")).unwrap();
        std::fs::write(e.input.path().join("Holodeck.ics"), ics("dixon-hill")).unwrap();
        e.fetch().await;

        std::fs::remove_file(e.input.path().join("Holodeck.ics")).unwrap();
        std::os::unix::fs::symlink(
            e.input.path().join("nowhere"),
            e.input.path().join("Dangling.ics"),
        )
        .unwrap();
        let second = e.fetch().await;
        assert_eq!(second.events_deleted, 0);
        assert_eq!(e.uids().await, vec!["dixon-hill", "red-alert"]);
        e.db.close().await;
    }

    /// Events without a UID are a row on their file, standing until the
    /// file is read again.
    #[tokio::test]
    async fn events_without_a_uid_are_a_row_on_their_file() {
        let e = Env::new().await;
        let path = e.input.path().join("Bridge.ics");
        let no_uid = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\n\
             DTSTART:23640101T090000Z\r\nSUMMARY:drill\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        std::fs::write(&path, format!("{}{no_uid}", ics("red-alert"))).unwrap();
        e.fetch().await;
        assert_eq!(
            e.problems().await,
            [(
                "file:calendar/ics:Bridge.ics".to_string(),
                "no_identity".to_string()
            )]
        );

        std::fs::write(&path, ics("red-alert")).unwrap();
        e.fetch().await;
        assert!(e.problems().await.is_empty());
        e.db.close().await;
    }

    /// A file that will not read is a row, and is read again next run.
    #[tokio::test]
    async fn a_file_that_will_not_read_is_a_row() {
        let e = Env::new().await;
        let path = e.input.path().join("Holodeck.ics");
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        e.fetch().await;
        assert_eq!(
            e.problems().await,
            [(
                "listing:ics Holodeck.ics".to_string(),
                "fetch_failed".to_string()
            )]
        );

        std::fs::write(&path, ics("dixon-hill")).unwrap();
        e.fetch().await;
        assert!(e.problems().await.is_empty());
        assert_eq!(e.uids().await, vec!["dixon-hill"]);
        e.db.close().await;
    }

    /// An `.ics` rewritten to nothing (0 bytes, text that is not a
    /// calendar, a copy cut off before `END:VCALENDAR`, events none of
    /// which has a UID) read as a calendar with no events, and every event
    /// was deleted. Only a whole `VCALENDAR` with no events empties it.
    #[tokio::test]
    async fn a_file_that_is_recognizably_nothing_deletes_nothing() {
        let e = Env::new().await;
        let path = e.input.path().join("Bridge.ics");
        let whole = ics("red-alert");
        std::fs::write(&path, &whole).unwrap();
        e.fetch().await;
        assert_eq!(e.uids().await, vec!["red-alert"]);

        let cut_off = whole[..whole.find("END:VCALENDAR").unwrap()].to_string();
        let no_uid = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\n\
             DTSTART:23640101T090000Z\r\nSUMMARY:drill\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
            .to_string();
        for (what, body) in [
            ("an empty file", String::new()),
            ("text that is not a calendar", "Red alert.\n".to_string()),
            ("a copy cut off before its end", cut_off),
            ("events none of which has a UID", no_uid),
        ] {
            std::fs::write(&path, &body).unwrap();
            let s = e.fetch().await;
            assert_eq!(s.events_deleted, 0, "{what} deleted events");
            assert_eq!(e.uids().await, vec!["red-alert"], "{what}");
            assert_eq!(
                e.problems().await,
                [(
                    "listing:ics Bridge.ics".to_string(),
                    "fetch_failed".to_string()
                )],
                "{what}"
            );
        }

        std::fs::write(
            &path,
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nX-WR-CALNAME:Bridge\r\nEND:VCALENDAR\r\n",
        )
        .unwrap();
        let emptied = e.fetch().await;
        assert_eq!(emptied.events_deleted, 1);
        assert!(e.uids().await.is_empty());
        assert!(e.problems().await.is_empty());
        e.db.close().await;
    }

    #[test]
    fn a_calendar_is_keyed_by_its_path_under_the_root() {
        let root = Path::new("/takeout/Calendar");
        assert_eq!(
            calendar_id(root, Path::new("/takeout/Calendar/Bridge.ics")),
            "Bridge"
        );
        assert_eq!(
            calendar_id(root, Path::new("/takeout/Calendar/2370/Away team.ics")),
            "2370/Away team"
        );
        // A single file configured as the path is its own root.
        assert_eq!(
            calendar_id(Path::new("/x/Bridge.ics"), Path::new("/x/Bridge.ics")),
            "Bridge"
        );
    }
}
