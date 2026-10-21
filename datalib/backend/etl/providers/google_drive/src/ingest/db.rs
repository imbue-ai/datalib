//! Doltlite-backed raw store for the `google_drive` provider, and the one
//! write a capture makes.

use anyhow::Result;
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

use datalib_etl::blob_cas::{BlobCas, CasInsert};
use datalib_etl::bulk::{bulk_upsert_in_tx, BulkUpsertable};
use datalib_etl::doltlite_raw as dr;
use datalib_time::IsoOffsetTimestamp;

use super::schema_raw::{
    full_ddl, CapturePartRow, CaptureRow, CommentRow, FileRow, RevisionRow, CAPTURES, COMMENTS,
    FILES, LADDER, REVISIONS,
};

pub use datalib_etl::doltlite_raw::db_path_for;

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl(), LADDER);

/// A row and the volatile half split off its payload.
#[derive(Debug, Clone)]
pub struct Split<R> {
    pub row: R,
    pub volatile: Option<Value>,
}

/// One part of a capture: its bytes, or `None` for an export Google
/// declined to make.
#[derive(Debug, Clone)]
pub struct Part {
    pub name: String,
    pub bytes: Option<Vec<u8>>,
    pub content_type: Option<String>,
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
            .filter_map(|p| p.bytes.as_ref())
            .map(Vec::len)
            .sum()
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
        .filter_map(|(c, p)| {
            Some(CasInsert {
                id: edge_id(c, p),
                bytes: p.bytes.as_deref()?,
                content_type: p.content_type.as_deref(),
            })
        })
        .collect();
    let stored = cas.put_many(inserts).await?;
    let edges: Vec<CapturePartRow> = captures
        .iter()
        .flat_map(|c| c.parts.iter().map(move |p| (c, p)))
        .map(|(c, p)| {
            let id = edge_id(c, p);
            CapturePartRow {
                blake3: stored.get(&id).cloned(),
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
