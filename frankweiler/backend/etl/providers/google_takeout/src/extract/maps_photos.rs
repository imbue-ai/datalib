//! `Maps/Photos and videos/*.json` + matching media file walker.
//!
//! Takeout pairs each photo with a JSON sidecar of the same stem
//! (`2026-06-04-af8bb6e0.jpg` ↔ `2026-06-04-af8bb6e0.json`). PK is
//! the file stem. Bytes land in `cas_objects` keyed by `blake3`; the
//! `maps_photos.blake3` column carries the hash so render can join
//! back without a separate edge table.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{blake3_hex, CasInsert};
use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::file_checkpoint::{self, FileFingerprint};
use frankweiler_etl::progress::Progress;
use frankweiler_time::IsoOffsetTimestamp;
use serde_json::Value;
use tracing::warn;

use super::db::RawDb;
use super::schema_raw::MapsPhotoRow;
use frankweiler_etl::doltlite_raw::WirePayload;

const DIR_REL: &str = "Maps/Photos and videos";
const SCOPE: &str = "google_takeout/maps_photos";

/// One pending CAS write: `(blake3, bytes, content_type)`. The
/// per-photo `ingest_one` produces zero or one of these; the
/// outer walker collects them into a `Vec` and hands borrows of
/// each tuple to [`CasInsert`] for the batched `put_many`.
type PendingCas = (String, Vec<u8>, Option<String>);

/// `(rows_upserted, blobs_stored)`.
pub async fn ingest(db: &RawDb, root: &Path, progress: &Progress) -> Result<(usize, usize)> {
    let dir = root.join(DIR_REL);
    if !dir.exists() {
        return Ok((0, 0));
    }
    let stamped = file_checkpoint::load(db.pool(), SCOPE).await?;
    let mut rows: Vec<MapsPhotoRow> = Vec::new();
    let mut cas_inserts_owned: Vec<PendingCas> = Vec::new();
    let mut fingerprints: Vec<(String, FileFingerprint)> = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let json_fp = FileFingerprint::of(&path)?;
        if file_checkpoint::should_skip(&stamped, &json_fp) {
            continue;
        }
        match ingest_one(&path) {
            Ok(Some((row, cas))) => {
                rows.push(row);
                if let Some(c) = cas {
                    cas_inserts_owned.push(c);
                }
                fingerprints.push((path.to_string_lossy().into_owned(), json_fp));
            }
            Ok(None) => {
                // Missing media file or malformed sidecar — fingerprint
                // still recorded so we don't loop on it forever.
                fingerprints.push((path.to_string_lossy().into_owned(), json_fp));
            }
            Err(e) => {
                warn!(event = "maps_photo_failed", path = %path.display(), error = %e);
            }
        }
    }
    let row_count = rows.len();
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
    let now = IsoOffsetTimestamp::now_local().to_rfc3339();
    let mut tx = db.pool().begin().await.context("begin maps_photos tx")?;
    bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
    for (_path, fp) in &fingerprints {
        file_checkpoint::record_finished(&mut tx, SCOPE, fp).await?;
    }
    tx.commit().await.context("commit maps_photos tx")?;
    Ok((row_count, blob_count))
}

fn ingest_one(json_path: &Path) -> Result<Option<(MapsPhotoRow, Option<PendingCas>)>> {
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
    let media_path = locate_media_sibling(json_path);
    let (blake3, cas) = match media_path.as_deref().and_then(|p| std::fs::read(p).ok()) {
        Some(bytes) => {
            let hash = blake3_hex(&bytes);
            let ct = content_type_for(media_path.as_deref().unwrap());
            (Some(hash.clone()), Some((hash, bytes, ct)))
        }
        None => (None, None),
    };
    let payload_str = serde_json::to_string(&payload).context("serialize photo payload")?;
    Ok(Some((
        MapsPhotoRow {
            id_and_payload: WirePayload {
                id: stem,
                payload: payload_str,
            },
            when_ts,
            blake3,
        },
        cas,
    )))
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
