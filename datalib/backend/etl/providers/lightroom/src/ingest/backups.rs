//! A folder of Lightroom backups: which entries are backups, and which
//! the store does not hold yet. Pure over an `fsscan` of the folder and
//! the ledger; `sync` acts on it.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use chrono::{NaiveDateTime, TimeZone};
use sqlx::sqlite::SqlitePool;

use datalib_etl::fsscan::{self, ScannedFile};

use super::unpack::{is_catalog, is_zip};

/// The store's record of the backups it holds, one row per backup,
/// written in the commit that mirrored it.
pub const LEDGER: &str = "lightroom_snapshots";

pub const LEDGER_DDL: &str = "CREATE TABLE IF NOT EXISTS lightroom_snapshots (
    snapshot TEXT PRIMARY KEY,
    taken_at TEXT NOT NULL,
    file TEXT NOT NULL,
    blake3 TEXT NOT NULL
)";

/// How Lightroom names a backup's folder: `2026-09-27 1650`, sometimes
/// with a note a person added after it.
const NAME_DATE_FORMAT: &str = "%Y-%m-%d %H%M";
const NAME_DATE_LEN: usize = "2026-09-27 1650".len();

/// `taken_at` as the ledger stores it.
pub const LEDGER_DATE_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

/// One entry directly under the backups folder, with the catalog files
/// found in it: a folder, or a catalog file sitting there on its own.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub files: Vec<ScannedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backup {
    /// The entry's name, which is also its key in [`LEDGER`].
    pub name: String,
    pub taken_at: NaiveDateTime,
    /// What to mirror: the `.zip` Lightroom wrote, else a bare `.lrcat`.
    pub file: ScannedFile,
}

/// A ledger row, as the plan needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub snapshot: String,
    pub taken_at: NaiveDateTime,
    pub blake3: String,
}

/// What a run will do, decided from the folder and the ledger alone.
#[derive(Debug, Default)]
pub struct Plan {
    /// Every backup found, oldest first, whether or not the store has it.
    pub found: Vec<Backup>,
    /// The ones to mirror now, oldest first.
    pub ingest: Vec<Backup>,
    /// `(entry name, why it cannot be mirrored)`.
    pub refused: Vec<(String, String)>,
}

/// Group a scan of the folder by its top-level entry. Hidden entries
/// (`.DS_Store`, `.Trashes`) are nobody's backup.
pub fn entries(files: &[ScannedFile]) -> Vec<Entry> {
    let mut by_name: BTreeMap<&str, Vec<ScannedFile>> = BTreeMap::new();
    for f in files {
        let top = f.rel.split('/').next().unwrap_or(&f.rel);
        if top.starts_with('.') {
            continue;
        }
        by_name.entry(top).or_default().push(f.clone());
    }
    by_name
        .into_iter()
        .map(|(name, files)| Entry {
            name: name.to_string(),
            files,
        })
        .collect()
}

pub fn plan(entries: Vec<Entry>, ledger: &[Held]) -> Plan {
    let mut out = Plan::default();
    for entry in entries {
        match read_entry(&entry) {
            Ok(Some(b)) => out.found.push(b),
            Ok(None) => {}
            Err(why) => out.refused.push((entry.name, why)),
        }
    }
    out.found
        .sort_by(|a, b| (a.taken_at, &a.name).cmp(&(b.taken_at, &b.name)));

    // Known by content, not by name: a folder renamed by hand is the
    // backup the store already has, and a file rewritten since it was
    // committed is a backup it does not.
    out.ingest = out
        .found
        .iter()
        .filter(|b| {
            let hash = fsscan::hex(&b.file.blake3);
            !ledger.iter().any(|h| h.blake3 == hash)
        })
        .cloned()
        .collect();
    out
}

pub fn newest(ledger: &[Held]) -> Option<&Held> {
    ledger.iter().max_by_key(|h| h.taken_at)
}

/// `Ok(None)` for an entry with no catalog in it, which is not a backup.
fn read_entry(entry: &Entry) -> Result<Option<Backup>, String> {
    let path = |f: &&ScannedFile| f.path.clone();
    let zips: Vec<&ScannedFile> = entry.files.iter().filter(|f| is_zip(&path(f))).collect();
    let catalogs: Vec<&ScannedFile> = entry
        .files
        .iter()
        .filter(|f| is_catalog(&path(f)))
        .collect();
    // A folder that has both is one Lightroom wrote and someone unpacked
    // since. The zip is what Lightroom wrote; the unpacked copy may have
    // been opened, and so changed, after.
    let file = match (zips.as_slice(), catalogs.as_slice()) {
        ([zip], _) => (*zip).clone(),
        ([], [catalog]) => (*catalog).clone(),
        ([], []) => return Ok(None),
        ([], many) | (many, _) => {
            return Err(format!(
                "holds {} catalogs; a backup folder holds one",
                many.len()
            ))
        }
    };
    let taken_at = taken_at(&entry.name).ok_or_else(|| {
        "its name does not start with the date and time Lightroom names a backup by, \
         like `2026-09-27 1650`"
            .to_string()
    })?;
    Ok(Some(Backup {
        name: entry.name.clone(),
        taken_at,
        file,
    }))
}

