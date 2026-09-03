//! Read-only view of what the qmd index currently holds, per document.
//!
//! Answers the two questions the grid's `Indexed` / `Embedded` columns
//! ask: is *the current content of this rendered markdown* in the qmd
//! index, and does it have a complete set of embedding vectors? The
//! second is the one that matters most in practice — `qmd update` and
//! `qmd embed` are separate passes, so a document can be findable by
//! keyword and invisible to semantic search for as long as the embed
//! pass takes.
//!
//! ## Why the join key is the content hash, not the path
//!
//! qmd stores each document under `handelize(<path relative to the
//! collection root>)` — a transform that lowercases nothing but
//! rewrites every run of non-alphanumeric characters to `-`
//! (`third-party/qmd/src/store.ts:1971`), so
//! `slack/rendered_md/a__b.md` is stored as
//! `slack/rendered-md/a-b.md`. Reproducing that in Rust means porting
//! its Unicode classes and its emoji→hex-codepoint step, and then
//! keeping the port in step with a vendored dependency we don't build.
//!
//! `documents.hash` is a plain SHA-256 over the file's UTF-8 bytes
//! (`store.ts:2365`), which we can compute exactly, from the same
//! bytes, with no shared code. It also answers freshness for free: qmd
//! decides whether to re-index a file by comparing this very hash
//! (`store.ts:1332`), so "the index holds a row whose hash equals the
//! file's hash right now" *is* qmd's own definition of up-to-date. A
//! path join would report a stale document as indexed.
//!
//! Verified against the TNG fixture's real qmd index: all 51 rendered
//! documents match `documents.hash` byte-for-byte
//! (`tests/qmd_index_state.rs`).
//!
//! One consequence worth naming: two rendered files with byte-identical
//! content share a hash, so if one is indexed both report indexed. They
//! also produce identical search behavior, so the badge is still
//! telling the truth about the content — just not about the file.
//!
//! ## What this module does not do
//!
//! It never writes. The `qmd_index` step is the index's only writer,
//! and the connection here is opened read-only so a bug on this side
//! cannot take a write lock on a file a sync is in the middle of
//! rebuilding.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use crate::qmd::{qmd_index_path, DEFAULT_COLLECTION};
use crate::repo::IndexRepo;

/// Max hashes per `IN (…)` batch. SQLite's default
/// `SQLITE_MAX_VARIABLE_NUMBER` is 999 on older builds; 400 leaves
/// room for the query's own binds and costs nothing at our sizes.
const HASH_BATCH: usize = 400;

/// What the qmd index holds for one content hash.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DocIndexState {
    /// An active `documents` row in our collection carries this hash —
    /// i.e. this exact content is in the keyword index.
    pub indexed: bool,
    /// …and it has a complete set of embedding vectors, so semantic
    /// search can reach it.
    pub embedded: bool,
}

/// Collection-wide totals, for the "N of M documents searchable" line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct QmdIndexSummary {
    /// Active `documents` rows in our collection.
    pub documents: u64,
    /// …of which have a complete vector set.
    pub embedded: u64,
}

/// Read-only handle on a data root's qmd index.
pub struct QmdIndexReader {
    pool: SqlitePool,
    collection: String,
}

