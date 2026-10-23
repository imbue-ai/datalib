//! Doltlite-backed raw store for the `google_drive` provider, and the
//! writes a capture and a revision export make.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

use datalib_etl::blob_cas::{BlobCas, CasInsert};
use datalib_etl::bulk::{bulk_upsert_in_tx, BulkUpsertable};
use datalib_etl::doltlite_raw as dr;
use datalib_time::IsoOffsetTimestamp;

use super::schema_raw::{
    full_ddl, CapturePartRow, CaptureRow, CommentRow, FileRow, RevisionExportRow, RevisionRow,
    CAPTURES, COMMENTS, FILES, LADDER, REVISIONS,
};

pub use datalib_etl::doltlite_raw::db_path_for;

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl(), LADDER);

/// A row and the volatile half split off its payload.
#[derive(Debug, Clone)]
pub struct Split<R> {
    pub row: R,
    pub volatile: Option<Value>,
}

/// One part of a capture.
#[derive(Debug, Clone)]
pub struct Part {
    pub name: String,
    pub body: Body,
    pub content_type: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Body {
    Fetched(Vec<u8>),
    /// Bytes the CAS already holds, under this hash.
    Held(String),
    /// An export Google declined to make.
    Declined,
}

/// Everything one version of one file came to.
#[derive(Debug, Clone)]
pub struct Capture {
    pub file: Split<FileRow>,
    pub revisions: Vec<Split<RevisionRow>>,
    pub comments: Vec<Split<CommentRow>>,
    pub row: Split<CaptureRow>,
    pub parts: Vec<Part>,
}

impl Capture {
    pub fn bytes(&self) -> usize {
        self.parts
            .iter()
            .map(|p| match &p.body {
                Body::Fetched(bytes) => bytes.len(),
                Body::Held(_) | Body::Declined => 0,
            })
            .sum()
    }
}

/// The image parts of every stored capture of some files, by file and by
/// the revision its native JSON named: a capture whose native JSON names
/// the same revision points at the same images.
#[derive(Debug, Default)]
pub struct HeldImages(HashMap<(String, String), HashMap<String, String>>);

impl HeldImages {
    /// The hash of `part` as a capture of `file_id` at `revision` holds it.
    pub fn get(&self, file_id: &str, revision: &str, part: &str) -> Option<&str> {
        self.0
            .get(&(file_id.to_string(), revision.to_string()))?
            .get(part)
            .map(String::as_str)
    }
}

impl RawDb {
    pub async fn held_images(&self, file_ids: &[&str]) -> Result<HeldImages> {
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT c.file_id, c.native_revision_id, p.part, p.blake3 \
             FROM captures c JOIN capture_parts p ON p.capture_id = c.id \
             WHERE c.file_id IN (SELECT value FROM json_each(?)) \
               AND c.native_revision_id IS NOT NULL \
               AND p.part LIKE 'image:%' AND p.blake3 IS NOT NULL",
        )
        .bind(serde_json::to_string(file_ids)?)
        .fetch_all(self.pool())
        .await
        .context("read the stored captures' images")?;
        let mut held = HeldImages::default();
        for (file_id, revision, part, blake3) in rows {
            held.0
                .entry((file_id, revision))
                .or_default()
                .insert(part, blake3);
        }
        Ok(held)
    }
}

/// Write `captures` in `tx`: their bytes into the CAS first, in one
/// `put_many` that commits on its own, then every row naming them.
pub async fn store_captures(
    tx: &mut Transaction<'static, Sqlite>,
    cas: &BlobCas,
    captures: &[&Capture],
) -> Result<()> {
    let edge_id = |c: &Capture, p: &Part| format!("{}#{}", c.row.row.id, p.name);
    let inserts: Vec<CasInsert<'_, String>> = captures
        .iter()
        .flat_map(|c| c.parts.iter().map(move |p| (c, p)))
        .filter_map(|(c, p)| match &p.body {
            Body::Fetched(bytes) => Some(CasInsert {
                id: edge_id(c, p),
                bytes,
                content_type: p.content_type.as_deref(),
            }),
            Body::Held(_) | Body::Declined => None,
        })
        .collect();
    let stored = cas.put_many(inserts).await?;
    let edges: Vec<CapturePartRow> = captures
        .iter()
        .flat_map(|c| c.parts.iter().map(move |p| (c, p)))
        .map(|(c, p)| {
            let id = edge_id(c, p);
            CapturePartRow {
                blake3: match &p.body {
                    Body::Fetched(_) => stored.get(&id).cloned(),
                    Body::Held(blake3) => Some(blake3.clone()),
                    Body::Declined => None,
                },
                id,
                capture_id: c.row.row.id.clone(),
                part: p.name.clone(),
            }
        })
        .collect();

    let now = IsoOffsetTimestamp::now_local();
    let files: Vec<&Split<FileRow>> = captures.iter().map(|c| &c.file).collect();
    upsert_split(tx, FILES, &files, &now).await?;
    let revisions: Vec<&Split<RevisionRow>> = captures.iter().flat_map(|c| &c.revisions).collect();
    upsert_split(tx, REVISIONS, &revisions, &now).await?;
    let comments: Vec<&Split<CommentRow>> = captures.iter().flat_map(|c| &c.comments).collect();
    upsert_split(tx, COMMENTS, &comments, &now).await?;
    let rows: Vec<&Split<CaptureRow>> = captures.iter().map(|c| &c.row).collect();
    upsert_split(tx, CAPTURES, &rows, &now).await?;
    bulk_upsert_in_tx(tx, &edges, &now).await
}

