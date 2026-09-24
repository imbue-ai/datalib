//! A directory of `.ics` files: a Google Takeout `Calendar/` folder, an
//! Apple Calendar export. Each file is one calendar and the whole of
//! it, so an event a re-read file no longer carries was deleted.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::control::DownloadControl;
use datalib_etl::file_checkpoint;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::fsscan;
use datalib_etl::progress::Progress;
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
    for f in changes.needs_reading() {
        if opts.control.stop.requested() {
            break;
        }
        opts.progress
            .set_message(&format!("reading {}", f.path.display()));
        match ingest_one(db, &opts.input_path, &f.path, &mut summary).await {
            // Stamped only after a clean read, so a crash mid-file leaves
            // no cursor and the next run reads it again.
            Ok(()) => file_checkpoint::record_file_pool(db.pool(), CHECKPOINT_SCOPE, f).await?,
            Err(e) => {
                summary.errors += 1;
                warn!(event = "calendar_ics_ingest_failed", path = %f.path.display(), error = %format!("{e:#}"), "an ics file could not be read");
            }
        }
        opts.progress.inc(1);
    }
    Ok(summary)
}

async fn ingest_one(
    db: &RawDb,
    root: &Path,
    file: &Path,
    summary: &mut FetchSummary,
) -> Result<()> {
    let body = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let split = ical::split_file(&body);
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
    if split.events_without_uid > 0 {
        summary.errors += split.events_without_uid;
        warn!(event = "calendar_ics_event_without_uid", path = %file.display(), n = split.events_without_uid, "events with no UID cannot be told apart from one run to the next; skipped them");
    }

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
    db.delete_ics_uids(&calendar_id, &gone).await
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