impl QmdIndexReader {
    /// Open the qmd index under `root`, or `Ok(None)` when there isn't
    /// one yet — a data root that has never synced has no
    /// `index.sqlite`, which is a state to report ("nothing is
    /// indexed"), not an error to raise.
    pub async fn open(root: &Path) -> Result<Option<Self>, sqlx::Error> {
        let path = qmd_index_path(root);
        if !path.exists() {
            return Ok(None);
        }
        // `create_if_missing(false)` + `read_only(true)`: this file
        // belongs to the `qmd_index` step. Note qmd's index is a plain
        // SQLite database, unlike every `.doltlite_db` in the tree —
        // the doltlite amalgamation our binaries link reads it fine.
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
            .create_if_missing(false)
            .read_only(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        Ok(Some(Self {
            pool,
            collection: DEFAULT_COLLECTION.to_string(),
        }))
    }

    /// Wrap a pool that is already open on a qmd-shaped database.
    /// Exists for tests, which build the three tables in a throwaway
    /// file rather than shipping a binary index around.
    pub fn from_pool(pool: SqlitePool, collection: impl Into<String>) -> Self {
        Self {
            pool,
            collection: collection.into(),
        }
    }

    /// Look up the index state of each of `hashes`. Hashes with no
    /// active `documents` row are absent from the returned map; the
    /// caller reports those as not-indexed.
    pub async fn states_for_hashes(
        &self,
        hashes: &[String],
    ) -> Result<HashMap<String, DocIndexState>, sqlx::Error> {
        let mut out = HashMap::with_capacity(hashes.len());
        for chunk in hashes.chunks(HASH_BATCH) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            // The inner GROUP BY is per (hash, model): a hash embedded
            // under two models would otherwise have its chunk count
            // double-counted against a single model's `total_chunks`.
            // "Complete under at least one model" is the outer MAX.
            //
            // Deliberately NOT filtered to the *current* model +
            // fingerprint the way qmd's own `getHashesNeedingEmbedding`
            // is (`store.ts:2118`) — that predicate needs qmd's model
            // resolution, which lives in its config and its GGUF files.
            // The difference shows up only after a model change, where
            // we keep reporting `embedded` until the re-embed lands.
            let sql = format!(
                "SELECT d.hash AS hash, \
                        COALESCE(MAX(CASE WHEN v.chunks >= v.expected THEN 1 ELSE 0 END), 0) AS embedded \
                   FROM documents d \
                   LEFT JOIN (SELECT hash, model, COUNT(*) AS chunks, \
                                     MAX(total_chunks) AS expected \
                                FROM content_vectors GROUP BY hash, model) v \
                     ON v.hash = d.hash \
                  WHERE d.collection = ? AND d.active = 1 AND d.hash IN ({placeholders}) \
                  GROUP BY d.hash"
            );
            // Audited for injection per sqlx 0.9's `SqlSafeStr` bound: the only
            // interpolation is `placeholders`, a `?,?,?` run built from
            // `chunk.len()`. Collection and hashes are bound.
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(&self.collection);
            for h in chunk {
                q = q.bind(h);
            }
            for row in q.fetch_all(&self.pool).await? {
                let hash: String = row.try_get("hash")?;
                let embedded: i64 = row.try_get("embedded")?;
                out.insert(
                    hash,
                    DocIndexState {
                        indexed: true,
                        embedded: embedded != 0,
                    },
                );
            }
        }
        Ok(out)
    }

    /// Collection-wide totals.
    pub async fn summary(&self) -> Result<QmdIndexSummary, sqlx::Error> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS documents, \
                    COALESCE(SUM(CASE WHEN e.embedded = 1 THEN 1 ELSE 0 END), 0) AS embedded \
               FROM (SELECT DISTINCT hash FROM documents \
                      WHERE collection = ? AND active = 1) d \
               LEFT JOIN (SELECT hash, \
                                 MAX(CASE WHEN chunks >= expected THEN 1 ELSE 0 END) AS embedded \
                            FROM (SELECT hash, model, COUNT(*) AS chunks, \
                                         MAX(total_chunks) AS expected \
                                    FROM content_vectors GROUP BY hash, model) \
                           GROUP BY hash) e \
                 ON e.hash = d.hash",
        )
        .bind(&self.collection)
        .fetch_one(&self.pool)
        .await?;
        Ok(QmdIndexSummary {
            documents: row.try_get::<i64, _>("documents")? as u64,
            embedded: row.try_get::<i64, _>("embedded")? as u64,
        })
    }
}

/// SHA-256 of a file's bytes, hex-encoded — the same digest qmd stores
/// in `documents.hash` (`store.ts:2365`, over the UTF-8 text it read;
/// identical to the raw bytes for the valid UTF-8 our renderers emit).
///
/// SHA-256 and not blake3, which is what this repo hashes its *own*
/// content with (`blob_cas::blake3_hex`, `fswalk::hash_file`, the pdf
/// provider's `blake3`). This digest is not ours to choose: it is the
/// join key into an index a vendored dependency writes. Hence the
/// algorithm in the name — the same reason `wa_media_files` carries a
/// `sha256` (WhatsApp's key) and a `blake3` (our CAS key) side by side.
pub fn file_sha256_hex(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(format!("{:x}", h.finalize()))
}

/// One markdown's index state, as reported to a caller.
///
/// `indexed`/`embedded` are `Option<bool>` on purpose: `None` means "we
/// could not determine this" (no rendered document, file unreadable),
/// which is a different claim from `Some(false)` — "the index does not
/// hold this content". A UI that collapses the two shows a red ❌ for
/// facts nobody checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocReport {
    pub indexed: Option<bool>,
    pub embedded: Option<bool>,
    /// Why the answer is unknown. `None` on the ordinary paths.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl DocReport {
    fn unknown(note: impl Into<String>) -> Self {
        Self {
            indexed: None,
            embedded: None,
            note: Some(note.into()),
        }
    }
}