fn taken_at(name: &str) -> Option<NaiveDateTime> {
    let head = name.get(..NAME_DATE_LEN)?;
    NaiveDateTime::parse_from_str(head, NAME_DATE_FORMAT).ok()
}

/// The commit date for a backup: its folder's time, read in this
/// machine's time zone, which is the one Lightroom named it in.
pub fn commit_date(taken_at: NaiveDateTime) -> Option<String> {
    let date = chrono::Local
        .from_local_datetime(&taken_at)
        .earliest()
        .map(|t| t.to_rfc3339());
    if date.is_none() {
        tracing::warn!(
            %taken_at,
            "lightroom: a backup's time falls in a daylight-saving gap; committing it dated now"
        );
    }
    date
}

pub async fn read_ledger(pool: &SqlitePool) -> Result<Vec<Held>> {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT snapshot, taken_at, blake3 FROM lightroom_snapshots")
            .fetch_all(pool)
            .await
            .context("read the snapshots ledger")?;
    rows.into_iter()
        .map(|(snapshot, t, blake3)| {
            let taken_at = NaiveDateTime::parse_from_str(&t, LEDGER_DATE_FORMAT)
                .with_context(|| format!("ledger row {snapshot:?} has taken_at {t:?}"))?;
            Ok(Held {
                snapshot,
                taken_at,
                blake3,
            })
        })
        .collect()
}

