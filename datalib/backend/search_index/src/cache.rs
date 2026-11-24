//! The inference cache: a chunk's vector under the model that made it and
//! the hash of the prompt it was shown (`chunk::chunk_hash`). A model is a
//! pure function, so an entry is never wrong, only unused; it lives in a
//! file of its own so that rebuilding the index, or the grid index it may
//! join, never throws a day of embedding away. The `vectors` table knows
//! no document: the chunks filed beside it (`chunked.rs`) name the
//! hashes, and search joins from them to here.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use datalib_store_meta::StoreKind;
use sqlx::sqlite::{SqliteConnection, SqlitePool};

const DDL: [&str; 1] = ["CREATE TABLE IF NOT EXISTS vectors (
        model TEXT NOT NULL,
        hash TEXT NOT NULL,
        vector BLOB NOT NULL,
        PRIMARY KEY (model, hash)
    ) WITHOUT ROWID"];

/// The keys alone, a few percent of the table: every row of `vectors`
/// holds a 3 KB vector, so asking it whether a vector is held reads a
/// page for every few chunks, and on a cache larger than the page cache
/// that is a disk read each. SQLite picks this index for a lookup by `IN`,
/// but the primary key for a correlated `NOT EXISTS`, so a query that only
/// asks names it (`INDEXED BY`) rather than leave it to the planner.
/// Recorded apart from [`DDL`], as the file's `index_hash`: a cache whose
/// tables move is emptied, and one whose indexes move only gets them.
const KEYS: &str = "CREATE INDEX IF NOT EXISTS vectors_keys ON vectors (model, hash)";

/// Of a model's vectors, the hashes held among a bound JSON array of them.
const HELD_SQL: &str = "SELECT hash FROM vectors INDEXED BY vectors_keys WHERE model = ? \
                          AND hash IN (SELECT value FROM json_each(?))";

/// How many hashes go into one statement's bound JSON array.
const BATCH: usize = 10_000;

/// The cache's page size. A WITHOUT ROWID table keeps at most about a
/// sixteenth of a page of a row in its leaf (under 1 KB of SQLite's
/// default 4 KB), so at 4 KB each 3 KB vector spilled onto an overflow
/// page of its own, wherever it was written, and reading every vector was
/// a random read each. At 16 KB the row stays in its leaf, and reading
/// the vectors in key order reads the file in order: over 932k vectors,
/// 105 s against about 8 s. The file's header records it, so it is part of
/// the shape a reader asks for ([`require`]) though not of [`shape`], and
/// moving it keeps every row.
pub const PAGE_SIZE: i64 = 16_384;

/// How long the cache's writer waits for another connection to close.
const WAIT: Duration = Duration::from_secs(60);

/// What every source's `embed` writes, so any one brings it up to date.
pub(crate) const CACHE_STEP: &str = "<source>/embed";

/// The cache's shape. Its tables are the vectors alone: a change to them
/// empties the cache, and every source's `embed` embeds again. What can be
/// made again without them, the keys' index and the chunks filed beside
/// the vectors, is its `indexes`: a change to those drops the chunk tables
/// alone, and each `embed` cuts its documents again and finds its vectors
/// by their prompts' hashes.
pub fn shape() -> datalib_store_meta::Shape {
    datalib_store_meta::Shape {
        tables: datalib_store_meta::schema_hash(DDL),
        indexes: Some(datalib_store_meta::schema_hash(remade())),
        versions: datalib_store_meta::Versions::default(),
    }
}

fn remade() -> impl Iterator<Item = &'static str> {
    crate::chunked::DDL.iter().copied().chain([KEYS])
}

/// Refuses the cache at `path`, open as `pool`, unless it is in this
/// build's shape, pages included. Every reader asks before it reads, so
/// none holds the file while [`rewrite_pages`] needs it to itself.
pub(crate) async fn require(path: &Path, pool: &SqlitePool) -> Result<()> {
    let meta = datalib_store_meta::read(pool).await?;
    datalib_store_meta::require(path, Some(CACHE_STEP), meta.as_ref(), &shape())?;
    if page_size(&mut *pool.acquire().await?).await? == PAGE_SIZE {
        return Ok(());
    }
    Err(anyhow::Error::new(datalib_store_meta::OtherShape {
        store: path.to_path_buf(),
        have: meta.map(|m| m.shape()),
        want: shape(),
        step: Some(CACHE_STEP.to_string()),
    }))
}

