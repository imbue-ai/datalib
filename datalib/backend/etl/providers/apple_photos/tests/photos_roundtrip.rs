//! What is Photos-shaped about mirroring `Photos.sqlite`, end to end.
//! The engine's own contract (unchanged source → no commit, diffs by
//! key, history across a rebuild) is covered in
//! `//datalib/backend/etl/sqlite_mirror:mirror_roundtrip`; these tests
//! cover the Core Data shapes that fixture has none of.

use std::path::{Path, PathBuf};

use anyhow::Result;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl::doltlite_raw as dr;
use datalib_etl::progress::Progress;
use datalib_etl_apple_photos::processor::mirror_options;
use datalib_etl_apple_photos_config::{ApplePhotosConfig, DATABASE_IN_BUNDLE};
use datalib_etl_sqlite_mirror::{mirror, MirrorOptions, MirrorStats};
use datalib_source_common::LocalPath;

/// A library bundle + mirror pair in a tempdir, with the fixture copied
/// in so tests can edit it the way Photos would.
struct Fixture {
    _dir: tempfile::TempDir,
    bundle: PathBuf,
    mirror: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = dir.path().join("Tng.photoslibrary");
        let db = bundle.join(DATABASE_IN_BUNDLE);
        std::fs::create_dir_all(db.parent().unwrap()).expect("mkdir database/");
        std::fs::copy(fixture_db(), &db).expect("stage library fixture");
        // A Bazel runfile is read-only and `fs::copy` keeps the mode; the
        // tests play Photos editing the library, so make it writable.
        let mut perms = std::fs::metadata(&db).expect("stat db").permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&db, perms).expect("make db writable");
        let mirror = dir.path().join("entities.doltlite_db");
        Self {
            _dir: dir,
            bundle,
            mirror,
        }
    }

    fn db(&self) -> PathBuf {
        self.bundle.join(DATABASE_IN_BUNDLE)
    }

    fn config(&self) -> ApplePhotosConfig {
        ApplePhotosConfig {
            library: Some(LocalPath {
                path: self.bundle.clone(),
            }),
            ..Default::default()
        }
    }

    fn options(&self) -> MirrorOptions {
        mirror_options(&self.config()).expect("options from a bundle path")
    }

    async fn ingest_with(&self, opts: MirrorOptions) -> Result<(MirrorStats, Option<String>)> {
        let pool = mirror::open_mirror(&self.mirror).await?;
        let stats = mirror::run(&pool, &opts, &Progress::noop()).await?;
        let commit = dr::commit_run(&pool, &format!("apple_photos: {}", stats.summary())).await?;
        pool.close().await;
        Ok((stats, commit))
    }

    async fn ingest(&self) -> Result<(MirrorStats, Option<String>)> {
        self.ingest_with(self.options()).await
    }

    async fn edit_library(&self, stmts: &[&str]) -> Result<()> {
        let pool = mirror::open_sqlite(&self.db(), false).await?;
        for s in stmts {
            // Test: `stmts` are literal library edits written by the test itself.
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

fn fixture_db() -> PathBuf {
    let p = std::env::var("APPLE_PHOTOS_TNG_DB")
        .expect("APPLE_PHOTOS_TNG_DB must point at the generated Photos.sqlite fixture");
    let p = PathBuf::from(p);
    assert!(p.exists(), "library fixture missing at {}", p.display());
    p
}

async fn scalar_i64(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .get::<i64, _>(0)
}

async fn table_names(pool: &SqlitePool) -> Vec<String> {
    let rows = sqlx::query(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
         AND name NOT IN ('sync_runs', 'sync_scope_state', 'sync_scope_config') ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .expect("list mirror tables");
    rows.iter().map(|r| r.get::<String, _>("name")).collect()
}

async fn columns(pool: &SqlitePool, table: &str) -> Vec<String> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA table_xinfo(\"{table}\")"
    )))
    .fetch_all(pool)
    .await
    .unwrap_or_else(|e| panic!("table_xinfo({table}): {e}"));
    rows.iter().map(|r| r.get::<String, _>("name")).collect()
}

