//! End-to-end tests for the Lightroom→doltlite mirror.
//!
//! The contract under test is the one that makes this an *incremental
//! backup* rather than a repeated full copy:
//!
//! 1. A first ingest lands every table and row (`first_ingest_*`).
//! 2. Re-ingesting an unchanged catalog produces **no commit at all** —
//!    the ingester deletes and rewrites every row, and doltlite's
//!    content-addressed storage recognises every one of them as already
//!    at HEAD (`unchanged_source_produces_no_commit`).
//! 3. Inserts, updates and deletes in the catalog show up in the mirror,
//!    and in `dolt_diff_<table>` with the right `diff_type`
//!    (`insert_update_and_delete_*`).
//! 4. History survives: after a row is edited and another deleted, the
//!    *earlier* values are still readable from `dolt_history_<table>`
//!    (`history_is_preserved_*`).
//!
//! Plus the two things the design notes claim and would otherwise be
//! unverified prose: that the stable-key rewrite turns an `id_local`
//! renumbering into a modification rather than a delete+add
//! (`id_local_renumbering_*`), and that source schema changes reconcile
//! (`source_gaining_a_column_*`, `source_dropping_a_column_*`).
//!
//! ## Why the fixture is built by Python
//!
//! Every Rust binary here links doltlite as its `sqlite3`. It reads and
//! writes plain SQLite files transparently — which is why these tests can
//! mutate the catalog with `sqlx` — but any file it *creates* is in
//! doltlite's own format. So the plain-SQLite catalog is minted by
//! `//tests/fixtures:make_lightroom_catalog.py` in a genrule and staged
//! as test data.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl::doltlite_raw as dr;
use datalib_etl::progress::Progress;
use datalib_etl_lightroom::download::{self, mirror, FetchOptions, MirrorOptions, MirrorStats};
use datalib_etl_lightroom_config::XMP_COLUMN_PATTERNS;

// ─────────────────────────────────────────────────────────────────────
// Harness
// ─────────────────────────────────────────────────────────────────────

/// A catalog + mirror pair in a tempdir, with the fixture already copied
/// in so tests can edit it freely.
struct Fixture {
    _dir: tempfile::TempDir,
    catalog: PathBuf,
    mirror: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let catalog = dir.path().join("TngCatalog.lrcat");
        std::fs::copy(fixture_catalog(), &catalog).expect("stage catalog fixture");
        // `fs::copy` carries the source's mode across, and a Bazel
        // runfile is read-only. These tests play the part of Lightroom
        // editing the library, so the staged copy has to be writable.
        let mut perms = std::fs::metadata(&catalog)
            .expect("stat catalog")
            .permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&catalog, perms).expect("make catalog writable");
        let mirror = dir.path().join("entities.doltlite_db");
        Self {
            _dir: dir,
            catalog,
            mirror,
        }
    }

    fn options(&self) -> MirrorOptions {
        MirrorOptions {
            source_path: self.catalog.clone(),
            snapshot: true,
            include_tables: vec!["*".to_string()],
            exclude_tables: Vec::new(),
            exclude_columns: Vec::new(),
            stable_key_columns: vec!["id_global".to_string()],
            primary_keys: BTreeMap::new(),
            gc: false,
        }
    }

    /// One ingest run: mirror, then commit exactly as the CLI and the
    /// orchestrator's `RawStoreSession::finish` do. `None` means nothing
    /// changed — that is the deduplication signal these tests turn on.
    async fn ingest_with(&self, opts: MirrorOptions) -> Result<(MirrorStats, Option<String>)> {
        let pool = mirror::open_mirror(&self.mirror).await?;
        let stats = download::fetch(FetchOptions {
            mirror_path: self.mirror.clone(),
            pool: Some(pool.clone()),
            options: opts,
            progress: Progress::noop(),
        })
        .await?;
        let commit = dr::commit_run(&pool, &format!("lightroom: {}", stats.summary())).await?;
        pool.close().await;
        Ok((stats, commit))
    }

    async fn ingest(&self) -> Result<(MirrorStats, Option<String>)> {
        self.ingest_with(self.options()).await
    }

    /// Run statements against the *catalog* — i.e. play the part of
    /// Lightroom editing the library. The pool is closed before
    /// returning so the next ingest gets an unlocked file.
    async fn edit_catalog(&self, stmts: &[&str]) -> Result<()> {
        let pool = mirror::open_sqlite(&self.catalog, false).await?;
        for s in stmts {
            // Test: `stmts` are literal catalog edits written by the test itself.
            sqlx::query(sqlx::AssertSqlSafe(*s))
                .execute(&pool)
                .await
                .map_err(|e| anyhow::anyhow!("{s}: {e}"))?;
        }
        pool.close().await;
        Ok(())
    }

    async fn mirror_pool(&self) -> Result<SqlitePool> {
        mirror::open_sqlite(&self.mirror, false).await
    }
}

