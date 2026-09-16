//! Facebook "Download your information" export ingester: every JSON
//! file becomes a table, every record a row, and every media file a
//! record points at goes into the CAS. The HTML flavour of the export is
//! not read — ask Facebook for JSON.

pub mod mojibake;
pub mod schema_raw;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::blob_cas::{cas_path_for, load_blake3_index, BlobCas, CasEdgeAccumulator};
use datalib_etl::bulk::BulkUpsertable as _;
use datalib_etl::control::DownloadControl;
use datalib_etl::doltlite_raw::{self as dr};
use datalib_etl::progress::Progress;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_macros::RawStoreHandle;
use serde::Serialize;
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use tracing::warn;
use uuid::Uuid;

use schema_raw::{canonical_table, facebook_ns, media_ddl, MediaBlobRow};

pub use datalib_etl::doltlite_raw::db_path_for;

/// Rows per multi-VALUES INSERT statement. 2 binds/row keeps us well
/// under SQLite's 32k-param ceiling.
const INSERT_CHUNK: usize = 400;

/// Ids per `DELETE … WHERE id IN (…)` statement, for the same reason.
const DELETE_CHUNK: usize = 900;

/// Media bytes held in memory before they are written to the CAS. An
/// export's photos can run to gigabytes, so the walk flushes as it goes.
const MEDIA_FLUSH_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, RawStoreHandle)]
pub struct RawDb {
    pool: SqlitePool,
    /// The media bytes. `Some` on the ingest path always; `None` only on a
    /// reader whose store predates any media, since opening a missing
    /// file read-only is an error and creating it would be a write
    /// render does not own.
    cas: Option<BlobCas>,
}

impl RawDb {
    /// Open the store to write it. The record tables are created as the
    /// walk meets their files, so only the media edge table is DDL here.
    pub async fn open(db_path: &Path) -> Result<Self> {
        let ddl = media_ddl();
        let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &ddl).await?;
        let cas = BlobCas::open(&cas_path_for(db_path)).await?;
        Ok(Self {
            pool,
            cas: Some(cas),
        })
    }

    /// Open the store to *read* it, for the render pass: no rescue
    /// commit, no DDL, no commit — three writes to a file render does not
    /// own. See `datalib_etl::doltlite_raw::open_reader`.
    pub async fn open_reader(db_path: &Path) -> Result<Self> {
        let cas_path = cas_path_for(db_path);
        let cas = if cas_path.is_file() {
            Some(BlobCas::open_reader(&cas_path).await?)
        } else {
            None
        };
        Ok(Self {
            pool: dr::open_reader(db_path).await?,
            cas,
        })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// `None` only on a reader whose store has no CAS file — see the field.
    pub fn cas(&self) -> Option<&BlobCas> {
        self.cas.as_ref()
    }

    /// Release every store this handle opened, and wait for the
    /// connections to go away. Dropping only schedules that.
    pub async fn close(self) {
        self.close_all().await;
    }

    pub async fn load_payloads(
        &self,
        reads: datalib_etl::pin::Reads<'_>,
        table: &str,
    ) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, reads, table).await
    }
}

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: two live connections to
    /// one `.doltlite_db` make each other's `dolt_commit` fail. See
    /// `datalib/backend/etl/README.md`.
    pub db: RawDb,
    /// Root of the unpacked export: the directory holding
    /// `your_facebook_activity/`, `connections/` and the rest.
    pub input_path: PathBuf,
    pub progress: Progress,
    pub control: DownloadControl,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct FetchSummary {
    pub files: usize,
    pub rows: usize,
    pub parse_errors: usize,
    /// Media files whose bytes this run put into the CAS.
    pub media_stored: usize,
    /// Media files already in the CAS from an earlier run.
    pub media_known: usize,
    /// `uri`s no file under the export root answers to.
    pub media_missing: usize,
}

/// Every record table this run read, each keyed by row id.
type Tables = BTreeMap<String, BTreeMap<String, Value>>;

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = opts.db.clone();
    // Every run is a full snapshot of the export: the rows land by upsert
    // and whatever the export no longer holds is pruned at the end of
    // the same transaction, so there is nothing a reset would add.
    let _ = opts.control.reset_and_redownload;

    let mut summary = FetchSummary::default();
    let mut by_table: Tables = BTreeMap::new();

    for path in discover_json(&opts.input_path) {
        let rel = relative(&opts.input_path, &path);
        let table = canonical_table(&rel);
        match read_records(&path) {
            Ok(records) => {
                summary.files += 1;
                let rows = by_table.entry(table.clone()).or_default();
                for record in records {
                    let id = row_id(&table, &record);
                    rows.insert(id, record);
                }
                opts.progress.set_message(&format!(
                    "{table}: {} rows ({} files)",
                    rows.len(),
                    summary.files
                ));
            }
            Err(e) => {
                warn!(event = "facebook_file_failed", file = %path.display(), table, error = %format!("{e:#}"));
                summary.parse_errors += 1;
            }
        }
    }

    let mut tx = db.pool().begin().await.context("begin facebook tx")?;
    for (table, rows) in &by_table {
        upsert_and_prune(&mut tx, table, rows).await?;
        summary.rows += rows.len();
    }
    tx.commit().await.context("commit facebook tx")?;

    // Media after the records are committed: an edge is additive, and a
    // photo that fails to read costs a warning, never the rows.
    // Through the handle's own CAS, so nothing here opens a second store.
    if let Some(cas) = db.cas() {
        store_media(
            &db,
            cas,
            &opts.input_path,
            &by_table,
            &opts.progress,
            &mut summary,
        )
        .await?;
    }
    Ok(summary)
}