pub async fn record(
    pool: &SqlitePool,
    snapshot: &str,
    taken_at: NaiveDateTime,
    file: &str,
    blake3: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO lightroom_snapshots (snapshot, taken_at, file, blake3) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(snapshot)
    .bind(taken_at.format(LEDGER_DATE_FORMAT).to_string())
    .bind(file)
    .bind(blake3)
    .execute(pool)
    .await
    .with_context(|| format!("record {snapshot} in {LEDGER}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, LEDGER_DATE_FORMAT).unwrap()
    }

    /// A stand-in digest: equal for equal `content`, distinct for the
    /// short strings these tests use.
    fn digest(content: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (i, b) in content.bytes().enumerate() {
            out[i % 32] = out[i % 32].wrapping_mul(31).wrapping_add(b);
        }
        out[31] = content.len() as u8;
        out
    }

    /// A scanned file whose content is named by `content`, so two files
    /// with the same `content` hash the same.
    fn file(rel: &str, content: &str) -> ScannedFile {
        ScannedFile {
            path: PathBuf::from(format!("/B/{rel}")),
            rel: rel.into(),
            size: 1,
            blake3: digest(content),
        }
    }

    fn hash(content: &str) -> String {
        fsscan::hex(&digest(content))
    }

    fn held(snapshot: &str, t: &str, content: &str) -> Held {
        Held {
            snapshot: snapshot.into(),
            taken_at: at(t),
            blake3: hash(content),
        }
    }

    fn names(bs: &[Backup]) -> Vec<&str> {
        bs.iter().map(|b| b.name.as_str()).collect()
    }

    fn refused(p: &Plan) -> Vec<&str> {
        p.refused.iter().map(|(n, _)| n.as_str()).collect()
    }

    /// The layout of a real backups folder, which a person has partly
    /// unpacked and annotated by hand, as `fsscan` reports it: catalog
    /// files only, `-wal`/`-shm` and `.DS_Store` filtered out.
    fn lightroom_folder() -> Vec<ScannedFile> {
        vec![
            file("2026-09-02 0911/Lightroom Catalog-v13-3.zip", "v13"),
            file(
                "2018-03-07 2110/Lightroom Catalog-2-3.lrcat",
                "2018 unpacked",
            ),
            file("2018-03-07 2110/Lightroom Catalog-2-3.lrcat.zip", "2018"),
            file(
                "2019-12-14 0731 - Before restoring captions/Lightroom Catalog-2-3.lrcat.zip",
                "2019",
            ),
            file("2016-10-01 0856/Lightroom Catalog.lrcat", "2016"),
        ]
    }

    fn plan_of(files: Vec<ScannedFile>, ledger: &[Held]) -> Plan {
        plan(entries(&files), ledger)
    }

    #[test]
    fn backups_are_mirrored_in_the_order_they_were_taken() {
        let p = plan_of(lightroom_folder(), &[]);
        assert_eq!(
            names(&p.ingest),
            [
                "2016-10-01 0856",
                "2018-03-07 2110",
                "2019-12-14 0731 - Before restoring captions",
                "2026-09-02 0911",
            ]
        );
        assert!(p.refused.is_empty(), "{:?}", p.refused);
        assert_eq!(p.ingest[0].taken_at, at("2016-10-01T08:56:00"));
    }

    #[test]
    fn the_zip_lightroom_wrote_beats_an_unpacked_copy_beside_it() {
        let p = plan_of(lightroom_folder(), &[]);
        let b = p
            .ingest
            .iter()
            .find(|b| b.name == "2018-03-07 2110")
            .unwrap();
        assert!(b.file.rel.ends_with("Lightroom Catalog-2-3.lrcat.zip"));
        let bare = p
            .ingest
            .iter()
            .find(|b| b.name == "2016-10-01 0856")
            .unwrap();
        assert!(bare.file.rel.ends_with("Lightroom Catalog.lrcat"));
    }

    #[test]
    fn backups_the_store_holds_are_skipped() {
        let ledger = [
            held("2016-10-01 0856", "2016-10-01T08:56:00", "2016"),
            held("2018-03-07 2110", "2018-03-07T21:10:00", "2018"),
        ];
        let p = plan_of(lightroom_folder(), &ledger);
        assert_eq!(p.found.len(), 4);
        assert_eq!(
            names(&p.ingest),
            [
                "2019-12-14 0731 - Before restoring captions",
                "2026-09-02 0911"
            ]
        );
    }

    /// A backup is known by its bytes: renaming its folder by hand, as
    /// someone adding a note does, does not make it a new backup.
    #[test]
    fn a_renamed_backup_folder_is_the_backup_already_held() {
        let ledger = [held("2019-12-14 0731", "2019-12-14T07:31:00", "2019")];
        let p = plan_of(lightroom_folder(), &ledger);
        let renamed = "2019-12-14 0731 - Before restoring captions";
        assert!(!names(&p.ingest).contains(&renamed));
        assert!(p.refused.is_empty(), "{:?}", p.refused);
    }

    /// A backup whose file changed after it was committed is new bytes,
    /// so it is replayed like any backup the store does not hold.
    #[test]
    fn a_backup_changed_since_it_was_committed_is_replayed() {
        let ledger = [held(
            "2016-10-01 0856",
            "2016-10-01T08:56:00",
            "2016, before",
        )];
        let p = plan_of(lightroom_folder(), &ledger);
        assert!(names(&p.ingest).contains(&"2016-10-01 0856"));
        assert!(p.refused.is_empty(), "{:?}", p.refused);
    }

    /// A backup that turns up after a newer one was committed is still
    /// replayed, in date order among the new ones; `sync` then puts the
    /// newest state back on top.
    #[test]
    fn a_backup_older_than_the_newest_held_is_replayed() {
        let ledger = [held("2019-01-28 1032", "2019-01-28T10:32:00", "2019-01")];
        let p = plan_of(lightroom_folder(), &ledger);
        assert_eq!(p.ingest.len(), 4, "{:?}", names(&p.ingest));
        assert_eq!(p.ingest[0].name, "2016-10-01 0856");
        assert!(p.refused.is_empty(), "{:?}", p.refused);
    }

    #[test]
    fn a_folder_that_cannot_be_placed_is_refused_and_one_with_no_catalog_ignored() {
        let p = plan_of(
            vec![
                file("Old catalogs/Lightroom Catalog.lrcat.zip", "old"),
                file("2020-01-01 0000/a.zip", "a"),
                file("2020-01-01 0000/b.zip", "b"),
                file("2021-05-06 0700 Catalog.zip", "loose"),
                file(".Trashes/2022-01-01 0000/x.zip", "trash"),
            ],
            &[],
        );
        assert_eq!(names(&p.ingest), ["2021-05-06 0700 Catalog.zip"]);
        assert_eq!(refused(&p), ["2020-01-01 0000", "Old catalogs"]);
    }

    #[test]
    fn a_commit_date_carries_the_local_offset() {
        let d = commit_date(at("2016-10-01T08:56:00")).unwrap();
        assert!(d.starts_with("2016-10-01T08:56:00"), "{d}");
        assert!(d.len() > "2016-10-01T08:56:00".len(), "no offset in {d}");
    }
}