async fn primary_key(pool: &SqlitePool, table: &str) -> Vec<String> {
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

async fn diff_types(pool: &SqlitePool, table: &str, commit: &str) -> Vec<String> {
    let sql = format!("SELECT diff_type FROM dolt_diff_{table} WHERE to_commit = ?");
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
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

/// What the default config leaves of the fixture's 11 tables + 1
/// virtual table + 3 shadow tables.
const MIRRORED_TABLES: &[&str] = &[
    "ZADDITIONALASSETATTRIBUTES",
    "ZASSET",
    "ZDETECTEDFACE",
    "ZGENERICALBUM",
    "Z_33ASSETS",
    "Z_RT_Asset_boundedByRect",
];

#[tokio::test]
async fn the_bundle_path_is_enough() {
    let f = Fixture::new();
    let opts = f.options();
    assert_eq!(opts.source_path, f.bundle.join(DATABASE_IN_BUNDLE));
    // Handing over the database itself is accepted too.
    let cfg = ApplePhotosConfig {
        library: Some(LocalPath { path: f.db() }),
        ..Default::default()
    };
    assert_eq!(mirror_options(&cfg).unwrap().source_path, f.db());
    assert!(mirror_options(&ApplePhotosConfig::default()).is_err());
}

/// The first run, with the defaults: history and daemon tables gone,
/// `Z_OPT` gone, the R-tree present as rows and its shadow tables not.
#[tokio::test]
async fn default_run_keeps_the_photos_and_drops_the_bookkeeping() -> Result<()> {
    let f = Fixture::new();
    let (stats, commit) = f.ingest().await?;
    assert!(commit.is_some());
    assert_eq!(stats.tables, MIRRORED_TABLES.len());
    assert_eq!(stats.shadow_tables_skipped, 3);
    assert_eq!(stats.virtual_tables_skipped, 0);
    // Z_OPT on each of the four entity tables that survive the table filter.
    assert_eq!(stats.columns_dropped, 4);

    let pool = f.mirror_pool().await?;
    assert_eq!(table_names(&pool).await, MIRRORED_TABLES);
    assert!(!columns(&pool, "ZASSET")
        .await
        .contains(&"Z_OPT".to_string()));
    assert_eq!(scalar_i64(&pool, "SELECT COUNT(*) FROM ZASSET").await, 4);
    assert_eq!(
        scalar_i64(&pool, "SELECT COUNT(*) FROM Z_RT_Asset_boundedByRect").await,
        2,
        "the R-tree's rows come through the virtual table"
    );
    pool.close().await;
    Ok(())
}

/// `ZUUID` is never declared UNIQUE, so the run vouches for it table by
/// table: the two tables where every row has one are keyed on it, and
/// the one with a NULL keeps `Z_PK` rather than failing the run.
#[tokio::test]
async fn zuuid_is_the_key_wherever_it_holds() -> Result<()> {
    let f = Fixture::new();
    let (stats, _) = f.ingest().await?;
    assert_eq!(stats.tables_restably_keyed, 2);

    let pool = f.mirror_pool().await?;
    assert_eq!(primary_key(&pool, "ZASSET").await, vec!["ZUUID"]);
    assert_eq!(primary_key(&pool, "ZGENERICALBUM").await, vec!["ZUUID"]);
    assert_eq!(primary_key(&pool, "ZDETECTEDFACE").await, vec!["Z_PK"]);
    assert_eq!(
        primary_key(&pool, "Z_33ASSETS").await,
        vec!["Z_33ALBUMS", "Z_3ASSETS"]
    );
    pool.close().await;
    Ok(())
}

/// The reason for keying on `ZUUID`: Photos renumbering `Z_PK` on a
/// library repair reads as four modified rows, not four removed and
/// four added.
#[tokio::test]
async fn a_z_pk_renumbering_is_a_modification_not_a_churn() -> Result<()> {
    let f = Fixture::new();
    f.ingest().await?;
    f.edit_library(&["UPDATE ZASSET SET Z_PK = Z_PK + 1000"])
        .await?;
    let (_, commit) = f.ingest().await?;
    let commit = commit.expect("a renumbering is a change");

    let pool = f.mirror_pool().await?;
    assert_eq!(
        diff_types(&pool, "ZASSET", &commit).await,
        vec!["modified"; 4]
    );
    pool.close().await;
    Ok(())
}

/// One favourite toggled in Photos is one modified row, and the
/// bookkeeping the daemons write alongside it is not a change at all.
#[tokio::test]
async fn a_favorite_toggle_is_one_modified_row_and_daemon_churn_is_nothing() -> Result<()> {
    let f = Fixture::new();
    f.ingest().await?;

    // What photoanalysisd does between two runs on an untouched library.
    f.edit_library(&[
        "INSERT INTO ACHANGE (Z_PK, Z_ENT, Z_OPT, ZCHANGETYPE, ZENTITY, ZENTITYPK, \
         ZTRANSACTIONID) VALUES (21, 3, 1, 1, 3, 1, 6)",
        "INSERT INTO ATRANSACTION (Z_PK, Z_ENT, Z_OPT, ZAUTHORTS, ZTIMESTAMP, ZAUTHOR) \
         VALUES (6, 2, 1, 2, 0.0, 'analysis')",
        "UPDATE Z_PRIMARYKEY SET Z_MAX = Z_MAX + 1",
        "UPDATE ZASSET SET Z_OPT = Z_OPT + 1",
    ])
    .await?;
    let (_, commit) = f.ingest().await?;
    assert!(commit.is_none(), "daemon bookkeeping alone is not a change");

    // Then the user favourites one photo. The trigger bumps Z_OPT too,
    // which is why Z_OPT has to be excluded rather than merely ignored.
    f.edit_library(&["UPDATE ZASSET SET ZFAVORITE = 1 WHERE Z_PK = 2"])
        .await?;
    let (_, commit) = f.ingest().await?;
    let commit = commit.expect("a favourite is a change");
    let pool = f.mirror_pool().await?;
    assert_eq!(diff_types(&pool, "ZASSET", &commit).await, vec!["modified"]);
    let row = sqlx::query(
        "SELECT from_ZFAVORITE, to_ZFAVORITE, to_ZUUID FROM dolt_diff_ZASSET \
         WHERE to_commit = ?",
    )
    .bind(&commit)
    .fetch_one(&pool)
    .await?;
    assert_eq!(row.get::<i64, _>("from_ZFAVORITE"), 0);
    assert_eq!(row.get::<i64, _>("to_ZFAVORITE"), 1);
    assert_eq!(
        row.get::<String, _>("to_ZUUID"),
        "1A2B3C4D-0002-4000-8000-000000000002"
    );
    pool.close().await;
    Ok(())
}

/// With `skip_history = false` the mirror is faithful: the change log
/// and the counters come across, and so does `Z_OPT`.
#[tokio::test]
async fn keep_history_mirrors_everything_but_the_shadow_tables() -> Result<()> {
    let f = Fixture::new();
    let cfg = ApplePhotosConfig {
        skip_history: false,
        ..f.config()
    };
    let (stats, _) = f.ingest_with(mirror_options(&cfg)?).await?;
    assert_eq!(stats.tables, 12);
    assert_eq!(stats.columns_dropped, 0);
    assert_eq!(stats.shadow_tables_skipped, 3);

    let pool = f.mirror_pool().await?;
    assert_eq!(scalar_i64(&pool, "SELECT COUNT(*) FROM ACHANGE").await, 20);
    assert!(columns(&pool, "ZASSET")
        .await
        .contains(&"Z_OPT".to_string()));
    assert!(!table_names(&pool)
        .await
        .iter()
        .any(|t| t.starts_with("Z_RT_Asset_boundedByRect_")));
    pool.close().await;
    Ok(())
}

/// The file inside the bundle is what the engine opens; nothing else in
/// the bundle is read.
#[test]
fn only_the_database_is_read_from_the_bundle() {
    assert_eq!(
        Path::new(DATABASE_IN_BUNDLE),
        Path::new("database/Photos.sqlite")
    );
}
