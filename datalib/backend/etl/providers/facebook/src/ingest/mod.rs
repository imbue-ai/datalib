//! Facebook "Download your information" export ingester: every JSON
//! file becomes a table, every record a row, and every media file a
//! record points at goes into the CAS. The HTML flavour of the export is
//! not read — ask Facebook for JSON.

pub mod messenger;
mod migrate;
pub mod mojibake;
pub mod schema_raw;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::blob_cas::{cas_path_for, load_blake3_index, BlobCas, CasEdgeAccumulator};
use datalib_etl::bulk::BulkUpsertable as _;
use datalib_etl::control::DownloadControl;
use datalib_etl::doltlite_raw::{self as dr};
use datalib_etl::download_problems::RunProblem;
use datalib_etl::progress::Progress;
use datalib_etl::prune;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_files::export_files::ExportFiles;
use datalib_etl_files::file_checkpoint;
use datalib_etl_files::fsscan::ScannedFile;
use datalib_etl_macros::RawStoreHandle;
use datalib_problems::Reason;
use serde::Serialize;
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use uuid::Uuid;

use messenger::ThreadFile;
use schema_raw::{
    canonical_table, facebook_ns, store_ddl, MediaBlobRow, MESSENGER_MESSAGES_TABLE,
    MESSENGER_THREADS_TABLE,
};

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
    /// The commit a reader is pinned at; `None` for the writer.
    pin: Option<datalib_etl::pin::Pin>,
}

impl RawDb {
    /// Open the store to write it. The record tables are created as the
    /// walk meets their files, so only the fixed tables are DDL here.
    pub async fn open(db_path: &Path) -> Result<Self> {
        let ddl = store_ddl();
        let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
        let pool = dr::open_migrating(db_path, &ddl, schema_raw::LADDER).await?;
        let cas = BlobCas::open(&cas_path_for(db_path)).await?;
        Ok(Self {
            pool,
            cas: Some(cas),
            pin: None,
        })
    }

    /// Open the store to *read* it, for the render pass: no discard of
    /// the working set, no DDL, no commit — three writes to a file render
    /// does not own. See `datalib_etl::doltlite_raw::open_reader`.
    ///
    /// Pinned at `commit`, else HEAD; `None` when nothing is committed.
    pub async fn open_reader(db_path: &Path, commit: Option<&str>) -> Result<Option<Self>> {
        let Some(reader) = dr::open_reader(db_path, commit).await? else {
            return Ok(None);
        };
        let cas = BlobCas::open_for_render(db_path).await?;
        Ok(Some(Self {
            pool: reader.pool().clone(),
            cas,
            pin: Some(reader.pin().clone()),
        }))
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// The commit this reader reads at. `None` on the writer's handle.
    pub fn pin(&self) -> Option<&datalib_etl::pin::Pin> {
        self.pin.as_ref()
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

    pub async fn load_payloads(&self, table: &str) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, table).await
    }
}

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
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
    /// Records the export no longer holds, deleted.
    pub removed: usize,
    /// Media edges of deleted records, or to a `uri` a record stopped naming.
    pub media_edges_removed: usize,
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
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    run_problems::collecting(&pool, &stop, |found| read_export(opts, found)).await
}