/// Resolve the index state of a set of rendered markdowns, by uuid.
///
/// The whole chain in one place, so the HTTP handler is glue and a test
/// can exercise what the handler actually runs:
///
/// 1. `markdown_uuid` → on-disk path, through `markdowns.md_path`
///    (a UUID lookup — no path *comparison* anywhere in this function).
/// 2. path → SHA-256 of the file's current bytes.
/// 3. hash → what the qmd index holds for that content.
///
/// Every requested uuid appears in the result, so a caller cannot read
/// an omission as a `false`. Errors that concern one document (an
/// unreadable file) become that document's `note`; only a failure that
/// concerns the whole batch is returned as `Err`.
pub async fn resolve_markdown_states(
    repo: &dyn IndexRepo,
    reader: &QmdIndexReader,
    markdown_uuids: &[String],
) -> Result<HashMap<String, DocReport>, String> {
    let mut out: HashMap<String, DocReport> = HashMap::with_capacity(markdown_uuids.len());

    let paths = repo
        .md_paths_for(markdown_uuids)
        .await
        .map_err(|e| format!("markdown paths: {e}"))?;

    // File reads + hashing: off the async executor.
    let to_hash: Vec<(String, std::path::PathBuf)> = markdown_uuids
        .iter()
        .filter_map(|u| paths.get(u).map(|p| (u.clone(), p.clone())))
        .collect();
    let hashed = tokio::task::spawn_blocking(move || {
        to_hash
            .into_iter()
            .map(|(uuid, path)| (uuid, file_sha256_hex(&path)))
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| format!("hashing task: {e}"))?;

    let mut hash_of: HashMap<String, String> = HashMap::new();
    for (uuid, res) in hashed {
        match res {
            Ok(h) => {
                hash_of.insert(uuid, h);
            }
            Err(e) => {
                // The index says what it says, but we cannot compare
                // against a file we cannot read.
                out.insert(
                    uuid,
                    DocReport::unknown(format!("rendered file unreadable: {e}")),
                );
            }
        }
    }

    let mut wanted: Vec<String> = hash_of.values().cloned().collect();
    wanted.sort();
    wanted.dedup();
    let states = reader
        .states_for_hashes(&wanted)
        .await
        .map_err(|e| format!("qmd lookup: {e}"))?;

    for u in markdown_uuids {
        if out.contains_key(u) {
            continue; // already answered (unreadable file)
        }
        let report = match hash_of.get(u) {
            Some(h) => {
                let st = states.get(h).copied().unwrap_or_default();
                DocReport {
                    indexed: Some(st.indexed),
                    embedded: Some(st.embedded),
                    note: None,
                }
            }
            // No `markdowns` row, or one with no `md_path`.
            None => DocReport::unknown("no rendered document"),
        };
        out.insert(u.clone(), report);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a throwaway database with qmd's three relevant tables.
    /// Only the columns this module reads are declared — a narrower
    /// schema than qmd's, on purpose: if we ever start depending on a
    /// column that isn't here, these tests fail rather than passing
    /// against a shape the real index doesn't have.
    async fn qmd_shaped_db(dir: &std::path::Path) -> SqlitePool {
        let pool = datalib_core::store::open_pool(&dir.join("index.sqlite"))
            .await
            .expect("open");
        sqlx::query(
            "CREATE TABLE documents (collection TEXT, path TEXT, hash TEXT, active INTEGER)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE content_vectors (hash TEXT, seq INTEGER, model TEXT, total_chunks INTEGER)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn add_doc(pool: &SqlitePool, collection: &str, path: &str, hash: &str, active: i64) {
        sqlx::query("INSERT INTO documents (collection, path, hash, active) VALUES (?,?,?,?)")
            .bind(collection)
            .bind(path)
            .bind(hash)
            .bind(active)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn add_chunks(pool: &SqlitePool, hash: &str, model: &str, have: i64, total: i64) {
        for seq in 0..have {
            sqlx::query(
                "INSERT INTO content_vectors (hash, seq, model, total_chunks) VALUES (?,?,?,?)",
            )
            .bind(hash)
            .bind(seq)
            .bind(model)
            .bind(total)
            .execute(pool)
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn embedded_requires_every_chunk() {
        let td = tempfile::tempdir().unwrap();
        let pool = qmd_shaped_db(td.path()).await;
        add_doc(&pool, "mirror", "a.md", "h_none", 1).await;
        add_doc(&pool, "mirror", "b.md", "h_partial", 1).await;
        add_doc(&pool, "mirror", "c.md", "h_full", 1).await;
        // No vectors at all — indexed for keyword search, invisible to
        // semantic search. This is the state the second column exists
        // to show.
        add_chunks(&pool, "h_partial", "m1", 2, 3).await;
        add_chunks(&pool, "h_full", "m1", 3, 3).await;

        let r = QmdIndexReader::from_pool(pool, "mirror");
        let st = r
            .states_for_hashes(&["h_none".into(), "h_partial".into(), "h_full".into()])
            .await
            .unwrap();
        assert_eq!(
            st["h_none"],
            DocIndexState {
                indexed: true,
                embedded: false
            }
        );
        assert_eq!(
            st["h_partial"],
            DocIndexState {
                indexed: true,
                embedded: false
            }
        );
        assert_eq!(
            st["h_full"],
            DocIndexState {
                indexed: true,
                embedded: true
            }
        );
    }

    /// A hash embedded under two models must not have its chunk counts
    /// pooled: 2 chunks under each of two 3-chunk models is 4 rows, and
    /// a naive `COUNT(*) >= MAX(total_chunks)` would call that complete.
    #[tokio::test]
    async fn chunk_completeness_is_per_model() {
        let td = tempfile::tempdir().unwrap();
        let pool = qmd_shaped_db(td.path()).await;
        add_doc(&pool, "mirror", "a.md", "h", 1).await;
        add_chunks(&pool, "h", "old", 2, 3).await;
        add_chunks(&pool, "h", "new", 2, 3).await;

        let r = QmdIndexReader::from_pool(pool, "mirror");
        let st = r.states_for_hashes(&["h".into()]).await.unwrap();
        assert!(
            !st["h"].embedded,
            "4 rows across two models is not one complete set"
        );
    }

    /// Complete under any one model counts as embedded — a re-embed
    /// that has finished for the new model while the old vectors are
    /// still around is not a regression to report.
    #[tokio::test]
    async fn complete_under_one_of_two_models_counts() {
        let td = tempfile::tempdir().unwrap();
        let pool = qmd_shaped_db(td.path()).await;
        add_doc(&pool, "mirror", "a.md", "h", 1).await;
        add_chunks(&pool, "h", "old", 1, 3).await;
        add_chunks(&pool, "h", "new", 3, 3).await;

        let r = QmdIndexReader::from_pool(pool, "mirror");
        let st = r.states_for_hashes(&["h".into()]).await.unwrap();
        assert!(st["h"].embedded);
    }

    /// Deactivated documents (qmd marks a vanished file `active = 0`
    /// rather than deleting it) and documents in another collection are
    /// both invisible — a hash we can't see reports as not indexed.
    #[tokio::test]
    async fn inactive_and_foreign_documents_do_not_count() {
        let td = tempfile::tempdir().unwrap();
        let pool = qmd_shaped_db(td.path()).await;
        add_doc(&pool, "mirror", "gone.md", "h_gone", 0).await;
        add_doc(&pool, "other", "x.md", "h_other", 1).await;
        add_chunks(&pool, "h_gone", "m", 1, 1).await;
        add_chunks(&pool, "h_other", "m", 1, 1).await;

        let r = QmdIndexReader::from_pool(pool, "mirror");
        let st = r
            .states_for_hashes(&["h_gone".into(), "h_other".into()])
            .await
            .unwrap();
        assert!(st.is_empty(), "got {st:?}");
    }

    /// Two paths with identical content share one hash. Both are
    /// indexed, and the summary counts the content once — it is a
    /// "how much is searchable" number, not a file census.
    #[tokio::test]
    async fn summary_counts_distinct_content() {
        let td = tempfile::tempdir().unwrap();
        let pool = qmd_shaped_db(td.path()).await;
        add_doc(&pool, "mirror", "a.md", "dup", 1).await;
        add_doc(&pool, "mirror", "b.md", "dup", 1).await;
        add_doc(&pool, "mirror", "c.md", "solo", 1).await;
        add_chunks(&pool, "dup", "m", 1, 1).await;

        let r = QmdIndexReader::from_pool(pool, "mirror");
        let s = r.summary().await.unwrap();
        assert_eq!(s.documents, 2);
        assert_eq!(s.embedded, 1);
    }

    /// Batching is an implementation detail; crossing the batch
    /// boundary must not drop or duplicate anything.
    #[tokio::test]
    async fn lookups_span_batches() {
        let td = tempfile::tempdir().unwrap();
        let pool = qmd_shaped_db(td.path()).await;
        let hashes: Vec<String> = (0..HASH_BATCH + 7).map(|i| format!("h{i:05}")).collect();
        for h in &hashes {
            add_doc(&pool, "mirror", h, h, 1).await;
        }
        let r = QmdIndexReader::from_pool(pool, "mirror");
        let st = r.states_for_hashes(&hashes).await.unwrap();
        assert_eq!(st.len(), hashes.len());
    }

    #[test]
    fn file_sha256_hex_is_sha256_of_the_bytes() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("x.md");
        std::fs::write(&p, b"hello\n").unwrap();
        // sha256("hello\n")
        assert_eq!(
            file_sha256_hex(&p).unwrap(),
            "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03"
        );
    }
}
