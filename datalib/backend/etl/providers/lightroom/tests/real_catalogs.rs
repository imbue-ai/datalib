//! Stack four real Lightroom catalogs onto one doltlite store.
//!
//! The catalogs come from
//! [github.com/thadd3us/lightroom_db_diff](https://github.com/thadd3us/lightroom_db_diff),
//! fetched by Bazel (`http_file` in `MODULE.bazel`, pinned by commit sha
//! and sha256) rather than vendored — ~7 MB of binary shouldn't live in
//! this repo's history. They are a chronological progression of one
//! library:
//!
//! | # | Catalog | What the author changed |
//! | --- | --- | --- |
//! | 00 | `fresh` | the starting point |
//! | 01 | `gps_captions_collections_keywords` | GPS, captions, a collection, keywords |
//! | 02 | `two_more_photos_and_edits` | imported two photos, edited others |
//! | 03 | `more_face_tags_gps_edit` | more face tags, revised GPS |
//!
//! This is the test `mirror_roundtrip.rs` can't be: a synthetic fixture
//! shows the mechanism works, but only a real catalog shows that the
//! mechanism *pays* — that a day's worth of Lightroom editing touches 23
//! of 113 tables rather than all of them, and that four 1.7 MB catalogs
//! cost less stacked than stored side by side. It is also the only place
//! a second real Lightroom schema version (115 tables here, vs the
//! 133-table catalog the design was first checked against) gets
//! exercised.
//!
//! Tagged `requires-network`: once Bazel has fetched the catalogs the
//! test is hermetic and cached, but a cold cache has to reach github.com.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl::doltlite_raw as dr;
use datalib_etl::progress::Progress;
use datalib_etl_lightroom::download::{self, mirror, FetchOptions, MirrorOptions, MirrorStats};

/// Every table in these catalogs. Two of the 115 `sqlite_master` rows are
/// SQLite's own `sqlite_stat1` / `sqlite_stat4` query-planner statistics,
/// which the mirror skips along with everything else `sqlite_%`.
const CATALOG_TABLES: usize = 113;

/// How many of those 113 tables hold rows in catalog 00. Lightroom
/// creates the whole schema up front and most of it stays empty in a
/// small library (38, 46, 43, 45 non-empty across the four catalogs), so
/// the first commit's *schema* diff covers all 113 tables while its
/// *data* diff covers only these.
///
/// doltlite reported `data_change = 1` for a newly created empty table
/// through v0.11.51 and 0 from v0.11.52 on. 0 is the right answer —
/// there is no data to have changed — which is why this is 38 and not
/// [`CATALOG_TABLES`].
const CATALOG_TABLES_WITH_ROWS: i64 = 38;

/// The four catalogs in the order the author edited them. Bazel stages
/// each one and passes its path in the matching env var.
const SEQUENCE: &[(&str, &str)] = &[
    ("LIGHTROOM_CATALOG_00", "fresh"),
    ("LIGHTROOM_CATALOG_01", "gps_captions_collections_keywords"),
    ("LIGHTROOM_CATALOG_02", "two_more_photos_and_edits"),
    ("LIGHTROOM_CATALOG_03", "more_face_tags_gps_edit"),
];

struct Store {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl Store {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("entities.doltlite_db");
        Self { _dir: dir, path }
    }

    /// Mirror one catalog and commit, exactly as the CLI does.
    async fn ingest(&self, catalog: &Path) -> Result<(MirrorStats, Option<String>)> {
        let pool = mirror::open_mirror(&self.path).await?;
        let stats = download::fetch(FetchOptions {
            mirror_path: self.path.clone(),
            pool: Some(pool.clone()),
            options: MirrorOptions {
                source_path: catalog.to_path_buf(),
                snapshot: true,
                include_tables: vec!["*".to_string()],
                exclude_tables: Vec::new(),
                exclude_columns: Vec::new(),
                stable_key_columns: vec!["id_global".to_string()],
                primary_keys: BTreeMap::new(),
                gc: false,
            },
            progress: Progress::noop(),
        })
        .await?;
        let commit = dr::commit_run(&pool, &format!("lightroom: {}", stats.summary())).await?;
        pool.close().await;
        Ok((stats, commit))
    }

    async fn pool(&self) -> Result<SqlitePool> {
        mirror::open_sqlite(&self.path, false).await
    }