async fn read_export(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let db = opts.db.clone();

    let mut summary = FetchSummary::default();
    let mut read: BTreeMap<String, Vec<(String, Value)>> = BTreeMap::new();
    let export = ExportFiles::walk(&opts.input_path)?;
    let mut problems = export.walk_problems();
    // Chunks of one table share it (`album/0.json`, `album/1.json`), so a
    // chunk that would not read leaves the whole table unpruned: its rows
    // are not in this run's set, and absence from it means nothing.
    let mut unread_tables: HashSet<String> = HashSet::new();
    // Every chunk file this run read, by table, to stamp as what the table
    // was last read from.
    let mut chunks: BTreeMap<String, Vec<ScannedFile>> = BTreeMap::new();
    let mut present: HashSet<String> = HashSet::new();

    for path in export.with_extension("json") {
        let rel = relative(&opts.input_path, path);
        present.insert(rel.clone());
        match read_records(path, &rel) {
            Ok((records, file)) => {
                summary.files += 1;
                for table in tables_of(&rel) {
                    chunks.entry(table).or_default().push(file.clone());
                }
                for r in records {
                    read.entry(r.table).or_default().push((r.id, r.payload));
                }
                opts.progress
                    .set_message(&format!("{rel}: read ({} files)", summary.files));
            }
            Err(e) => {
                problems.push(RunProblem::listing(
                    &format!("file {rel}"),
                    format!("{e:#}"),
                ));
                unread_tables.extend(tables_of(&rel));
                summary.parse_errors += 1;
            }
        }
    }

    let by_table: Tables = read
        .into_iter()
        .map(|(table, rows)| {
            let rows = key_rows(&table, rows);
            (table, rows)
        })
        .collect();

    // A table is split into chunks (`album/0.json`, `album/1.json`), and an
    // export unpacked only in part can hold some of them: absence from this
    // run's set then means nothing. A table prunes only when every chunk it
    // was last read from is here.
    let mut short_tables: HashSet<String> = HashSet::new();
    for table in by_table.keys() {
        let scope = chunk_scope(table);
        for rel in file_checkpoint::load_cursor(db.pool(), &scope)
            .await?
            .keys()
        {
            if !present.contains(rel) {
                problems.push(RunProblem::listing(
                    &format!("file {rel}"),
                    "a part of a table the export holds the rest of is missing, so \
                     nothing of the table was deleted; reset the source if the export \
                     really has fewer parts now"
                        .to_string(),
                ));
                short_tables.insert(table.clone());
            }
        }
    }

    let mut tx = db.pool().begin().await.context("begin facebook tx")?;
    let mut pruned: HashSet<String> = HashSet::new();
    let mut read_in_pruned: HashSet<&str> = HashSet::new();
    for (table, rows) in &by_table {
        let prune = export.errors.is_empty()
            && !unread_tables.contains(table)
            && !short_tables.contains(table);
        let gone = upsert_and_prune(&mut tx, table, rows, prune).await?;
        if prune {
            pruned.extend(gone);
            read_in_pruned.extend(rows.keys().map(String::as_str));
        }
        summary.rows += rows.len();
        for file in chunks.get(table).into_iter().flatten() {
            file_checkpoint::record_file(&mut tx, &chunk_scope(table), file).await?;
        }
    }
    summary.removed = pruned.len();
    summary.media_edges_removed =
        prune_media_edges(&mut tx, &by_table, &pruned, &read_in_pruned).await?;
    tx.commit().await.context("commit facebook tx")?;

    // Media after the records are committed: an edge is additive, and a
    // photo that fails to read costs its edge's problem, never the rows.
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
    found.extend(problems);
    Ok(summary)
}

/// Delete the media edges that are no longer true, in the transaction that
/// prunes the records: a deleted record's, and those of a record read this
/// run, in a table that pruned, to a `uri` it no longer names. A record in
/// a table held back keeps its edges, as it keeps its row. Returns how many
/// went.
async fn prune_media_edges(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    by_table: &Tables,
    pruned: &HashSet<String>,
    read_in_pruned: &HashSet<&str>,
) -> Result<usize> {
    let mut read: HashSet<&str> = HashSet::new();
    let mut named: HashSet<String> = HashSet::new();
    for rows in by_table.values() {
        for (id, record) in rows {
            read.insert(id);
            let mut uris = Vec::new();
            collect_uris(record, &mut uris);
            named.extend(uris.iter().map(|uri| MediaBlobRow::pk_recipe(id, uri)));
        }
    }
    let stored: Vec<(String, String)> = sqlx::query_as("SELECT id, owner_id FROM media_blobs")
        .fetch_all(&mut **tx)
        .await
        .context("list media_blobs")?;
    let held = stored.len();
    let keep: HashSet<String> = stored
        .into_iter()
        .filter(|(id, owner)| {
            let owner = owner.as_str();
            let owner_gone = pruned.contains(owner) && !read.contains(owner);
            let unnamed = read_in_pruned.contains(owner) && !named.contains(id);
            !(owner_gone || unnamed)
        })
        .map(|(id, _)| id)
        .collect();
    let gone = prune::prune_scope_in_tx(tx, MediaBlobRow::TABLE, &[], &keep).await?;
    prune::record(MediaBlobRow::TABLE, held, gone.len());
    Ok(gone.len())
}

