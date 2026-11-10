//! `datalib-lightroom-retime`: applies a file of capture-time edits to a
//! Lightroom Classic catalog, with Lightroom closed (README.md). `plan`
//! decides what each edit writes; this file reads the catalog, backs it
//! up and writes.

pub mod edits;
pub mod plan;
pub mod xmp;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sqlx::sqlite::SqliteConnection;
use sqlx::{Connection, Row as _};

use edits::Edit;
use plan::{Outcome, Photo, Write};

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub edit: Edit,
    pub name: Option<String>,
    pub was: Option<String>,
    pub outcome: Outcome,
}

#[derive(Debug)]
pub struct Report {
    pub rows: Vec<Row>,
    pub written: Option<Written>,
}

#[derive(Debug)]
pub struct Written {
    pub backup: PathBuf,
    pub cache_removed: Option<PathBuf>,
}

impl Report {
    pub fn refused(&self) -> usize {
        self.count(|o| matches!(o, Outcome::Refused(_)))
    }

    pub fn to_write(&self) -> usize {
        self.count(|o| matches!(o, Outcome::Write(_)))
    }

    pub fn already_done(&self) -> usize {
        self.count(|o| matches!(o, Outcome::AlreadyDone))
    }

    fn count(&self, f: impl Fn(&Outcome) -> bool) -> usize {
        self.rows.iter().filter(|r| f(&r.outcome)).count()
    }
}

/// Lightroom holds `<catalog>.lock` while it has the catalog open.
pub fn lock_file(catalog: &Path) -> PathBuf {
    with_suffix(catalog, ".lock")
}