/// Upsert this run's rows and delete the ones the export no longer
/// holds, in the caller's transaction, so a commit landing at any point
/// sees either last run's table or this run's — never an emptied one.
async fn upsert_and_prune(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    rows: &BTreeMap<String, Value>,
) -> Result<()> {
    let ddl = dr::wire_payload_table_ddl(table, &[]);
    // Audited: `table` is `canonical_table`'s output — ASCII alphanumerics
    // and `_` only — so it is safe as an identifier; rows are bound.
    sqlx::query(sqlx::AssertSqlSafe(ddl))
        .execute(&mut **tx)
        .await
        .with_context(|| format!("create table {table}"))?;

    let existing: Vec<String> =
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT id FROM {table}")))
            .fetch_all(&mut **tx)
            .await
            .with_context(|| format!("list ids in {table}"))?;
    let gone: Vec<&String> = existing
        .iter()
        .filter(|id| !rows.contains_key(*id))
        .collect();
    for chunk in gone.chunks(DELETE_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE id IN ({placeholders})"
        )));
        for id in chunk {
            q = q.bind((*id).clone());
        }
        q.execute(&mut **tx)
            .await
            .with_context(|| format!("prune {table}"))?;
    }

    let rows: Vec<(&String, String)> = rows.iter().map(|(id, v)| (id, v.to_string())).collect();
    for chunk in rows.chunks(INSERT_CHUNK) {
        let mut sql = format!("INSERT OR REPLACE INTO {table} (id, payload) VALUES ");
        for i in 0..chunk.len() {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str("(?, jsonb(?))");
        }
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for (id, payload) in chunk {
            q = q.bind((*id).clone()).bind(payload.clone());
        }
        q.execute(&mut **tx)
            .await
            .with_context(|| format!("insert into {table}"))?;
    }
    Ok(())
}

/// Every `uri` in every record, read off disk into the CAS once. A `uri`
/// is export-relative (`your_facebook_activity/posts/media/…`), and one
/// file can be reached from several records — an album and the post
/// that added its photos — so the edge is per (record, uri) and the
/// bytes are stored once.
async fn store_media(
    db: &RawDb,
    cas: &BlobCas,
    root: &Path,
    by_table: &Tables,
    progress: &Progress,
    summary: &mut FetchSummary,
) -> Result<()> {
    let mut known = load_blake3_index(db.pool(), MediaBlobRow::TABLE, MediaBlobRow::REF_COLUMN)
        .await
        .context("load media_blobs index")?;
    let mut acc = CasEdgeAccumulator::new();
    let mut pending_bytes = 0usize;
    let mut missing: HashSet<String> = HashSet::new();

    for rows in by_table.values() {
        for (id, record) in rows {
            let mut uris = Vec::new();
            collect_uris(record, &mut uris);
            uris.sort();
            uris.dedup();
            for uri in uris {
                if let Some(hash) = known.get(&uri) {
                    acc.add_known(id, &uri, hash.clone());
                    summary.media_known += 1;
                    continue;
                }
                if missing.contains(&uri) {
                    acc.add_failed(id, &uri, "media file not in the export");
                    continue;
                }
                let path = root.join(&uri);
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        let hash = datalib_etl::blob_cas::blake3_hex(&bytes);
                        pending_bytes += bytes.len();
                        acc.add_fetched(id, &uri, bytes, guess_content_type(&uri), file_name(&uri));
                        known.insert(uri.clone(), hash);
                        summary.media_stored += 1;
                    }
                    Err(e) => {
                        warn!(event = "facebook_media_missing", uri, error = %e);
                        missing.insert(uri.clone());
                        acc.add_failed(id, &uri, "media file not in the export");
                        summary.media_missing += 1;
                    }
                }
            }
            if pending_bytes >= MEDIA_FLUSH_BYTES {
                flush_media(&acc, db, cas).await?;
                acc = CasEdgeAccumulator::new();
                pending_bytes = 0;
                progress.set_message(&format!("media: {} stored", summary.media_stored));
            }
        }
    }
    flush_media(&acc, db, cas).await
}