/// The genrule-built catalog, staged as `data`.
fn fixture_catalog() -> PathBuf {
    let p = std::env::var("LIGHTROOM_TNG_CATALOG")
        .expect("LIGHTROOM_TNG_CATALOG must point at the generated .lrcat fixture");
    let p = PathBuf::from(p);
    assert!(p.exists(), "catalog fixture missing at {}", p.display());
    p
}

async fn scalar_i64(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get::<i64, _>(0)
}

async fn opt_string(pool: &SqlitePool, sql: &str) -> Option<String> {
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_optional(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .map(|r| r.get::<String, _>(0))
}

async fn commit_count(pool: &SqlitePool) -> i64 {
    scalar_i64(pool, "SELECT COUNT(*) FROM dolt_log").await
}

/// `diff_type`s recorded for `table` at commit `commit`, sorted.
async fn diff_types(pool: &SqlitePool, table: &str, commit: &str) -> Vec<String> {
    let sql =
        format!("SELECT diff_type FROM dolt_diff_{table} WHERE to_commit = ? OR from_commit = ?");
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
        .bind(commit)
        .bind(commit)
        .fetch_all(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let mut v: Vec<String> = rows
        .iter()
        .map(|r| r.get::<String, _>("diff_type"))
        .collect();
    v.sort();
    v
}

/// Column names present on a mirror table.
async fn mirror_columns(pool: &SqlitePool, table: &str) -> Vec<String> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA table_xinfo(\"{table}\")"
    )))
    .fetch_all(pool)
    .await
    .unwrap_or_else(|e| panic!("table_xinfo({table}): {e}"));
    rows.iter().map(|r| r.get::<String, _>("name")).collect()
}

/// A table's primary-key columns, in key order.
async fn mirror_pk(pool: &SqlitePool, table: &str) -> Vec<String> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA table_xinfo(\"{table}\")"
    )))
    .fetch_all(pool)
    .await
    .unwrap_or_else(|e| panic!("table_xinfo({table}): {e}"));
    let mut keyed: Vec<(i64, String)> = rows
        .iter()
        .map(|r| (r.get::<i64, _>("pk"), r.get::<String, _>("name")))
        .filter(|(pk, _)| *pk > 0)
        .collect();
    keyed.sort_by_key(|(pk, _)| *pk);
    keyed.into_iter().map(|(_, n)| n).collect()
}

const CATALOG_TABLES: &[&str] = &[
    "Adobe_AdditionalMetadata",
    "Adobe_images",
    "AgLibraryFile",
    "AgLibraryFolder",
    "AgLibraryImageChangeCounter",
    "AgLibraryKeyword",
    "AgLibraryKeywordImage",
    "AgMetadataSearchIndex",
    "AgOzSpaceIds",
    "MigrationSchemaVersion",
];