/// The cache the Metadata panel reads, which an edit made outside
/// Lightroom does not refresh; Lightroom rebuilds it when it is gone.
pub fn panel_cache(catalog: &Path) -> PathBuf {
    let stem = catalog.file_stem().unwrap_or_default().to_string_lossy();
    catalog.with_file_name(format!("{stem} Helper.lrdata"))
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Plans every edit and, with `write`, applies them: all of them or, if
/// any is refused, none. `stamp` names the backup.
pub async fn run(catalog: &Path, edits: Vec<Edit>, write: bool, stamp: &str) -> Result<Report> {
    refuse_if_open(catalog)?;
    let mut conn = open(catalog).await?;
    let planned = plan_all(&mut conn, edits.clone()).await;
    close(conn).await?;
    let report = Report {
        rows: planned?,
        written: None,
    };
    if !write || report.refused() > 0 || report.to_write() == 0 {
        return Ok(report);
    }
    let backup = back_up(catalog, stamp)?;
    let mut conn = open(catalog).await?;
    let applied = apply(&mut conn, edits, &report.rows).await;
    close(conn).await?;
    applied?;
    let cache = panel_cache(catalog);
    let cache_removed = if cache.exists() {
        fs::remove_dir_all(&cache).with_context(|| format!("removing {}", cache.display()))?;
        Some(cache)
    } else {
        None
    };
    Ok(Report {
        written: Some(Written {
            backup,
            cache_removed,
        }),
        ..report
    })
}

// Read-write even to plan: a read-only connection to a catalog in WAL
// mode leaves its `-wal` and `-shm` beside it. Foreign keys off, as
// Lightroom keeps them.
async fn open(catalog: &Path) -> Result<SqliteConnection> {
    let options = datalib_sqlite_open::file(catalog).foreign_keys(false);
    SqliteConnection::connect_with(&options)
        .await
        .with_context(|| format!("opening {}", catalog.display()))
}

// Closing the last connection folds the write-ahead log back into the
// catalog and removes it, so Lightroom finds the file whole.
async fn close(conn: SqliteConnection) -> Result<()> {
    conn.close().await?;
    Ok(())
}

async fn apply(conn: &mut SqliteConnection, edits: Vec<Edit>, planned: &[Row]) -> Result<()> {
    let mut tx = conn.begin_with("BEGIN IMMEDIATE").await?;
    let again = plan_all(&mut tx, edits).await?;
    if again != planned {
        bail!("the catalog changed while the edits were being planned; nothing written");
    }
    for row in &again {
        if let Outcome::Write(w) = &row.outcome {
            write_one(&mut tx, w)
                .await
                .with_context(|| format!("line {}", row.edit.line))?;
        }
    }
    tx.commit().await?;
    Ok(())
}

fn refuse_if_open(catalog: &Path) -> Result<()> {
    if lock_file(catalog).exists() {
        bail!(
            "Lightroom has {} open (its .lock file is there); quit Lightroom first",
            catalog.display()
        );
    }
    Ok(())
}

fn back_up(catalog: &Path, stamp: &str) -> Result<PathBuf> {
    let mut backup = with_suffix(catalog, &format!(".before-retime-{stamp}"));
    for n in 2.. {
        if !backup.exists() {
            break;
        }
        backup = with_suffix(catalog, &format!(".before-retime-{stamp}-{n}"));
    }
    fs::copy(catalog, &backup).with_context(|| format!("backing up to {}", backup.display()))?;
    // A write-ahead log left by a Lightroom that did not quit cleanly
    // holds part of the catalog.
    let wal = with_suffix(catalog, "-wal");
    if wal.exists() {
        fs::copy(&wal, with_suffix(&backup, "-wal"))?;
    }
    Ok(backup)
}

async fn plan_all(conn: &mut SqliteConnection, edits: Vec<Edit>) -> Result<Vec<Row>> {
    let ids: Vec<&str> = edits.iter().map(|e| e.id_global.as_str()).collect();
    let mut photos = read_photos(conn, &ids).await?;
    Ok(edits
        .into_iter()
        .map(|edit| {
            let rows = photos.remove(&edit.id_global).unwrap_or_default();
            let first = rows.first();
            Row {
                name: first.map(|p| p.name.clone()),
                was: first.and_then(|p| p.capture_time.clone()),
                outcome: plan::plan(&edit, &rows),
                edit,
            }
        })
        .collect())
}

async fn read_photos(
    conn: &mut SqliteConnection,
    ids: &[&str],
) -> Result<HashMap<String, Vec<Photo>>> {
    // Lightroom's columns are untyped, so each is read as what it holds.
    let rows = sqlx::query(
        "SELECT i.id_global, i.id_local, coalesce(f.baseName || '.' || f.extension, ''),
                CAST(i.captureTime AS TEXT), CAST(i.originalCaptureTime AS TEXT),
                CAST(m.xmp AS BLOB),
                (SELECT count(*) FROM AgHarvestedExifMetadata e WHERE e.image = i.id_local)
         FROM Adobe_images i
         LEFT JOIN AgLibraryFile f ON f.id_local = i.rootFile
         LEFT JOIN Adobe_AdditionalMetadata m ON m.image = i.id_local
         WHERE i.id_global IN (SELECT value FROM json_each(?))",
    )
    .bind(serde_json::to_string(ids)?)
    .fetch_all(conn)
    .await?;
    let mut photos: HashMap<String, Vec<Photo>> = HashMap::new();
    for row in rows {
        let exif_rows: i64 = row.try_get(6)?;
        photos.entry(row.try_get(0)?).or_default().push(Photo {
            image: row.try_get(1)?,
            name: row.try_get(2)?,
            capture_time: row.try_get(3)?,
            original_capture_time: row.try_get(4)?,
            xmp: row.try_get(5)?,
            exif_rows: exif_rows as usize,
        });
    }
    Ok(photos)
}

async fn write_one(conn: &mut SqliteConnection, w: &Write) -> Result<()> {
    let changed = match &w.original_capture_time {
        Some(original) => sqlx::query(
            "UPDATE Adobe_images SET captureTime = ?, originalCaptureTime = ? WHERE id_local = ?",
        )
        .bind(&w.capture_time)
        .bind(original)
        .bind(w.image),
        None => sqlx::query("UPDATE Adobe_images SET captureTime = ? WHERE id_local = ?")
            .bind(&w.capture_time)
            .bind(w.image),
    }
    .execute(&mut *conn)
    .await?
    .rows_affected();
    expect_one("Adobe_images", changed)?;
    let changed = sqlx::query("UPDATE Adobe_AdditionalMetadata SET xmp = ? WHERE image = ?")
        .bind(&w.xmp)
        .bind(w.image)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    expect_one("Adobe_AdditionalMetadata", changed)?;
    // Lightroom keeps these as reals (2024.0).
    let changed = sqlx::query(
        "UPDATE AgHarvestedExifMetadata SET dateYear = ?, dateMonth = ?, dateDay = ? WHERE image = ?",
    )
    .bind(w.year)
    .bind(w.month)
    .bind(w.day)
    .bind(w.image)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if changed == 0 {
        bail!("no AgHarvestedExifMetadata row changed");
    }
    Ok(())
}

fn expect_one(table: &str, changed: u64) -> Result<()> {
    if changed != 1 {
        bail!("{changed} rows of {table} changed, not one");
    }
    Ok(())
}