async fn flush_media(acc: &CasEdgeAccumulator, db: &RawDb, cas: &BlobCas) -> Result<()> {
    acc.flush(db.pool(), cas, |owning, uri, blake3| MediaBlobRow {
        id: MediaBlobRow::pk_recipe(owning, uri),
        owner_id: owning.to_string(),
        uri: uri.to_string(),
        blake3: blake3.map(str::to_string),
    })
    .await
    .context("flush media_blobs")
}

/// Every `uri` string in a record, wherever it is nested: a post's
/// attachments, an album's photos and cover, a comment's attachment.
fn collect_uris(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, v) in map {
                if k == "uri" {
                    if let Some(s) = v.as_str() {
                        if looks_like_export_path(s) {
                            out.push(s.to_string());
                        }
                    }
                } else {
                    collect_uris(v, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| collect_uris(v, out)),
        _ => {}
    }
}

/// A `uri` that names a file in the export rather than a web address —
/// a shared link's `uri` is `https://…`, which has nothing to store.
fn looks_like_export_path(s: &str) -> bool {
    !s.is_empty() && !s.contains("://") && !s.starts_with('/') && !s.contains("..")
}

/// Parse one export file into its records: an array is one record per
/// element, an object wrapping a single array (`{"comments_v2": […]}`)
/// likewise, and anything else — an album, the profile — is one record.
/// Every string is passed through [`mojibake::fix`] on the way in.
fn read_records(path: &Path) -> Result<Vec<Value>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut v: Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    mojibake::fix(&mut v);
    Ok(split_records(v))
}

fn split_records(v: Value) -> Vec<Value> {
    match v {
        Value::Array(items) => items,
        Value::Object(map)
            if map.len() == 1 && map.values().next().is_some_and(Value::is_array) =>
        {
            match map.into_iter().next() {
                Some((_, Value::Array(items))) => items,
                _ => Vec::new(),
            }
        }
        other => vec![other],
    }
}

/// A record's row id: Facebook's own `fbid` when the record carries one,
/// else a uuidv5 over the table and the record's canonical JSON. Most
/// records — posts, comments, the profile — have no id of their own.
fn row_id(table: &str, record: &Value) -> String {
    if let Some(fbid) = record
        .get("fbid")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return fbid.to_string();
    }
    let recipe = format!("{table}\u{0}{record}");
    Uuid::new_v5(&facebook_ns(), recipe.as_bytes())
        .as_hyphenated()
        .to_string()
}

fn discover_json(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("json"))
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn file_name(uri: &str) -> Option<String> {
    uri.rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn guess_content_type(uri: &str) -> Option<String> {
    let ext = uri.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    let ct = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "pdf" => "application/pdf",
        _ => return None,
    };
    Some(ct.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn splits_the_three_file_shapes() {
        assert_eq!(split_records(json!([1, 2])).len(), 2);
        assert_eq!(
            split_records(json!({"friends_v2": [{"name": "a"}]})).len(),
            1
        );
        assert_eq!(
            split_records(json!({"friends_v2": [{"name": "a"}]}))[0],
            json!({"name": "a"})
        );
        // An album, a profile: one key or several, none of them the list.
        let album = json!({"name": "Album", "photos": [], "description": "d"});
        assert_eq!(split_records(album.clone()), vec![album]);
        let profile = json!({"profile_v2": {"name": {"full_name": "x"}}});
        assert_eq!(split_records(profile.clone()), vec![profile]);
    }

    #[test]
    fn row_id_prefers_fbid_then_hashes() {
        assert_eq!(row_id("t", &json!({"fbid": "123", "x": 1})), "123");
        let a = row_id("t", &json!({"timestamp": 1, "title": "a"}));
        assert_eq!(a.len(), 36);
        assert_eq!(a, row_id("t", &json!({"timestamp": 1, "title": "a"})));
        assert_ne!(a, row_id("u", &json!({"timestamp": 1, "title": "a"})));
    }

    #[test]
    fn collects_nested_export_uris_and_skips_web_ones() {
        let v = json!({
            "attachments": [{"data": [{"media": {"uri": "your_facebook_activity/posts/media/a.jpg"}}]}],
            "cover_photo": {"uri": "your_facebook_activity/posts/media/b.jpg"},
            "link": {"uri": "https://example.com/x"},
        });
        let mut out = Vec::new();
        collect_uris(&v, &mut out);
        out.sort();
        assert_eq!(
            out,
            vec![
                "your_facebook_activity/posts/media/a.jpg",
                "your_facebook_activity/posts/media/b.jpg"
            ]
        );
    }

    #[test]
    fn content_type_by_extension() {
        assert_eq!(guess_content_type("x/y.JPG").as_deref(), Some("image/jpeg"));
        assert_eq!(
            guess_content_type("x/y.webp").as_deref(),
            Some("image/webp")
        );
        assert_eq!(guess_content_type("x/y.mp4").as_deref(), Some("video/mp4"));
        assert_eq!(guess_content_type("x/y"), None);
    }
}