/// Whether the cache at `path` is in pages of another size than this
/// build writes.
pub async fn in_other_pages(path: &Path) -> Result<bool> {
    let pool = crate::read::connect(path).await?;
    let size = async { page_size(&mut *pool.acquire().await?).await }.await;
    pool.close().await;
    Ok(size? != PAGE_SIZE)
}

/// Rewrites the cache at `path` in [`PAGE_SIZE`] pages, keeping every row.
/// The caller is the cache's one writer.
pub async fn rewrite_pages(path: &Path) -> Result<()> {
    rewrite_pages_within(path, WAIT).await
}

async fn rewrite_pages_within(path: &Path, wait: Duration) -> Result<()> {
    let pool = crate::file::open(path, &[], WAIT)
        .await
        .with_context(|| format!("open the inference cache at {}", path.display()))?;
    let rewritten = to_page_size(&pool, wait).await;
    pool.close().await;
    rewritten.with_context(|| format!("rewrite {} in larger pages", path.display()))
}

async fn page_size(conn: &mut SqliteConnection) -> Result<i64> {
    sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(conn)
        .await
        .context("read the page size")
}

/// The page size moves only by a `VACUUM` outside WAL mode.
async fn to_page_size(pool: &SqlitePool, wait: Duration) -> Result<()> {
    let mut conn = pool.acquire().await?;
    let have = page_size(&mut conn).await?;
    if have == PAGE_SIZE {
        return Ok(());
    }
    let started = Instant::now();
    leave_wal(&mut conn, wait).await?;
    // `VACUUM` copies the file through a temporary database, which goes
    // to disk: this connection keeps its temporaries in memory otherwise.
    let rewrite = async {
        // Audited: `PAGE_SIZE` is a number.
        let page_size = format!("PRAGMA page_size = {PAGE_SIZE}");
        sqlx::query(sqlx::AssertSqlSafe(page_size))
            .execute(&mut *conn)
            .await?;
        for sql in ["PRAGMA temp_store = FILE", "VACUUM"] {
            sqlx::query(sql).execute(&mut *conn).await?;
        }
        anyhow::Ok(())
    }
    .await;
    for sql in ["PRAGMA temp_store = MEMORY", "PRAGMA journal_mode = WAL"] {
        sqlx::query(sql).execute(&mut *conn).await?;
    }
    rewrite?;
    let now = page_size(&mut conn).await?;
    if now != PAGE_SIZE {
        bail!("the inference cache is in {now}-byte pages after rewriting it in {PAGE_SIZE}");
    }
    tracing::info!(
        from = have,
        to = now,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "rewrote the inference cache in larger pages"
    );
    Ok(())
}

/// SQLite refuses to leave WAL, without asking the busy handler, while any
/// other connection has the file open, an idle one included. A reader that
/// is about to refuse the file holds it for a moment ([`require`]).
async fn leave_wal(conn: &mut SqliteConnection, wait: Duration) -> Result<()> {
    const RETRY: Duration = Duration::from_millis(100);
    let deadline = Instant::now() + wait;
    loop {
        match sqlx::query_scalar::<_, String>("PRAGMA journal_mode = DELETE")
            .fetch_one(&mut *conn)
            .await
        {
            Ok(mode) if mode.eq_ignore_ascii_case("delete") => return Ok(()),
            Ok(mode) => bail!("SQLite kept the inference cache in {mode} mode"),
            Err(e) if is_busy(&e) && Instant::now() < deadline => {
                tokio::time::sleep(RETRY).await;
            }
            Err(e) => {
                return Err(e).context("leave WAL mode, which needs no other connection open")
            }
        }
    }
}

/// Whether SQLite answered `SQLITE_BUSY`, in any of its extended forms.
fn is_busy(e: &sqlx::Error) -> bool {
    const SQLITE_BUSY: i32 = 5;
    e.as_database_error()
        .and_then(|d| d.code())
        .and_then(|code| code.parse::<i32>().ok())
        .is_some_and(|code| code & 0xff == SQLITE_BUSY)
}