/// Upsert this run's rows and, when `prune`, delete the ones the export
/// no longer holds, in the caller's transaction, so a commit landing at
/// any point sees either last run's table or this run's — never an
/// emptied one. Returns the ids deleted.
async fn upsert_and_prune(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    rows: &BTreeMap<String, Value>,
    prune: bool,
) -> Result<Vec<String>> {
    upsert_rows(tx, table, rows).await?;
    // Audited: `table` is `canonical_table`'s output or a Messenger table
    // constant — ASCII alphanumerics and `_` only — so it is safe as an
    // identifier; ids are bound.
    let existing: Vec<String> = if prune {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT id FROM {table}")))
            .fetch_all(&mut **tx)
            .await
            .with_context(|| format!("list ids in {table}"))?
    } else {
        Vec::new()
    };
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
    Ok(gone.into_iter().cloned().collect())
}

/// Create `table` if it is new and upsert `rows` into it. `table` is
/// `canonical_table`'s output or a Messenger table constant.
async fn upsert_rows(
    conn: &mut sqlx::SqliteConnection,
    table: &str,
    rows: &BTreeMap<String, Value>,
) -> Result<()> {
    sqlx::query(sqlx::AssertSqlSafe(dr::wire_payload_table_ddl(table, &[])))
        .execute(&mut *conn)
        .await
        .with_context(|| format!("create table {table}"))?;
    let rows: Vec<(&String, String)> = rows.iter().map(|(id, v)| (id, v.to_string())).collect();
    for chunk in rows.chunks(INSERT_CHUNK) {
        let mut sql = format!("INSERT INTO {table} (id, payload) VALUES ");
        for i in 0..chunk.len() {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str("(?, jsonb(?))");
        }
        sql.push_str(" ON CONFLICT(id) DO UPDATE SET payload = excluded.payload");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for (id, payload) in chunk {
            q = q.bind((*id).clone()).bind(payload.clone());
        }
        q.execute(&mut *conn)
            .await
            .with_context(|| format!("insert into {table}"))?;
    }
    Ok(())
}

/// The `ingested_files` scope naming the chunk files a table was read from.
fn chunk_scope(table: &str) -> String {
    format!("facebook/{table}")
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
    // A `uri` this run could not read, and the edge it leaves: skipped
    // when the export left the file out, failed when it is there and
    // would not read.
    let mut unread: HashMap<String, (bool, String)> = HashMap::new();
    // A `uri` whose bytes are in the accumulator, not yet flushed: the
    // CAS names them at the flush, and `known` learns the name then.
    let mut pending: HashSet<String> = HashSet::new();

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
                if pending.contains(&uri) {
                    acc.add_again(id, &uri);
                    summary.media_known += 1;
                    continue;
                }
                if !unread.contains_key(&uri) {
                    match std::fs::read(root.join(&uri)) {
                        Ok(bytes) => {
                            pending_bytes += bytes.len();
                            acc.add_fetched(id, &uri, bytes, guess_content_type(&uri));
                            pending.insert(uri);
                            summary.media_stored += 1;
                            continue;
                        }
                        Err(e) => {
                            let not_found = e.kind() == std::io::ErrorKind::NotFound;
                            let detail = if not_found {
                                format!("media file not in the export: {e}")
                            } else {
                                format!("media file would not read: {e}")
                            };
                            unread.insert(uri.clone(), (not_found, detail));
                            summary.media_missing += 1;
                        }
                    }
                }
                let (not_found, detail) = &unread[&uri];
                if *not_found {
                    acc.add_skipped(id, &uri, Reason::NotFound, detail.clone());
                } else {
                    acc.add_failed(id, &uri, detail.clone());
                }
            }
            if pending_bytes >= MEDIA_FLUSH_BYTES {
                known.extend(flush_media(&acc, db, cas).await?);
                pending.clear();
                acc = CasEdgeAccumulator::new();
                pending_bytes = 0;
                progress.set_message(&format!("media: {} stored", summary.media_stored));
            }
        }
    }
    flush_media(&acc, db, cas).await?;
    Ok(())
}