    fn bytes(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }
}

fn catalog(env_var: &str) -> PathBuf {
    let p = PathBuf::from(
        std::env::var(env_var)
            .unwrap_or_else(|_| panic!("{env_var} must point at a fetched .lrcat")),
    );
    assert!(p.exists(), "catalog missing at {}", p.display());
    p
}

async fn scalar_i64(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query(sql)
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get::<i64, _>(0)
}

/// Tables whose *data* changed in one commit.
async fn tables_changed(pool: &SqlitePool, commit: &str) -> i64 {
    scalar_i64(
        pool,
        &format!(
            "SELECT COUNT(*) FROM dolt_diff WHERE commit_hash = '{commit}' AND data_change = 1"
        ),
    )
    .await
}

/// `diff_type -> count` for the changes one commit introduced.
///
/// Filtered on `to_commit` alone. `dolt_diff_<table>` holds one row per
/// change between a commit and its parent, so also matching
/// `from_commit` would fold in the *next* commit's changes and roughly
/// double every count — a mistake worth naming, because the resulting
/// numbers still look plausible.
async fn diffs(pool: &SqlitePool, table: &str, commit: &str) -> BTreeMap<String, i64> {
    let sql = format!(
        "SELECT diff_type, COUNT(*) AS n FROM dolt_diff_{table} \
         WHERE to_commit = '{commit}' GROUP BY diff_type"
    );
    sqlx::query(&sql)
        .fetch_all(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .iter()
        .map(|r| (r.get::<String, _>("diff_type"), r.get::<i64, _>("n")))
        .collect()
}

fn counts(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
    pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

/// Ingest all four, in order, into one store. Returns the commit hash of
/// each run.
async fn ingest_sequence(store: &Store) -> Result<Vec<String>> {
    let mut commits = Vec::new();
    for (var, label) in SEQUENCE.iter() {
        let (stats, commit) = store.ingest(&catalog(var)).await?;
        assert_eq!(
            stats.tables, CATALOG_TABLES,
            "catalog {label} should mirror every table"
        );
        // No table disappears between these four catalogs, so nothing is
        // ever stale. (The per-run rebuild drops and recreates every
        // table by definition; `stale_tables_dropped` counts only tables
        // the *source* stopped having.)
        assert_eq!(stats.stale_tables_dropped, 0, "catalog {label}");
        commits.push(commit.unwrap_or_else(|| {
            panic!("catalog {label} differs from its predecessor, so it must commit")
        }));
    }
    Ok(commits)
}

// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn four_catalogs_stack_into_one_store_with_incremental_commits() -> Result<()> {
    let store = Store::new();
    let commits = ingest_sequence(&store).await?;
    let pool = store.pool().await?;

    // The first ingest necessarily touches every table that has anything
    // in it. Every later one touches only what the author actually
    // changed — this is the whole claim, measured against a real editing
    // session rather than a fixture.
    assert_eq!(
        tables_changed(&pool, &commits[0]).await,
        CATALOG_TABLES_WITH_ROWS
    );
    let incremental: Vec<i64> = vec![
        tables_changed(&pool, &commits[1]).await,
        tables_changed(&pool, &commits[2]).await,
        tables_changed(&pool, &commits[3]).await,
    ];
    assert_eq!(incremental, vec![32, 46, 23]);
    assert!(
        incremental.iter().all(|n| *n < CATALOG_TABLES as i64 / 2),
        "each incremental commit should touch well under half the catalog, got {incremental:?}"
    );

    // HEAD is the last catalog: 5 photos to start, 2 imported in #02.
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM Adobe_images").await,
        7
    );
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn each_commit_matches_what_its_catalog_is_named_after() -> Result<()> {
    // The catalogs' filenames say what the author changed. If the mirror
    // is faithful, the per-table diffs should say the same thing.
    let store = Store::new();
    let commits = ingest_sequence(&store).await?;
    let pool = store.pool().await?;

    // 01 — "gps_captions_collections_keywords": keywords and a
    // collection appear, and GPS lands on two photos (EXIF rows
    // modified, no new photos).
    let c = &commits[1];
    assert_eq!(
        diffs(&pool, "AgLibraryKeyword", c).await,
        counts(&[("added", 4)])
    );
    assert_eq!(
        diffs(&pool, "AgLibraryCollection", c).await,
        counts(&[("added", 2), ("removed", 1)])
    );
    assert_eq!(
        diffs(&pool, "AgHarvestedExifMetadata", c).await,
        counts(&[("modified", 2)]),
        "GPS edited on two photos, none imported"
    );
    assert_eq!(
        diffs(&pool, "Adobe_images", c).await,
        counts(&[("modified", 5)]),
        "no photo added by this catalog"
    );

    // 02 — "two_more_photos_and_edits": exactly two photos imported,
    // with their EXIF and IPTC rows arriving alongside.
    let c = &commits[2];
    assert_eq!(
        diffs(&pool, "Adobe_images", c).await,
        counts(&[("added", 2), ("modified", 5)]),
        "the two imported photos"
    );
    assert_eq!(
        diffs(&pool, "AgHarvestedExifMetadata", c).await,
        counts(&[("added", 2), ("modified", 2)])
    );
    assert_eq!(
        diffs(&pool, "AgLibraryIPTC", c).await,
        counts(&[("added", 2), ("modified", 3)])
    );

    // 03 — "more_face_tags_gps_edit": face tags added and GPS revised,
    // and — the point of the assertion — *no* photo added or removed.
    let c = &commits[3];
    assert_eq!(
        diffs(&pool, "Adobe_images", c).await,
        counts(&[("modified", 6)]),
        "a metadata-only session: nothing imported, nothing deleted"
    );
    assert_eq!(
        diffs(&pool, "AgLibraryFace", c).await,
        counts(&[("modified", 4)])
    );
    assert_eq!(
        diffs(&pool, "AgLibraryKeywordFace", c).await,
        counts(&[("added", 4)]),
        "the new face tags"
    );
    assert_eq!(
        diffs(&pool, "AgHarvestedExifMetadata", c).await,
        counts(&[("modified", 3)]),
        "the GPS edit"
    );
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn re_ingesting_the_newest_catalog_changes_nothing() -> Result<()> {
    let store = Store::new();
    ingest_sequence(&store).await?;
    let pool = store.pool().await?;
    let before = scalar_i64(&pool, "SELECT COUNT(*) FROM dolt_log").await;
    pool.close().await;

    // Deduplication on a real catalog, not a fixture: 358 rows across
    // 113 tables deleted and rewritten, and doltlite recognises every
    // one as already at HEAD.
    let (stats, commit) = store.ingest(&catalog(SEQUENCE[3].0)).await?;
    assert!(stats.rows > 300, "the run really did rewrite everything");
    assert_eq!(
        commit, None,
        "an unchanged catalog must not produce a commit"
    );

    let pool = store.pool().await?;
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM dolt_log").await,
        before
    );
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn stacking_costs_less_than_storing_the_catalogs_separately() -> Result<()> {
    let store = Store::new();
    ingest_sequence(&store).await?;

    let separate: u64 = SEQUENCE
        .iter()
        .map(|(v, _)| std::fs::metadata(catalog(v)).unwrap().len())
        .sum();
    let stacked = store.bytes();
    assert!(
        stacked < separate,
        "four catalogs stacked ({stacked} bytes) should cost less than four kept \
         side by side ({separate} bytes) — that is the point of the exercise"
    );
    Ok(())
}

#[tokio::test]
async fn every_earlier_state_is_still_queryable() -> Result<()> {
    let store = Store::new();
    let commits = ingest_sequence(&store).await?;
    let pool = store.pool().await?;

    // A photo present since catalog 00 accumulates one history row per
    // commit that touched it, and the whole chain stays readable.
    let id: String = sqlx::query("SELECT id_global FROM Adobe_images ORDER BY id_global LIMIT 1")
        .fetch_one(&pool)
        .await?
        .get("id_global");
    let versions = scalar_i64(
        &pool,
        &format!("SELECT COUNT(*) FROM dolt_history_Adobe_images WHERE id_global = '{id}'"),
    )
    .await;
    assert!(
        versions >= 2,
        "photo {id} should have more than one recorded version, got {versions}"
    );

    // Every commit is reachable and carries the run's summary.
    for c in &commits {
        assert_eq!(
            scalar_i64(
                &pool,
                &format!("SELECT COUNT(*) FROM dolt_log WHERE commit_hash = '{c}'")
            )
            .await,
            1
        );
    }
    pool.close().await;
    Ok(())
}