pub struct InferenceCache {
    pub(crate) pool: SqlitePool,
}

impl InferenceCache {
    pub async fn open(path: &Path) -> Result<InferenceCache> {
        let pool = crate::file::open(path, &[], WAIT)
            .await
            .with_context(|| format!("open the inference cache at {}", path.display()))?;
        let ddl: Vec<&str> = DDL.iter().copied().chain(remade()).collect();
        let shape = shape();
        let kept = crate::file::keep_in_shape(
            &pool,
            StoreKind::InferenceCache,
            &shape,
            &ddl,
            crate::chunked::TABLES,
        );
        let set_up = async {
            kept.await?;
            to_page_size(&pool, WAIT).await
        };
        if let Err(e) = set_up.await {
            pool.close().await;
            return Err(e).with_context(|| format!("set up {}", path.display()));
        }
        Ok(InferenceCache { pool })
    }

    pub async fn close(self) {
        self.pool.close().await;
    }

    /// Of `hashes`, the ones `model` has no vector for, each once, in the
    /// order given.
    pub async fn missing(&self, model: &str, hashes: &[String]) -> Result<Vec<String>> {
        let mut held = HashSet::new();
        for batch in hashes.chunks(BATCH) {
            let found: Vec<String> = sqlx::query_scalar(HELD_SQL)
                .bind(model)
                .bind(serde_json::to_string(batch)?)
                .fetch_all(&self.pool)
                .await
                .context("read the cached hashes")?;
            held.extend(found);
        }
        let mut seen = HashSet::new();
        Ok(hashes
            .iter()
            .filter(|h| !held.contains(*h) && seen.insert(h.as_str()))
            .cloned()
            .collect())
    }

    pub async fn put(&self, model: &str, vectors: &[(String, Vec<f32>)]) -> Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        for (hash, vector) in vectors {
            sqlx::query("INSERT OR REPLACE INTO vectors (model, hash, vector) VALUES (?, ?, ?)")
                .bind(model)
                .bind(hash)
                .bind(to_blob(vector))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await.context("commit the vectors")
    }

    pub async fn get(&self, model: &str, hashes: &[String]) -> Result<HashMap<String, Vec<f32>>> {
        let mut out = HashMap::new();
        for batch in hashes.chunks(BATCH) {
            let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
                "SELECT hash, vector FROM vectors WHERE model = ? \
                   AND hash IN (SELECT value FROM json_each(?))",
            )
            .bind(model)
            .bind(serde_json::to_string(batch)?)
            .fetch_all(&self.pool)
            .await
            .context("read cached vectors")?;
            for (hash, blob) in rows {
                out.insert(hash, from_blob(&blob)?);
            }
        }
        Ok(out)
    }
}