// ─────────────────────────────────────────────────────────────────────
// 1. First ingest
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn first_ingest_lands_every_table_and_row() -> Result<()> {
    let f = Fixture::new();
    let (stats, commit) = f.ingest().await?;

    assert_eq!(stats.tables, CATALOG_TABLES.len());
    assert_eq!(stats.columns_dropped, 0, "no filters configured");
    assert!(commit.is_some(), "a first ingest must produce a commit");

    let pool = f.mirror_pool().await?;
    for t in CATALOG_TABLES {
        let src = mirror::open_sqlite(&f.catalog, false).await?;
        let want = scalar_i64(&src, &format!("SELECT COUNT(*) FROM \"{t}\"")).await;
        src.close().await;
        let got = scalar_i64(&pool, &format!("SELECT COUNT(*) FROM \"{t}\"")).await;
        assert_eq!(got, want, "row count for {t}");
    }
    assert_eq!(
        stats.rows,
        4 + 4 + 4 + 2 + 3 + 5 + 4 + 2 + 1 + 4,
        "total rows mirrored"
    );
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn indexes_and_triggers_are_not_mirrored() -> Result<()> {
    let f = Fixture::new();
    f.ingest().await?;

    // `sqlite_autoindex_*` rows are excluded because they are the storage
    // engine's, not the source catalog's: doltlite 0.50 reports an
    // implicit index for every non-INTEGER primary key, the way stock
    // SQLite always has. It did not before 0.11.54's SQLite-compatibility
    // work, so this assertion used to be able to say "zero of anything".
    //
    // GLOB rather than LIKE: `_` is a single-character wildcard to LIKE,
    // and being sloppy about that in the pattern that decides what this
    // test ignores is how it would go quietly toothless.
    const NAMED_INDEXES_AND_TRIGGERS: &str = "SELECT COUNT(*) FROM sqlite_master \
           WHERE type IN ('index', 'trigger') \
             AND name NOT GLOB 'sqlite_autoindex_*'";

    // Prove the assertion below is not vacuous: the source really does
    // carry named indexes and a trigger for the mirror to drop.
    let src = mirror::open_sqlite(&f.catalog, false).await?;
    let in_source = scalar_i64(&src, NAMED_INDEXES_AND_TRIGGERS).await;
    src.close().await;
    assert!(
        in_source > 0,
        "the fixture catalog must define named indexes/triggers, else this \
         test cannot fail"
    );

    let pool = f.mirror_pool().await?;
    let n = scalar_i64(&pool, NAMED_INDEXES_AND_TRIGGERS).await;
    assert_eq!(
        n, 0,
        "the mirror keeps tables only — see lib.rs §What generic costs"
    );
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn dynamic_types_survive_the_copy() -> Result<()> {
    let f = Fixture::new();
    f.ingest().await?;
    let pool = f.mirror_pool().await?;
    // `xmp` is an untyped column holding a BLOB. A mirror that decided
    // untyped columns "are TEXT" would round-trip this as a string.
    let ty = opt_string(
        &pool,
        "SELECT typeof(xmp) FROM Adobe_AdditionalMetadata WHERE image = 101",
    )
    .await;
    assert_eq!(ty.as_deref(), Some("blob"));
    // …and a NULL stays NULL rather than becoming an empty string.
    let n = scalar_i64(
        &pool,
        "SELECT COUNT(*) FROM Adobe_images WHERE rating IS NULL",
    )
    .await;
    assert_eq!(n, 1, "the unrated image keeps its NULL rating");
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn large_values_round_trip_byte_for_byte() -> Result<()> {
    // The regression test for a doltlite bug that this provider walked
    // straight into (dolthub/doltlite#2327, fixed upstream in v0.11.53):
    // `INSERT … SELECT` from an ATTACHed plain SQLite file into a table
    // whose primary key is not a rowid alias silently corrupted every
    // value over ~4 KB, giving each row after the first the *first*
    // row's bytes truncated to its own length.
    //
    // Every ingredient matters, which is why it went unnoticed at first:
    // `Adobe_AdditionalMetadata` is keyed on `id_global` (a UUID, not a
    // rowid alias) precisely because of this provider's stable-key
    // rewrite, and its `xmp` packets are tens of KB. Row counts,
    // lengths, and `typeof()` all still come out right — only the bytes
    // are wrong — so nothing else in this file catches it.
    //
    // We carried a keyless-staging-table detour until the fix landed;
    // this test is what let us delete it, and what would catch the
    // shape coming back.
    let f = Fixture::new();
    f.ingest().await?;

    let src = mirror::open_sqlite(&f.catalog, false).await?;
    let want: Vec<(String, String)> = sqlx::query(
        "SELECT id_global, hex(xmp) AS h FROM Adobe_AdditionalMetadata ORDER BY id_global",
    )
    .fetch_all(&src)
    .await?
    .iter()
    .map(|r| (r.get::<String, _>("id_global"), r.get::<String, _>("h")))
    .collect();
    src.close().await;

    let pool = f.mirror_pool().await?;
    let got: Vec<(String, String)> = sqlx::query(
        "SELECT id_global, hex(xmp) AS h FROM Adobe_AdditionalMetadata ORDER BY id_global",
    )
    .fetch_all(&pool)
    .await?
    .iter()
    .map(|r| (r.get::<String, _>("id_global"), r.get::<String, _>("h")))
    .collect();

    assert!(
        want.iter().all(|(_, h)| h.len() / 2 > 4057),
        "the fixture's packets must exceed the 4057-byte corruption \
         threshold or this test proves nothing (largest: {} bytes)",
        want.iter().map(|(_, h)| h.len() / 2).max().unwrap_or(0)
    );
    assert_eq!(
        want.len(),
        got.len(),
        "same number of rows on both sides before comparing bytes"
    );
    for ((iw, hw), (ig, hg)) in want.iter().zip(got.iter()) {
        assert_eq!(iw, ig);
        assert_eq!(
            hw, hg,
            "photo {iw}'s XMP packet does not match the source byte for byte"
        );
    }
    pool.close().await;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// 2. Deduplication — the whole premise
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn unchanged_source_produces_no_commit() -> Result<()> {
    let f = Fixture::new();
    let (_, first) = f.ingest().await?;
    assert!(first.is_some());

    let pool = f.mirror_pool().await?;
    let before = commit_count(&pool).await;
    pool.close().await;

    // Second run rewrites every row of every table from an unchanged
    // catalog. Every row hashes to the chunk already at HEAD, so the
    // working tree comes back clean and there is nothing to commit.
    let (stats, second) = f.ingest().await?;
    assert!(stats.rows > 0, "the run really did rewrite every row");
    assert_eq!(stats.stale_tables_dropped, 0);
    // Note what this covers, now that every run drops and recreates
    // every table: it is not only "no row changed" but "no *schema*
    // changed either". A `CREATE TABLE` that differed in any way from
    // the one at HEAD — a stray NOT NULL, a type that didn't round-trip
    // — would dirty the tree and force a commit here. The earlier
    // shape-comparing design needed a separate assertion for that, and
    // still shipped a bug where ten of a real catalog's 133 tables
    // rebuilt on every run.
    assert_eq!(
        second, None,
        "an unchanged catalog must not produce a commit — that is the deduplication"
    );

    let pool = f.mirror_pool().await?;
    assert_eq!(commit_count(&pool).await, before, "dolt_log did not grow");
    pool.close().await;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// 3. Insert / update / delete
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn insert_update_and_delete_are_reflected_in_the_mirror() -> Result<()> {
    let f = Fixture::new();
    f.ingest().await?;

    f.edit_catalog(&[
        // INSERT: a new photo.
        "INSERT INTO Adobe_images (id_local, id_global, captureTime, fileFormat, fileWidth, \
         fileHeight, rating, rootFile) VALUES \
         (105, 'IMAGE-0105-CRUSHER', '2364-03-15T11:00:00-07:00', 'RAW', 6000, 4000, 2, 11)",
        // UPDATE: re-rate an existing one.
        "UPDATE Adobe_images SET rating = 1 WHERE id_global = 'IMAGE-0102-DATA'",
        // DELETE: remove one.
        "DELETE FROM Adobe_images WHERE id_global = 'IMAGE-0103-TROI'",
    ])
    .await?;

    let (_, commit) = f.ingest().await?;
    let commit = commit.expect("an edited catalog must produce a commit");

    let pool = f.mirror_pool().await?;

    // Row-level state.
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM Adobe_images").await,
        4,
        "4 originals + 1 inserted - 1 deleted"
    );
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM Adobe_images WHERE id_global = 'IMAGE-0105-CRUSHER'"
        )
        .await,
        1,
        "the inserted row is present"
    );
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT rating FROM Adobe_images WHERE id_global = 'IMAGE-0102-DATA'"
        )
        .await,
        1,
        "the update landed"
    );
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM Adobe_images WHERE id_global = 'IMAGE-0103-TROI'"
        )
        .await,
        0,
        "the deleted row is gone"
    );

    // …and dolt classified each edit correctly, which is what makes the
    // backup inspectable rather than merely correct.
    assert_eq!(
        diff_types(&pool, "Adobe_images", &commit).await,
        vec!["added", "modified", "removed"],
        "exactly one of each, and nothing else touched"
    );

    // Untouched tables contribute nothing to this commit.
    assert!(
        diff_types(&pool, "AgLibraryKeyword", &commit)
            .await
            .is_empty(),
        "a table the catalog didn't touch must not appear in the commit's diff"
    );
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn edits_to_a_keyless_table_are_reflected() -> Result<()> {
    // `AgOzSpaceIds` has no primary key and no unique index. doltlite
    // still versions it, by row multiset rather than by key — the honest
    // representation of a source table that has no identity either.
    let f = Fixture::new();
    f.ingest().await?;
    f.edit_catalog(&[
        "INSERT INTO AgOzSpaceIds (ozCatalogId, ozSpaceId, isPublic) \
         VALUES ('catalog-ncc-1701-d', 'space-gamma', 1)",
        "DELETE FROM AgOzSpaceIds WHERE ozSpaceId = 'space-beta'",
    ])
    .await?;

    let (_, commit) = f.ingest().await?;
    let commit = commit.expect("keyless edits must still commit");
    let pool = f.mirror_pool().await?;
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM AgOzSpaceIds").await,
        2
    );
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM AgOzSpaceIds WHERE ozSpaceId = 'space-gamma'"
        )
        .await,
        1
    );
    assert!(
        !diff_types(&pool, "AgOzSpaceIds", &commit).await.is_empty(),
        "the keyless table must appear in the commit's diff"
    );
    pool.close().await;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// 4. History
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn history_is_preserved_across_runs() -> Result<()> {
    let f = Fixture::new();
    let (_, c1) = f.ingest().await?;
    let c1 = c1.expect("first commit");

    f.edit_catalog(&[
        "UPDATE Adobe_images SET rating = 1 WHERE id_global = 'IMAGE-0102-DATA'",
        "DELETE FROM Adobe_images WHERE id_global = 'IMAGE-0103-TROI'",
    ])
    .await?;
    let (_, c2) = f.ingest().await?;
    let c2 = c2.expect("second commit");
    assert_ne!(c1, c2);

    f.edit_catalog(&["UPDATE Adobe_images SET rating = 0 WHERE id_global = 'IMAGE-0102-DATA'"])
        .await?;
    let (_, c3) = f.ingest().await?;
    let c3 = c3.expect("third commit");

    let pool = f.mirror_pool().await?;

    // Three ingests that changed something ⇒ three commits on top of
    // whatever the store's own initialisation left behind.
    for c in [&c1, &c2, &c3] {
        assert_eq!(
            scalar_i64(
                &pool,
                &format!("SELECT COUNT(*) FROM dolt_log WHERE commit_hash = '{c}'")
            )
            .await,
            1,
            "commit {c} is in the log"
        );
    }

    // The edited row's every prior value is still readable.
    let ratings = sqlx::query(
        "SELECT rating FROM dolt_history_Adobe_images \
         WHERE id_global = 'IMAGE-0102-DATA' ORDER BY rating",
    )
    .fetch_all(&pool)
    .await?;
    let ratings: Vec<i64> = ratings.iter().map(|r| r.get::<i64, _>("rating")).collect();
    assert_eq!(
        ratings,
        vec![0, 1, 4],
        "history holds the original 4, the edit to 1, and the edit to 0 — \
         HEAD alone would only show 0"
    );

    // The deleted row is gone from HEAD but not from history.
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM Adobe_images WHERE id_global = 'IMAGE-0103-TROI'"
        )
        .await,
        0
    );
    assert!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM dolt_history_Adobe_images WHERE id_global = 'IMAGE-0103-TROI'"
        )
        .await
            > 0,
        "the deleted photo's row is still recoverable from history — the point of the backup"
    );
    pool.close().await;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// 5. Key stability
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn stable_key_is_used_where_available_and_declared_key_elsewhere() -> Result<()> {
    let f = Fixture::new();
    let stats = f.ingest().await?.0;
    assert_eq!(
        stats.tables_restably_keyed, 5,
        "the five tables with id_global"
    );

    let pool = f.mirror_pool().await?;
    // Has `id_global UNIQUE NOT NULL` → keyed on it.
    assert_eq!(mirror_pk(&pool, "Adobe_images").await, vec!["id_global"]);
    // Has only `id_local INTEGER PRIMARY KEY` → keyed on that.
    assert_eq!(
        mirror_pk(&pool, "AgLibraryKeywordImage").await,
        vec!["id_local"]
    );
    // Has neither → keyless.
    assert!(mirror_pk(&pool, "AgOzSpaceIds").await.is_empty());
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn id_local_renumbering_is_a_modification_not_a_churn() -> Result<()> {
    // The scenario the `stable_key_columns` knob exists for: Lightroom
    // renumbers `id_local` (a rowid alias) on a catalog upgrade or
    // optimize, without any photo actually changing.
    let f = Fixture::new();
    f.ingest().await?;

    f.edit_catalog(&[
        "UPDATE Adobe_images SET id_local = id_local + 500",
        "UPDATE Adobe_AdditionalMetadata SET id_local = id_local + 500",
    ])
    .await?;
    let (_, commit) = f.ingest().await?;
    let commit = commit.expect("a renumbering does change stored bytes");

    let pool = f.mirror_pool().await?;
    let types = diff_types(&pool, "Adobe_images", &commit).await;
    assert_eq!(
        types,
        vec!["modified"; 4],
        "keyed on id_global, a renumbering is 4 one-column modifications; \
         keyed on id_local it would be 4 removals plus 4 additions"
    );
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM Adobe_images").await,
        4
    );
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn declared_keys_mode_keys_on_id_local() -> Result<()> {
    let f = Fixture::new();
    let mut opts = f.options();
    opts.stable_key_columns.clear();
    let stats = f.ingest_with(opts).await?.0;
    assert_eq!(stats.tables_restably_keyed, 0);

    let pool = f.mirror_pool().await?;
    assert_eq!(mirror_pk(&pool, "Adobe_images").await, vec!["id_local"]);
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn a_non_integer_primary_key_mirrors_as_the_key() -> Result<()> {
    // `MigrationSchemaVersion(version TEXT PRIMARY KEY)` — a key that is
    // not a rowid alias, so SQLite calls it nullable and dolt does not.
    let f = Fixture::new();
    f.ingest().await?;
    let pool = f.mirror_pool().await?;
    assert_eq!(
        mirror_pk(&pool, "MigrationSchemaVersion").await,
        vec!["version"]
    );
    assert_eq!(
        mirror_pk(&pool, "AgLibraryImageChangeCounter").await,
        vec!["image"]
    );
    assert_eq!(
        opt_string(&pool, "SELECT version FROM MigrationSchemaVersion").await,
        Some("11.0".to_string())
    );
    pool.close().await;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// 6. Filters
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn skip_xmp_removes_the_column_rather_than_blanking_it() -> Result<()> {
    let f = Fixture::new();
    let mut opts = f.options();
    opts.exclude_columns = XMP_COLUMN_PATTERNS.iter().map(|s| s.to_string()).collect();
    let (stats, _) = f.ingest_with(opts).await?;

    // Adobe_AdditionalMetadata.xmp + AgMetadataSearchIndex's two indexes.
    assert_eq!(stats.columns_dropped, 3);

    let pool = f.mirror_pool().await?;
    let cols = mirror_columns(&pool, "Adobe_AdditionalMetadata").await;
    assert!(
        !cols.contains(&"xmp".to_string()),
        "xmp is absent, not empty"
    );
    assert!(
        cols.contains(&"internalXmpDigest".to_string()),
        "its neighbours survive"
    );
    // Rows are still there — only the one column went.
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM Adobe_AdditionalMetadata").await,
        4
    );
    let search = mirror_columns(&pool, "AgMetadataSearchIndex").await;
    assert!(!search.contains(&"searchIndex".to_string()));
    assert!(!search.contains(&"exifSearchIndex".to_string()));
    assert!(search.contains(&"image".to_string()));
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn table_filters_select_what_is_mirrored() -> Result<()> {
    let f = Fixture::new();
    let mut opts = f.options();
    opts.include_tables = vec!["Ag*".to_string()];
    opts.exclude_tables = vec!["*Oz*".to_string()];
    let (stats, _) = f.ingest_with(opts).await?;
    assert_eq!(stats.tables, 6, "the six Ag* tables that aren't Oz");

    let pool = f.mirror_pool().await?;
    let mut names: Vec<String> = sqlx::query(
        // `sync_%` is the raw store's own bookkeeping (created by
        // `doltlite_raw::open`); `sqlite_%` is SQLite's internal
        // `sqlite_sequence`, which the AUTOINCREMENT in that bookkeeping
        // brings along. Neither is mirrored content.
        "SELECT name FROM sqlite_master WHERE type = 'table' \
         AND name NOT LIKE 'sync_%' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(&pool)
    .await?
    .iter()
    .map(|r| r.get::<String, _>("name"))
    .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "AgLibraryFile",
            "AgLibraryFolder",
            "AgLibraryImageChangeCounter",
            "AgLibraryKeyword",
            "AgLibraryKeywordImage",
            "AgMetadataSearchIndex",
        ]
    );
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn a_table_dropped_from_the_selection_is_dropped_from_the_mirror() -> Result<()> {
    let f = Fixture::new();
    f.ingest().await?;
    let mut opts = f.options();
    opts.exclude_tables = vec!["AgOzSpaceIds".to_string()];
    let (stats, _) = f.ingest_with(opts).await?;
    assert_eq!(stats.stale_tables_dropped, 1);

    let pool = f.mirror_pool().await?;
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'AgOzSpaceIds'"
        )
        .await,
        0,
        "HEAD reflects the catalog as it is now"
    );
    pool.close().await;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// 7. Schema evolution
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn source_gaining_a_column_is_mirrored_with_history_intact() -> Result<()> {
    let f = Fixture::new();
    f.ingest().await?;
    f.edit_catalog(&[
        "ALTER TABLE Adobe_images ADD COLUMN aiEditStatus",
        "UPDATE Adobe_images SET aiEditStatus = 'clean' WHERE id_global = 'IMAGE-0101-PICARD'",
    ])
    .await?;

    let (_, commit) = f.ingest().await?;
    assert!(commit.is_some());

    let pool = f.mirror_pool().await?;
    assert!(mirror_columns(&pool, "Adobe_images")
        .await
        .contains(&"aiEditStatus".to_string()));
    assert_eq!(
        opt_string(
            &pool,
            "SELECT aiEditStatus FROM Adobe_images WHERE id_global = 'IMAGE-0101-PICARD'"
        )
        .await
        .as_deref(),
        Some("clean")
    );
    // The table was dropped and rebuilt to gain the column, and the
    // rows' earlier versions are still in history regardless.
    assert!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM dolt_history_Adobe_images WHERE id_global = 'IMAGE-0101-PICARD'"
        )
        .await
            >= 2
    );
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn source_dropping_a_column_is_mirrored() -> Result<()> {
    let f = Fixture::new();
    f.ingest().await?;
    // (`parentId` rather than one of the indexed columns: SQLite itself
    // refuses to drop a column an index still references, so that
    // variant would fail in the fixture rather than in the mirror.)
    f.edit_catalog(&["ALTER TABLE AgLibraryFolder DROP COLUMN parentId"])
        .await?;

    let (_, commit) = f.ingest().await?;
    assert!(commit.is_some());

    let pool = f.mirror_pool().await?;
    let cols = mirror_columns(&pool, "AgLibraryFolder").await;
    assert!(!cols.contains(&"parentId".to_string()));
    assert!(cols.contains(&"pathFromRoot".to_string()));
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM AgLibraryFolder").await,
        2,
        "the rows are re-copied under the new shape"
    );
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn a_table_the_source_dropped_is_dropped_from_the_mirror() -> Result<()> {
    // The counterpart to discovery, and the case the rebuild loop alone
    // does NOT cover: that loop only visits tables the source still has,
    // so a table the catalog dropped would otherwise sit frozen at HEAD
    // forever. `drop_stale_tables` is the separate pass that catches it.
    //
    // Distinct from `a_table_dropped_from_the_selection_*`, which
    // exercises the same pass via the `exclude_tables` filter: this one
    // drops the table from the catalog itself, which is what actually
    // happens on a Lightroom upgrade.
    let f = Fixture::new();
    let (before, _) = f.ingest().await?;
    let first = f.ingest().await?;
    assert_eq!(first.1, None, "sanity: the second ingest is a no-op");

    f.edit_catalog(&["DROP TABLE AgOzSpaceIds"]).await?;

    let (after, commit) = f.ingest().await?;
    assert_eq!(after.tables, before.tables - 1);
    assert_eq!(
        after.stale_tables_dropped, 1,
        "the table the catalog dropped must be dropped from the mirror too"
    );
    assert!(commit.is_some());

    let pool = f.mirror_pool().await?;
    assert_eq!(
        scalar_i64(
            &pool,
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'AgOzSpaceIds'"
        )
        .await,
        0,
        "HEAD reflects the catalog as it is now"
    );
    // Every other table is untouched — dropping one must not disturb the rest.
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM Adobe_images").await,
        4
    );
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn a_table_the_source_gained_is_discovered_and_mirrored() -> Result<()> {
    // The other half of "handles evolving schemas": a Lightroom upgrade
    // adds tables as well as columns. Discovery is per-run and
    // unconditional, so a new table needs no configuration — it simply
    // shows up.
    let f = Fixture::new();
    let (before, _) = f.ingest().await?;

    f.edit_catalog(&[
        "CREATE TABLE AgLibraryImageCullingScore (\
             id_local INTEGER PRIMARY KEY, \
             image INTEGER NOT NULL DEFAULT 0, \
             score DEFAULT 0)",
        "INSERT INTO AgLibraryImageCullingScore (id_local, image, score) \
         VALUES (401, 101, 0.87), (402, 102, 0.42)",
    ])
    .await?;

    let (after, commit) = f.ingest().await?;
    assert_eq!(after.tables, before.tables + 1);
    assert_eq!(after.stale_tables_dropped, 0);
    assert!(commit.is_some());

    let pool = f.mirror_pool().await?;
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM AgLibraryImageCullingScore").await,
        2
    );
    assert_eq!(
        mirror_pk(&pool, "AgLibraryImageCullingScore").await,
        vec!["id_local"],
        "no id_global, so it keeps its declared key"
    );
    // The pre-existing tables kept their history across the discovery.
    assert!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM dolt_history_Adobe_images").await >= 4,
        "the four photos are still in history"
    );
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn a_dropped_columns_values_survive_at_their_commit() -> Result<()> {
    // When the source drops a column, HEAD stops having it, and
    // `dolt_history_` / `dolt_diff_` project rows through HEAD's schema
    // — so the column is absent from *those* views too. It is NOT gone:
    // branching at an earlier commit restores the old schema and the old
    // values.
    //
    // This test exists because the design notes assert that, and an
    // unverified "don't worry, it's still in history" is exactly the
    // kind of claim that is comfortable to believe and expensive to be
    // wrong about. It cannot pass vacuously: the recovery SELECT names a
    // column HEAD does not have, so a checkout that silently did nothing
    // would fail the query rather than the assertion.
    let f = Fixture::new();
    let (_, before) = f.ingest().await?;
    let before = before.expect("first commit");

    f.edit_catalog(&["ALTER TABLE AgLibraryFolder DROP COLUMN parentId"])
        .await?;
    f.ingest().await?;

    let pool = f.mirror_pool().await?;
    // Gone from HEAD, as it should be — HEAD mirrors the catalog as it is.
    assert!(!mirror_columns(&pool, "AgLibraryFolder")
        .await
        .contains(&"parentId".to_string()));

    // …and recoverable by branching at the earlier commit. Both
    // statements must run on one connection: doltlite's active branch is
    // per-connection.
    let mut conn = pool.acquire().await?;
    sqlx::query("SELECT dolt_branch(?, ?)")
        .bind("before_drop")
        .bind(&before)
        .execute(&mut *conn)
        .await?;
    sqlx::query("SELECT dolt_checkout(?)")
        .bind("before_drop")
        .execute(&mut *conn)
        .await?;
    let recovered: i64 = sqlx::query(
        "SELECT parentId FROM AgLibraryFolder WHERE id_global = 'FOLDER-0002-TENFORWARD'",
    )
    .fetch_one(&mut *conn)
    .await
    .expect("the dropped column reads back on the pre-drop branch")
    .get(0);
    assert_eq!(recovered, 1, "the value the catalog held before the drop");

    // Leave the connection on `main` so the pool isn't handed back
    // pointing at the recovery branch.
    sqlx::query("SELECT dolt_checkout(?)")
        .bind("main")
        .execute(&mut *conn)
        .await?;
    drop(conn);
    pool.close().await;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// 8. Snapshot
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn snapshot_yields_the_same_mirror_as_reading_in_place() -> Result<()> {
    let a = Fixture::new();
    let (with_snapshot, _) = a.ingest().await?;

    let b = Fixture::new();
    let mut opts = b.options();
    opts.snapshot = false;
    let (in_place, _) = b.ingest_with(opts).await?;

    assert_eq!(with_snapshot.tables, in_place.tables);
    assert_eq!(with_snapshot.rows, in_place.rows);
    Ok(())
}

#[tokio::test]
async fn snapshot_is_a_separate_readable_copy() -> Result<()> {
    let f = Fixture::new();
    let snap = mirror::snapshot(&f.catalog).await?;
    assert!(snap.is_copy());
    assert_ne!(snap.path(), f.catalog.as_path());
    let pool = mirror::open_sqlite(snap.path(), false).await?;
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM Adobe_images").await,
        4
    );
    pool.close().await;

    let path: PathBuf = snap.path().to_path_buf();
    drop(snap);
    assert!(
        !Path::new(&path).exists(),
        "the snapshot cleans up after itself"
    );
    Ok(())
}
