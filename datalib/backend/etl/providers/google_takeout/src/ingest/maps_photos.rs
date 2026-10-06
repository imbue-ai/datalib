//! `Maps/Photos and videos/*.json` + matching media file walker.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::blob_cas::{blake3_hex, CasInsert};
use datalib_etl::bulk::{bulk_upsert_entity_in_tx, bulk_upsert_in_tx};
use datalib_etl::doltlite_raw::{record_object_error, record_object_skipped};
use datalib_etl::download_problems::SkippedRecord;
use datalib_etl::file_checkpoint;
use datalib_etl::fsscan;
use datalib_etl::progress::Progress;
use datalib_etl::prune;
use datalib_etl::run_problems::RunProblems;
use datalib_problems::{Problem, Reason};
use datalib_time::IsoOffsetTimestamp;
use serde_json::Value;

use super::db::RawDb;
use super::schema_raw::MapsPhotoRow;
use datalib_etl::doltlite_raw::WirePayload;

const DIR_REL: &str = "Maps/Photos and videos";
const SCOPE: &str = "google_takeout/maps_photos";
const TABLE: &str = "maps_photos";

/// One pending CAS write: `(blake3, bytes, content_type)`. The
/// per-photo `ingest_one` produces zero or one of these; the
/// outer walker collects them into a `Vec` and hands borrows of
/// each tuple to [`CasInsert`] for the batched `put_many`.
type PendingCas = (String, Vec<u8>, Option<String>);

#[derive(Debug, Default, Clone)]
pub struct MapsPhotosSummary {
    pub rows: usize,
    pub blobs: usize,
    /// Photos whose sidecar is gone.
    pub removed: usize,
    pub files_removed: usize,
}

/// A photo whose sidecar read but whose media did not: the row lands
/// without bytes and the record carries why.
enum MediaProblem {
    NotFound(String),
    Unreadable(String),
}

struct Photo {
    row: MapsPhotoRow,
    cas: Option<PendingCas>,
    media_problem: Option<MediaProblem>,
}

pub async fn ingest(
    db: &RawDb,
    scan: &fsscan::Scan,
    progress: &Progress,
    found: &RunProblems,
) -> Result<MapsPhotosSummary> {
    let prev = file_checkpoint::load_cursor(db.pool(), SCOPE).await?;
    let changes = scan.changes_since(&prev);

    let mut complete: Vec<MapsPhotoRow> = Vec::new();
    let mut without_media: Vec<(MapsPhotoRow, MediaProblem)> = Vec::new();
    let mut cas_inserts_owned: Vec<PendingCas> = Vec::new();
    let mut done: Vec<&fsscan::ScannedFile> = Vec::new();
    let mut skipped: Vec<SkippedRecord> = Vec::new();
    for f in changes
        .needs_reading_under(DIR_REL)
        .filter(|f| f.path.extension().and_then(|s| s.to_str()) == Some("json"))
    {
        match ingest_one(&f.path) {
            Ok(Some(photo)) => {
                if let Some(c) = photo.cas {
                    cas_inserts_owned.push(c);
                }
                // A sidecar whose media did not read stays unstamped, so the
                // next run looks for the media again.
                match photo.media_problem {
                    None => {
                        complete.push(photo.row);
                        done.push(f);
                    }
                    Some(problem) => without_media.push((photo.row, problem)),
                }
            }
            Ok(None) => done.push(f),
            // Not stamped, so it is read again next run, and its row is
            // re-reported until it reads.
            Err(e) => skipped.push(SkippedRecord {
                entry: f.rel.clone(),
                problem: Problem::record(
                    Reason::FetchFailed,
                    &format!("{}: {}", f.rel, e.root_cause()),
                ),
            }),
        }
    }

    let row_count = complete.len() + without_media.len();
    let blob_count = cas_inserts_owned.len();
    progress.set_message(&format!(
        "maps_photos: {row_count} rows, {blob_count} blobs"
    ));

    if !cas_inserts_owned.is_empty() {
        let cas: Vec<CasInsert<'_>> = cas_inserts_owned
            .iter()
            .map(|(b, bytes, ct)| CasInsert {
                blake3: b.as_str(),
                bytes: bytes.as_slice(),
                content_type: ct.as_deref(),
            })
            .collect();
        db.cas().put_many(&cas).await?;
    }
    let now = IsoOffsetTimestamp::now_local();
    let mut tx = db.pool().begin().await.context("begin maps_photos tx")?;
    bulk_upsert_in_tx(&mut tx, &complete, &now).await?;
    // Without the bookkeeping a complete row gets: the attempt below is
    // its stamp, and it keeps when the problem was first seen.
    let rows: Vec<MapsPhotoRow> = without_media.iter().map(|(r, _)| r.clone()).collect();
    bulk_upsert_entity_in_tx(&mut tx, &rows).await?;
    for (row, problem) in &without_media {
        let id = &row.id_and_payload.id;
        match problem {
            MediaProblem::NotFound(detail) => {
                record_object_skipped(&mut tx, TABLE, id, Reason::NotFound, detail).await?
            }
            MediaProblem::Unreadable(detail) => {
                record_object_error(&mut tx, TABLE, id, detail).await?
            }
        }
    }
    for f in &done {
        file_checkpoint::record_file(&mut tx, SCOPE, f).await?;
    }
    tx.commit().await.context("commit maps_photos tx")?;
    found.skipped("maps_photos", skipped);

    // A photo row is keyed by its sidecar's stem, so a row whose stem no
    // sidecar here carries is gone — read as "not exported" when the
    // folder is missing, and as nothing when the walk could not see it all.
    let removed = if super::product_exported(scan, DIR_REL) && changes.walk_errors == 0 {
        let present: HashSet<String> = scan
            .files
            .iter()
            .filter(|f| is_sidecar(&f.rel))
            .filter_map(|f| stem(&f.rel))
            .map(str::to_string)
            .collect();
        prune::prune_scope(db.pool(), TABLE, &[], &present)
            .await?
            .len()
    } else {
        0
    };
    let gone = changes.gone();
    file_checkpoint::forget_files(db.pool(), SCOPE, &gone).await?;
    Ok(MapsPhotosSummary {
        rows: row_count,
        blobs: blob_count,
        removed,
        files_removed: gone.len(),
    })
}