fn to_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(blob: &[u8]) -> Result<Vec<f32>> {
    let (words, rest) = blob.as_chunks::<4>();
    if !rest.is_empty() {
        bail!("a cached vector of {} bytes, not whole f32s", blob.len());
    }
    Ok(words.iter().map(|w| f32::from_le_bytes(*w)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn open() -> (tempfile::TempDir, InferenceCache) {
        let dir = tempfile::tempdir().unwrap();
        let cache = InferenceCache::open(&dir.path().join("cache.sqlite"))
            .await
            .unwrap();
        (dir, cache)
    }

    fn h(s: &str) -> String {
        s.to_string()
    }

    #[tokio::test]
    async fn a_vector_is_found_under_its_model_and_hash_only() {
        let (_dir, cache) = open().await;
        cache
            .put("gemma", &[(h("a"), vec![1.0, -2.5])])
            .await
            .unwrap();
        assert_eq!(
            cache
                .missing("gemma", &[h("a"), h("b"), h("b")])
                .await
                .unwrap(),
            [h("b")]
        );
        assert_eq!(cache.missing("other", &[h("a")]).await.unwrap(), [h("a")]);
        let got = cache.get("gemma", &[h("a"), h("b")]).await.unwrap();
        assert_eq!(got, HashMap::from([(h("a"), vec![1.0, -2.5])]));
    }

    #[tokio::test]
    async fn the_cache_outlives_its_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.sqlite");
        let cache = InferenceCache::open(&path).await.unwrap();
        cache.put("gemma", &[(h("a"), vec![0.5])]).await.unwrap();
        cache.close().await;
        let cache = InferenceCache::open(&path).await.unwrap();
        assert!(cache.missing("gemma", &[h("a")]).await.unwrap().is_empty());
    }

    async fn plan(cache: &InferenceCache, sql: &str, binds: &[&str]) -> Vec<String> {
        // `sql` is one of this crate's own constant statements.
        let explain = sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}"));
        let mut query = sqlx::query_as::<_, (i64, i64, i64, String)>(explain);
        for bind in binds {
            query = query.bind(*bind);
        }
        query
            .fetch_all(&cache.pool)
            .await
            .unwrap()
            .into_iter()
            .map(|(_, _, _, detail)| detail)
            .collect()
    }

    /// Asking whether a vector is held reads the keys' index, not the
    /// table of 3 KB vectors: an embed of a large source spent four
    /// minutes on one random disk read per chunk before it did.
    #[tokio::test]
    async fn whether_a_vector_is_held_is_asked_of_the_keys_alone() {
        let (_dir, cache) = open().await;
        for (sql, binds) in [
            (HELD_SQL, ["gemma", "[]"]),
            (crate::chunked::OWED_SQL, ["mail", "gemma"]),
        ] {
            let plan = plan(&cache, sql, &binds).await;
            assert!(
                plan.iter()
                    .any(|d| d.contains("USING COVERING INDEX vectors_keys")),
                "{plan:?}"
            );
        }
    }

    /// The sweep finds the vectors no chunk names by reading the keys'
    /// index, and reads only those rows of the table: a sweep that read the
    /// whole table took 150 s on a 2.7 GB cache.
    #[tokio::test]
    async fn the_sweep_reads_the_keys_not_the_vectors() {
        let (_dir, cache) = open().await;
        let plan = plan(&cache, crate::chunked::SWEEP_SQL, &["gemma"]).await;
        assert!(
            plan.iter()
                .any(|d| d == "SCAN vectors USING COVERING INDEX vectors_keys"),
            "{plan:?}"
        );
        assert!(
            plan.iter()
                .all(|d| !d.starts_with("SCAN vectors") || d.contains("vectors_keys")),
            "the sweep reads the vectors table whole: {plan:?}"
        );
    }

    async fn pragma(cache: &InferenceCache, name: &str) -> String {
        // `name` is one of the pragmas below.
        let sql = sqlx::AssertSqlSafe(format!("SELECT CAST({name} AS TEXT) FROM pragma_{name}"));
        sqlx::query_scalar(sql)
            .fetch_one(&cache.pool)
            .await
            .unwrap()
    }

    /// A cache at `path` holding one vector, under "a", in SQLite's
    /// default 4 KB pages as an earlier build wrote it.
    async fn in_small_pages(path: &Path) -> Vec<f32> {
        let cache = InferenceCache::open(path).await.unwrap();
        let vector: Vec<f32> = (0..768).map(|i| i as f32).collect();
        cache
            .put("gemma", &[(h("a"), vector.clone())])
            .await
            .unwrap();
        for sql in [
            "PRAGMA journal_mode = DELETE",
            "PRAGMA page_size = 4096",
            "VACUUM",
            "PRAGMA journal_mode = WAL",
        ] {
            sqlx::query(sql).execute(&cache.pool).await.unwrap();
        }
        assert_eq!(pragma(&cache, "page_size").await, "4096", "set up");
        cache.close().await;
        assert!(in_other_pages(path).await.unwrap(), "set up");
        vector
    }

    async fn holds_in_large_pages(path: &Path, vector: Vec<f32>) {
        assert!(!in_other_pages(path).await.unwrap());
        let cache = InferenceCache::open(path).await.unwrap();
        assert_eq!(pragma(&cache, "page_size").await, PAGE_SIZE.to_string());
        assert_eq!(pragma(&cache, "journal_mode").await, "wal");
        let got = cache.get("gemma", &[h("a")]).await.unwrap();
        assert_eq!(got, HashMap::from([(h("a"), vector)]));
        cache.close().await;
    }

    #[tokio::test]
    async fn a_cache_in_small_pages_is_rewritten_keeping_every_vector() {
        let dir = tempfile::tempdir().unwrap();
        for opened_as_its_owner in [false, true] {
            let path = dir.path().join(format!("{opened_as_its_owner}.sqlite"));
            let vector = in_small_pages(&path).await;
            if opened_as_its_owner {
                InferenceCache::open(&path).await.unwrap().close().await;
            } else {
                rewrite_pages(&path).await.unwrap();
            }
            holds_in_large_pages(&path, vector).await;
        }
    }

    /// The search app opens the cache at launch, beside the prepare pass
    /// that rewrites it, and holds it for as long as it reads. It refuses
    /// small pages before it reads, so the rewrite finds no one there: a
    /// rewrite that waited for the app to close never ran.
    #[tokio::test]
    async fn a_reader_refuses_small_pages_and_leaves_them_to_be_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let (index, path) = (dir.path().join("db.sqlite"), dir.path().join("c.sqlite"));
        crate::store::Index::open(&index)
            .await
            .unwrap()
            .close()
            .await;
        let vector = in_small_pages(&path).await;

        let e = crate::Reader::open(&index, &path).await.err().unwrap();
        let refused = datalib_store_meta::other_shape(&e).expect("for its pages");
        assert_eq!(refused.step.as_deref(), Some(CACHE_STEP));

        rewrite_pages_within(&path, Duration::ZERO).await.unwrap();
        let reader = crate::Reader::open(&index, &path).await.unwrap();
        reader.expect("the index is started").close().await;
        holds_in_large_pages(&path, vector).await;
    }

    /// Another connection that has the file open fails the rewrite, which
    /// says so and leaves the cache as it was; once it has gone, the
    /// rewrite goes through.
    #[tokio::test]
    async fn a_rewrite_fails_while_another_connection_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.sqlite");
        let vector = in_small_pages(&path).await;
        let reader = crate::read::connect(&path).await.unwrap();
        let held: i64 = sqlx::query_scalar("SELECT count(*) FROM vectors")
            .fetch_one(&reader)
            .await
            .unwrap();
        assert_eq!(held, 1, "the reader reads the cache");

        let e = rewrite_pages_within(&path, Duration::ZERO).await;
        let e = format!("{:#}", e.unwrap_err());
        assert!(e.contains("needs no other connection open"), "{e}");
        assert!(in_other_pages(&path).await.unwrap());

        reader.close().await;
        rewrite_pages_within(&path, Duration::ZERO).await.unwrap();
        holds_in_large_pages(&path, vector).await;
    }

    /// A cache an earlier build wrote, with no keys' index, keeps every
    /// vector and gains the index when opened.
    #[tokio::test]
    async fn a_cache_from_before_the_keys_index_keeps_its_vectors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.sqlite");
        let cache = InferenceCache::open(&path).await.unwrap();
        cache.put("gemma", &[(h("a"), vec![0.5])]).await.unwrap();
        sqlx::query("DROP INDEX vectors_keys")
            .execute(&cache.pool)
            .await
            .unwrap();
        cache.close().await;
        let cache = InferenceCache::open(&path).await.unwrap();
        assert!(cache.missing("gemma", &[h("a")]).await.unwrap().is_empty());
        let indexed: Option<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE name = 'vectors_keys'")
                .fetch_optional(&cache.pool)
                .await
                .unwrap();
        assert_eq!(indexed.as_deref(), Some("vectors_keys"));
    }

    /// The vectors' shape is the one every library's cache records: a
    /// change to [`DDL`] moves this hash, and opening a cache in another
    /// shape empties it, which costs a library every embedding it holds.
    #[test]
    fn the_vectors_shape_is_the_one_libraries_hold() {
        assert_eq!(
            datalib_store_meta::schema_hash(DDL),
            "819354133eed18be9605e86cc206456aecbf3859ed4dde8722162cc048f748e6",
        );
    }
}