/// A stored revision, with the `exportLinks` Drive gave for it (JSON
/// `null` when it gave none).
#[derive(Debug, Clone)]
pub struct StoredRevision {
    pub id: String,
    pub file_id: String,
    pub export_links: Value,
}

impl RawDb {
    /// Every stored revision of every file whose listing entry has
    /// `mime_type`, oldest file id first.
    pub async fn revisions_of_kind(&self, mime_type: &str) -> Result<Vec<StoredRevision>> {
        let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT r.id, r.file_id, json_extract(r.payload, '$.exportLinks') \
             FROM revisions r JOIN files f ON f.id = r.file_id \
             WHERE json_extract(f.payload, '$.mimeType') = ? ORDER BY r.id",
        )
        .bind(mime_type)
        .fetch_all(self.pool())
        .await
        .context("read the stored revisions")?;
        rows.into_iter()
            .map(|(id, file_id, links)| {
                let export_links = match links {
                    Some(text) => serde_json::from_str(&text)
                        .with_context(|| format!("exportLinks of revision {id}"))?,
                    None => Value::Null,
                };
                Ok(StoredRevision {
                    id,
                    file_id,
                    export_links,
                })
            })
            .collect()
    }
}

/// One revision export as fetched: its edge's id and its bytes, or
/// `None` for a revision Drive no longer has the content of.
#[derive(Debug, Clone)]
pub struct RevisionExport {
    pub id: String,
    pub revision_id: String,
    pub part: String,
    pub content_type: String,
    pub bytes: Option<Vec<u8>>,
}

/// Write a flush of revision exports: the bytes into the CAS, which
/// commits on its own, then their edges.
pub async fn store_revision_exports(
    tx: &mut Transaction<'static, Sqlite>,
    cas: &BlobCas,
    exports: &[&RevisionExport],
) -> Result<()> {
    let inserts: Vec<CasInsert<'_, String>> = exports
        .iter()
        .filter_map(|e| {
            Some(CasInsert {
                id: e.id.clone(),
                bytes: e.bytes.as_deref()?,
                content_type: Some(e.content_type.as_str()),
            })
        })
        .collect();
    let stored = cas.put_many(inserts).await?;
    let edges: Vec<RevisionExportRow> = exports
        .iter()
        .map(|e| RevisionExportRow {
            id: e.id.clone(),
            revision_id: e.revision_id.clone(),
            part: e.part.clone(),
            blake3: stored.get(&e.id).cloned(),
        })
        .collect();
    bulk_upsert_in_tx(tx, &edges, &IsoOffsetTimestamp::now_local()).await
}

/// Upsert the rows, then put each one's volatile half in its sidecar,
/// which the upsert has just made sure exists.
async fn upsert_split<R: BulkUpsertable + Clone>(
    tx: &mut Transaction<'static, Sqlite>,
    table: &str,
    splits: &[&Split<R>],
    now: &IsoOffsetTimestamp,
) -> Result<()> {
    let rows: Vec<R> = splits.iter().map(|s| s.row.clone()).collect();
    bulk_upsert_in_tx(tx, &rows, now).await?;
    let volatile: Vec<(&str, &Value)> = splits
        .iter()
        .filter_map(|s| Some((s.row.id(), s.volatile.as_ref()?)))
        .collect();
    dr::set_volatile_payloads_in_tx(tx, table, &volatile).await
}