fn is_sidecar(rel: &str) -> bool {
    fsscan::is_under(rel, DIR_REL) && rel.ends_with(".json")
}

fn stem(rel: &str) -> Option<&str> {
    Path::new(rel).file_stem()?.to_str()
}

fn ingest_one(json_path: &Path) -> Result<Option<Photo>> {
    let bytes =
        std::fs::read(json_path).with_context(|| format!("read {}", json_path.display()))?;
    let payload: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", json_path.display()))?;
    let stem = json_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    if stem.is_empty() {
        return Ok(None);
    }
    let when_ts = payload
        .get("creationTime")
        .and_then(|v| v.get("timestamp"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let (blake3, cas, media_problem) = match locate_media_sibling(json_path) {
        None => (
            None,
            None,
            Some(MediaProblem::NotFound(format!(
                "no photo or video beside {stem}.json in the export"
            ))),
        ),
        Some(media_path) => match std::fs::read(&media_path) {
            Ok(bytes) => {
                let hash = blake3_hex(&bytes);
                let ct = content_type_for(&media_path);
                (Some(hash.clone()), Some((hash, bytes, ct)), None)
            }
            Err(e) => (
                None,
                None,
                Some(MediaProblem::Unreadable(format!(
                    "read {}: {e}",
                    media_path.display()
                ))),
            ),
        },
    };
    let payload_str = serde_json::to_string(&payload).context("serialize photo payload")?;
    Ok(Some(Photo {
        row: MapsPhotoRow {
            id_and_payload: WirePayload {
                id: stem,
                payload: payload_str,
            },
            when_ts,
            blake3,
        },
        cas,
        media_problem,
    }))
}

fn locate_media_sibling(json_path: &Path) -> Option<PathBuf> {
    let parent = json_path.parent()?;
    let stem = json_path.file_stem()?.to_str()?;
    for ext in ["jpg", "jpeg", "png", "heic", "mp4", "mov", "webp"] {
        let cand = parent.join(format!("{stem}.{ext}"));
        if cand.exists() {
            return Some(cand);
        }
    }
    None
}

fn content_type_for(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let ct = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "heic" => "image/heic",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        _ => return None,
    };
    Some(ct.to_string())
}