async fn flush_media(
    acc: &CasEdgeAccumulator,
    db: &RawDb,
    cas: &BlobCas,
) -> Result<HashMap<String, String>> {
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

/// The tables one export file's records land in.
fn tables_of(rel: &str) -> Vec<String> {
    match ThreadFile::of(rel) {
        Some(_) => vec![
            MESSENGER_THREADS_TABLE.to_string(),
            MESSENGER_MESSAGES_TABLE.to_string(),
        ],
        None => vec![canonical_table(rel)],
    }
}

/// One record of an export file, and the table and row it lands in.
pub struct Record {
    pub table: String,
    pub id: String,
    pub payload: Value,
}

/// One export file's records. Every string is
/// passed through [`mojibake::fix`] on the way in. The file comes back as
/// what to stamp once its rows are stored.
fn read_records(path: &Path, rel: &str) -> Result<(Vec<Record>, ScannedFile)> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut v: Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    mojibake::fix(&mut v);
    let file = ScannedFile {
        path: path.to_path_buf(),
        rel: rel.to_string(),
        size: bytes.len() as i64,
        blake3: *blake3::hash(&bytes).as_bytes(),
    };
    Ok((records_of(rel, v)?, file))
}

fn records_of(rel: &str, v: Value) -> Result<Vec<Record>> {
    if let Some(at) = ThreadFile::of(rel) {
        return at.rows(v);
    }
    let table = canonical_table(rel);
    Ok(split_records(v)
        .into_iter()
        .map(|payload| Record {
            table: table.clone(),
            id: row_id(&table, &payload),
            payload,
        })
        .collect())
}

/// An array is one record per element, an object wrapping a single array
/// (`{"comments_v2": […]}`) likewise, and anything else — an album, the
/// profile — is one record.
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
    content_id(table, record)
}

/// One table's rows by id. Facebook's `fbid` is the key where a record
/// has one, but an `fbid` is not always one record's alone: where it
/// names two that differ, each is keyed by its content instead, so
/// neither overwrites the other. The same record read twice is one row.
fn key_rows(table: &str, rows: Vec<(String, Value)>) -> BTreeMap<String, Value> {
    let mut by_fbid: HashMap<String, HashSet<String>> = HashMap::new();
    for (id, record) in &rows {
        if is_fbid_key(id, record) {
            by_fbid
                .entry(id.clone())
                .or_default()
                .insert(record.to_string());
        }
    }
    rows.into_iter()
        .map(|(id, record)| {
            let shared = is_fbid_key(&id, &record) && by_fbid.get(&id).is_some_and(|v| v.len() > 1);
            let id = if shared {
                content_id(table, &record)
            } else {
                id
            };
            (id, record)
        })
        .collect()
}

fn is_fbid_key(id: &str, record: &Value) -> bool {
    record.get("fbid").and_then(Value::as_str) == Some(id)
}

fn content_id(table: &str, record: &Value) -> String {
    let recipe = format!("{table}\u{0}{record}");
    Uuid::new_v5(&facebook_ns(), recipe.as_bytes())
        .as_hyphenated()
        .to_string()
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
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

    /// Two edit records of one post carried one `fbid`, and the second
    /// overwrote the first: a version of the post was lost at ingest.
    #[test]
    fn an_fbid_that_names_two_records_keeps_both() {
        let a = json!({"fbid": "9", "timestamp": 1, "text": "first"});
        let b = json!({"fbid": "9", "timestamp": 2, "text": "second"});
        let c = json!({"fbid": "8", "timestamp": 3});
        let rows = key_rows(
            "t",
            vec![
                ("9".into(), a.clone()),
                ("9".into(), b.clone()),
                ("8".into(), c.clone()),
                ("8".into(), c.clone()),
            ],
        );
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(rows.get("8"), Some(&c), "a lone fbid is still the key");
        assert!(rows.values().any(|v| v == &a) && rows.values().any(|v| v == &b));
        assert!(!rows.contains_key("9"));
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
