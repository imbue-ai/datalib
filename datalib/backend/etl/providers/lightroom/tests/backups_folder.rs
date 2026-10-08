//! A folder of Lightroom backups, built from the TNG catalog, mirrored
//! into one store: each backup a commit, oldest first, dated when it was
//! taken, and the live catalog on top when there is one.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use sqlx::sqlite::SqlitePool;

use datalib_etl::doltlite_raw as dr;
use datalib_etl::progress::Progress;
use datalib_etl::stop::StopFlag;
use datalib_etl_files::fingerprint_cache::FingerprintCache;
use datalib_etl_lightroom::ingest::sync::{self, SyncRun};
use datalib_etl_lightroom::ingest::{mirror, unpack, MirrorOptions};

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Backups")).unwrap();
        Self { dir }
    }

    fn backups(&self) -> PathBuf {
        self.dir.path().join("Backups")
    }

    fn store(&self) -> PathBuf {
        self.dir.path().join("entities.doltlite_db")
    }

    /// Write a backup into `Backups/<folder>/`: the TNG catalog with
    /// `edits` applied, zipped as Lightroom does when `zip` names the
    /// archive, else as a bare `.lrcat`.
    async fn backup(&self, folder: &str, catalog: &str, zip: Option<&str>, edits: &[&str]) {
        let scratch = tempfile::tempdir().unwrap();
        let lrcat = scratch.path().join(catalog);
        write_catalog(&lrcat, edits).await;

        let dest = self.backups().join(folder);
        std::fs::create_dir_all(&dest).unwrap();
        match zip {
            Some(zip_name) => {
                let mut w =
                    zip::ZipWriter::new(std::fs::File::create(dest.join(zip_name)).unwrap());
                let opts = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated);
                w.start_file(catalog, opts).unwrap();
                w.write_all(&std::fs::read(&lrcat).unwrap()).unwrap();
                w.finish().unwrap();
            }
            None => {
                std::fs::copy(&lrcat, dest.join(catalog)).unwrap();
            }
        }
    }

    /// The live catalog, next to the backups folder: the TNG catalog with
    /// `edits` applied, replacing what was there.
    async fn live(&self, edits: &[&str]) -> PathBuf {
        let path = self.dir.path().join("Live.lrcat");
        let _ = std::fs::remove_file(&path);
        write_catalog(&path, edits).await;
        path
    }

    async fn sync(&self, options: &MirrorOptions) -> Result<SyncRun> {
        self.sync_with(options, None).await
    }

    /// One sync: mirror what is new, report, and make the run's last
    /// commit, as the processor does.
    async fn sync_with(&self, options: &MirrorOptions, catalog: Option<&Path>) -> Result<SyncRun> {
        self.run(options, Some(&self.backups()), catalog).await
    }

    async fn sync_catalog(&self, options: &MirrorOptions, catalog: &Path) -> Result<SyncRun> {
        self.run(options, None, Some(catalog)).await
    }

    async fn run(
        &self,
        options: &MirrorOptions,
        backups: Option<&Path>,
        catalog: Option<&Path>,
    ) -> Result<SyncRun> {
        let pool = mirror::open_mirror(&self.store()).await?;
        let cache = FingerprintCache::open(&self.dir.path().join("fingerprints.sqlite")).await?;
        let inputs = sync::Inputs { backups, catalog };
        let run = sync::run(
            &pool,
            &cache,
            inputs,
            options,
            &Progress::noop(),
            &StopFlag::default(),
            "lightroom",
        )
        .await;
        if let Ok(run) = &run {
            dr::commit_run(&pool, &format!("download lightroom: {}", run.summary())).await?;
        }
        pool.close().await;
        run
    }

    async fn read(&self) -> SqlitePool {
        mirror::open_sqlite(&self.store(), false).await.unwrap()
    }
}

async fn write_catalog(path: &Path, edits: &[&str]) {
    std::fs::copy(fixture_catalog(), path).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms).unwrap();
    let pool = mirror::open_sqlite(path, false).await.unwrap();
    for e in edits {
        // Test: `edits` are literal catalog edits written by the test.
        sqlx::query(sqlx::AssertSqlSafe(*e))
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
}

fn fixture_catalog() -> PathBuf {
    PathBuf::from(
        std::env::var("SQLITE_MIRROR_TNG_CATALOG")
            .expect("SQLITE_MIRROR_TNG_CATALOG must point at the generated .lrcat fixture"),
    )
}

fn options() -> MirrorOptions {
    MirrorOptions {
        stable_key_columns: vec!["id_global".into()],
        ..MirrorOptions::new(PathBuf::new())
    }
}

/// `(message, date)` of every commit, oldest first, without the store's
/// initialization commit.
async fn log(pool: &SqlitePool) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT message, date FROM dolt_log WHERE message != 'Initialize data repository'",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    rows.reverse();
    rows
}

async fn head(pool: &SqlitePool) -> String {
    sqlx::query_scalar("SELECT commit_hash FROM dolt_log LIMIT 1")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn rating_of_picard(pool: &SqlitePool) -> Option<i64> {
    sqlx::query_scalar("SELECT rating FROM Adobe_images WHERE id_global = 'IMAGE-0101-PICARD'")
        .fetch_one(pool)
        .await
        .unwrap()
}

const RERATE: &str = "UPDATE Adobe_images SET rating = 1 WHERE id_global = 'IMAGE-0101-PICARD'";
const KEYWORD: &str = "INSERT INTO AgLibraryKeyword (id_local, id_global, lc_name, name) \
                       VALUES (9001, 'KEYWORD-9001-HOLODECK', 'holodeck', 'Holodeck')";

/// The folder's backups land oldest first, one commit each, dated at the
/// time in the folder's name — read in local time, stored as UTC — so
/// `dolt_log` and `dolt_history_` read as the catalog's own history.
#[tokio::test]
async fn each_backup_is_a_commit_dated_when_it_was_taken() -> Result<()> {
    let f = Fixture::new();
    // Written newest first, so directory order is not what orders them.
    f.backup(
        "2023-01-02 0800",
        "TngCatalog-v13.lrcat",
        Some("TngCatalog-v13.zip"),
        &[RERATE, KEYWORD],
    )
    .await;
    f.backup(
        "2022-06-15 1400 - before keywords",
        "TngCatalog-2.lrcat",
        None,
        &[RERATE],
    )
    .await;
    f.backup(
        "2021-03-01 0900",
        "TngCatalog.lrcat",
        Some("TngCatalog.lrcat.zip"),
        &[],
    )
    .await;

    let run = f.sync(&options()).await?;
    assert_eq!(
        run.mirrored,
        [
            "2021-03-01 0900",
            "2022-06-15 1400 - before keywords",
            "2023-01-02 0800"
        ]
    );
    assert!(run.problems.is_empty(), "{:?}", run.problems);

    let pool = f.read().await;
    let log = log(&pool).await;
    let backups: Vec<&(String, String)> = log
        .iter()
        .filter(|(m, _)| m.contains(": backup "))
        .collect();
    assert_eq!(backups.len(), 3, "{log:?}");
    // TZ=UTC+7 in the BUILD file: seven hours behind UTC.
    let dates: Vec<&str> = backups.iter().map(|(_, d)| d.as_str()).collect();
    assert_eq!(
        dates,
        [
            "2021-03-01 16:00:00",
            "2022-06-15 21:00:00",
            "2023-01-02 15:00:00"
        ]
    );
    let first_lines: Vec<&str> = backups
        .iter()
        .map(|(m, _)| m.lines().next().unwrap_or(""))
        .collect();
    assert_eq!(
        first_lines,
        [
            "download lightroom: backup 2021-03-01 0900/TngCatalog.lrcat.zip",
            "download lightroom: backup 2022-06-15 1400 - before keywords/TngCatalog-2.lrcat",
            "download lightroom: backup 2023-01-02 0800/TngCatalog-v13.zip",
        ]
    );

    let history: Vec<(String, Option<i64>)> = sqlx::query_as(
        "SELECT commit_date, rating FROM dolt_history_Adobe_images \
          WHERE id_global = 'IMAGE-0101-PICARD' ORDER BY commit_date",
    )
    .fetch_all(&pool)
    .await?;
    let ratings: Vec<Option<i64>> = history.iter().map(|(_, r)| *r).collect();
    assert_eq!(ratings.first(), Some(&Some(5)), "{history:?}");
    assert_eq!(ratings.last(), Some(&Some(1)), "{history:?}");

    let keywords: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM AgLibraryKeyword WHERE id_global = 'KEYWORD-9001-HOLODECK'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(keywords, 1, "HEAD is the newest backup");

    let ledger: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT snapshot, taken_at, file FROM lightroom_snapshots ORDER BY taken_at",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        ledger,
        [
            (
                "2021-03-01 0900".into(),
                "2021-03-01T09:00:00".into(),
                "2021-03-01 0900/TngCatalog.lrcat.zip".into()
            ),
            (
                "2022-06-15 1400 - before keywords".into(),
                "2022-06-15T14:00:00".into(),
                "2022-06-15 1400 - before keywords/TngCatalog-2.lrcat".into()
            ),
            (
                "2023-01-02 0800".into(),
                "2023-01-02T08:00:00".into(),
                "2023-01-02 0800/TngCatalog-v13.zip".into()
            ),
        ]
    );
    pool.close().await;
    Ok(())
}

/// A second sync over the same folder mirrors the newest backup again,
/// which changes nothing, so it commits nothing.
#[tokio::test]
async fn a_folder_with_nothing_new_commits_nothing() -> Result<()> {
    let f = Fixture::new();
    f.backup(
        "2021-03-01 0900",
        "TngCatalog.lrcat",
        Some("TngCatalog.lrcat.zip"),
        &[],
    )
    .await;
    f.sync(&options()).await?;
    let before = head(&f.read().await).await;

    let run = f.sync(&options()).await?;
    assert!(run.mirrored.is_empty());
    assert!(run.last.is_some(), "the newest is mirrored again");
    assert_eq!(run.backups_found, 1);
    assert_eq!(head(&f.read().await).await, before);
    Ok(())
}

/// HEAD left on an older state, as a run that failed or was stopped
/// after replaying an older backup leaves it, is put back on the newest
/// by the next sync, though the folder has nothing new.
#[tokio::test]
async fn a_head_left_behind_is_put_right_by_the_next_sync() -> Result<()> {
    let f = Fixture::new();
    f.backup(
        "2022-06-15 1400",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[RERATE],
    )
    .await;
    f.sync(&options()).await?;

    let scratch = tempfile::tempdir()?;
    let older = scratch.path().join("TngCatalog.lrcat");
    write_catalog(&older, &[]).await;
    let pool = mirror::open_mirror(&f.store()).await?;
    let keep_ledger = MirrorOptions {
        sidecar_tables: vec!["lightroom_snapshots".into()],
        ..options()
    };
    unpack::mirror_file(&pool, &older, &keep_ledger, &Progress::noop()).await?;
    dr::commit_run(&pool, "an older state, left as HEAD").await?;
    pool.close().await;
    assert_eq!(rating_of_picard(&f.read().await).await, Some(5));

    let run = f.sync(&options()).await?;
    assert!(run.mirrored.is_empty(), "{:?}", run.mirrored);
    let pool = f.read().await;
    assert_eq!(rating_of_picard(&pool).await, Some(1), "HEAD is 2022 again");
    pool.close().await;
    Ok(())
}

/// A backup that turns up older than the newest one committed is still
/// replayed, and the newest is then mirrored again, so HEAD ends on the
/// newest state. A newer backup is simply appended.
#[tokio::test]
async fn an_older_backup_is_replayed_and_the_newest_put_back_on_top() -> Result<()> {
    let f = Fixture::new();
    f.backup(
        "2022-06-15 1400",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[],
    )
    .await;
    f.sync(&options()).await?;

    f.backup(
        "2020-01-01 0000",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[KEYWORD],
    )
    .await;
    let run = f.sync(&options()).await?;
    assert_eq!(run.mirrored, ["2020-01-01 0000"]);
    assert!(run.problems.is_empty(), "{:?}", run.problems);

    let pool = f.read().await;
    let first_lines: Vec<String> = log(&pool)
        .await
        .into_iter()
        .filter_map(|(m, _)| m.lines().next().map(str::to_string))
        .filter(|l| l.contains(": backup "))
        .collect();
    assert_eq!(
        first_lines,
        [
            "download lightroom: backup 2022-06-15 1400/TngCatalog.zip",
            "download lightroom: backup 2020-01-01 0000/TngCatalog.zip",
            "download lightroom: backup 2022-06-15 1400/TngCatalog.zip, \
             mirrored again to put the newest back on top",
        ]
    );
    let keyword_history: Vec<String> = sqlx::query_scalar(
        "SELECT diff_type FROM dolt_diff_AgLibraryKeyword \
          WHERE coalesce(to_id_global, from_id_global) = 'KEYWORD-9001-HOLODECK'",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        keyword_history.len(),
        2,
        "added, then removed: {keyword_history:?}"
    );
    let keywords: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM AgLibraryKeyword WHERE id_global = 'KEYWORD-9001-HOLODECK'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(keywords, 0, "HEAD is the 2022 backup again");
    pool.close().await;

    f.backup(
        "2023-01-02 0800",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[RERATE],
    )
    .await;
    let run = f.sync(&options()).await?;
    assert_eq!(run.mirrored, ["2023-01-02 0800"]);
    let pool = f.read().await;
    let again = log(&pool)
        .await
        .iter()
        .filter(|(m, _)| m.contains("mirrored again"))
        .count();
    assert_eq!(
        again, 1,
        "the newest was committed last; nothing to put back"
    );
    assert_eq!(rating_of_picard(&pool).await, Some(1));
    pool.close().await;
    Ok(())
}

/// A sync mirrors a backup with the keys that backup's own catalog
/// declares as UNIQUE indexes, so a table with no PRIMARY KEY still diffs
/// by key.
#[tokio::test]
async fn a_backup_is_mirrored_with_the_keys_its_catalog_declares() -> Result<()> {
    let f = Fixture::new();
    f.backup(
        "2021-03-01 0900",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[
            "CREATE TABLE SyncedPayload (image INTEGER, payloadKey TEXT, payloadData TEXT)",
            "CREATE UNIQUE INDEX index_SyncedPayload_primaryKey ON SyncedPayload(image, payloadKey)",
            "INSERT INTO SyncedPayload VALUES (1,'a','x'),(2,'a','y')",
        ],
    )
    .await;
    f.sync(&options()).await?;

    let pool = f.read().await;
    let key: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM pragma_table_info('SyncedPayload') WHERE pk > 0 ORDER BY pk",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(key, ["image", "payloadKey"]);
    pool.close().await;
    Ok(())
}

/// A filter changed with no new backup to carry it reaches HEAD: every
/// sync ends by mirroring the newest backup again under the filters it
/// is given.
#[tokio::test]
async fn a_changed_filter_mirrors_the_newest_backup_again() -> Result<()> {
    let f = Fixture::new();
    f.backup(
        "2021-03-01 0900",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[],
    )
    .await;
    f.sync(&options()).await?;
    let pool = f.read().await;
    assert!(table_exists(&pool, "AgOzSpaceIds").await);
    pool.close().await;

    let narrowed = MirrorOptions {
        exclude_tables: vec!["AgOz*".into()],
        ..options()
    };
    let run = f.sync(&narrowed).await?;
    assert!(run.mirrored.is_empty(), "{:?}", run.mirrored);
    let pool = f.read().await;
    assert!(!table_exists(&pool, "AgOzSpaceIds").await);
    assert!(log(&pool).await.iter().any(|(m, _)| m.lines().next()
        == Some(
            "download lightroom: backup 2021-03-01 0900/TngCatalog.zip, \
             mirrored again to put the newest back on top"
        )));
    let before = head(&pool).await;
    pool.close().await;

    let run = f.sync(&narrowed).await?;
    assert!(run.mirrored.is_empty(), "{:?}", run.mirrored);
    assert_eq!(head(&f.read().await).await, before, "nothing new to commit");
    Ok(())
}

#[tokio::test]
async fn a_folder_with_no_backups_fails_the_run() {
    let f = Fixture::new();
    std::fs::create_dir(f.backups().join("not a backup")).unwrap();
    let err = f.sync(&options()).await.unwrap_err().to_string();
    assert!(err.contains("found no Lightroom backups"), "{err}");
}

async fn problem_keys(f: &Fixture) -> Vec<String> {
    let pool = f.read().await;
    let keys = sqlx::query_scalar("SELECT scope_key FROM problems ORDER BY scope_key")
        .fetch_all(&pool)
        .await
        .unwrap();
    pool.close().await;
    keys
}

/// A backup that will not mirror is a problem on that backup, the ones
/// around it still land, and HEAD ends on the newest that did. The next
/// sync tries it again, and once it mirrors the problem goes.
#[tokio::test]
async fn a_backup_that_will_not_mirror_holds_up_no_other() -> Result<()> {
    let f = Fixture::new();
    f.backup(
        "2021-03-01 0900",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[],
    )
    .await;
    f.backup(
        "2022-06-15 1400",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[RERATE],
    )
    .await;
    let newest = f.backups().join("2023-01-02 0800");
    std::fs::create_dir(&newest)?;
    std::fs::write(newest.join("TngCatalog.zip"), b"a zip the Borg got to")?;

    let run = f.sync(&options()).await?;
    assert_eq!(run.mirrored, ["2021-03-01 0900", "2022-06-15 1400"]);
    assert_eq!(
        problem_keys(&f).await,
        ["record:lightroom_snapshots:2023-01-02 0800"],
        "one row, though HEAD's turn would have tried it again"
    );
    let pool = f.read().await;
    assert_eq!(rating_of_picard(&pool).await, Some(1), "HEAD is 2022");
    pool.close().await;

    f.backup(
        "2023-01-02 0800",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[RERATE, KEYWORD],
    )
    .await;
    let run = f.sync(&options()).await?;
    assert_eq!(run.mirrored, ["2023-01-02 0800"]);
    assert_eq!(problem_keys(&f).await, Vec::<String>::new());
    Ok(())
}

/// When the newest backup cannot be put back on top, HEAD is an older
/// state and that is a problem on the newest — on every later sync too,
/// which keeps trying, until it lands.
#[tokio::test]
async fn a_newest_that_cannot_go_back_on_top_is_retried_until_it_does() -> Result<()> {
    let f = Fixture::new();
    f.backup(
        "2022-06-15 1400",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[RERATE],
    )
    .await;
    f.sync(&options()).await?;
    let away = f.dir.path().join("away");
    std::fs::rename(f.backups().join("2022-06-15 1400"), &away)?;
    f.backup(
        "2020-01-01 0000",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[],
    )
    .await;

    let run = f.sync(&options()).await?;
    assert_eq!(run.mirrored, ["2020-01-01 0000"]);
    let newest = ["record:lightroom_snapshots:2022-06-15 1400"];
    assert_eq!(problem_keys(&f).await, newest);
    let run = f.sync(&options()).await?;
    assert!(run.mirrored.is_empty());
    assert_eq!(problem_keys(&f).await, newest, "still behind, still said");

    std::fs::rename(&away, f.backups().join("2022-06-15 1400"))?;
    f.sync(&options()).await?;
    assert_eq!(problem_keys(&f).await, Vec::<String>::new());
    let pool = f.read().await;
    assert_eq!(rating_of_picard(&pool).await, Some(1), "HEAD is 2022 again");
    let before = head(&pool).await;
    pool.close().await;
    f.sync(&options()).await?;
    assert_eq!(
        head(&f.read().await).await,
        before,
        "and nothing more to commit"
    );
    Ok(())
}

/// The newest backup's folder deleted by hand: HEAD keeps that state
/// rather than going back to an older backup, and the missing backup is
/// a problem until a newer one arrives.
#[tokio::test]
async fn a_deleted_newest_backup_leaves_head_where_it_is() -> Result<()> {
    let f = Fixture::new();
    f.backup(
        "2021-03-01 0900",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[],
    )
    .await;
    f.backup(
        "2022-06-15 1400",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[RERATE],
    )
    .await;
    f.sync(&options()).await?;

    std::fs::remove_dir_all(f.backups().join("2022-06-15 1400"))?;
    let run = f.sync(&options()).await?;
    assert!(run.mirrored.is_empty(), "{:?}", run.mirrored);
    assert_eq!(
        problem_keys(&f).await,
        ["record:lightroom_snapshots:2022-06-15 1400"]
    );
    let pool = f.read().await;
    assert_eq!(rating_of_picard(&pool).await, Some(1), "HEAD is still 2022");
    pool.close().await;

    f.backup(
        "2023-01-02 0800",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[RERATE, KEYWORD],
    )
    .await;
    let run = f.sync(&options()).await?;
    assert_eq!(run.mirrored, ["2023-01-02 0800"]);
    assert_eq!(problem_keys(&f).await, Vec::<String>::new());
    Ok(())
}

/// A stopped run's problems are not the whole truth, so the last run's
/// stand.
#[tokio::test]
async fn a_stopped_run_leaves_the_last_runs_problems() -> Result<()> {
    let f = Fixture::new();
    f.backup(
        "2021-03-01 0900",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[],
    )
    .await;
    let bad = f.backups().join("2022-06-15 1400");
    std::fs::create_dir(&bad)?;
    std::fs::write(bad.join("TngCatalog.zip"), b"not a zip")?;
    f.sync(&options()).await?;
    let before = problem_keys(&f).await;
    assert_eq!(before, ["record:lightroom_snapshots:2022-06-15 1400"]);

    let pool = mirror::open_mirror(&f.store()).await?;
    let cache = FingerprintCache::open(&f.dir.path().join("fingerprints.sqlite")).await?;
    let stop = StopFlag::new();
    stop.request();
    let backups = f.backups();
    let run = sync::run(
        &pool,
        &cache,
        sync::Inputs {
            backups: Some(&backups),
            catalog: None,
        },
        &options(),
        &Progress::noop(),
        &stop,
        "lightroom",
    )
    .await?;
    assert!(run.stopped);
    dr::commit_run(&pool, "stopped").await?;
    pool.close().await;
    assert_eq!(problem_keys(&f).await, before);
    Ok(())
}

async fn table_exists(pool: &SqlitePool, name: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
        == 1
}

const RENAME: &str =
    "UPDATE AgLibraryKeyword SET name = 'Holodeck 3' WHERE id_global = 'KEYWORD-9001-HOLODECK'";

/// With both set, the backups are history and the live catalog is HEAD:
/// backups first, oldest first, then the catalog committed on top.
#[tokio::test]
async fn the_live_catalog_lands_on_top_of_the_backups() -> Result<()> {
    let f = Fixture::new();
    f.backup(
        "2021-03-01 0900",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[],
    )
    .await;
    f.backup(
        "2022-06-15 1400",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[RERATE],
    )
    .await;
    let live = f.live(&[RERATE, KEYWORD]).await;

    let run = f.sync_with(&options(), Some(&live)).await?;
    assert_eq!(run.mirrored, ["2021-03-01 0900", "2022-06-15 1400"]);
    assert!(run.live.is_some());

    let pool = f.read().await;
    let messages: Vec<String> = log(&pool).await.into_iter().map(|(m, _)| m).collect();
    let order: Vec<&str> = messages
        .iter()
        .filter_map(|m| {
            if m.contains(": backup ") {
                Some("backup")
            } else if m.lines().next().is_some_and(|l| l.ends_with("/Live.lrcat")) {
                Some("catalog")
            } else {
                None
            }
        })
        .collect();
    assert_eq!(order, ["backup", "backup", "catalog"], "{messages:?}");
    let keywords: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM AgLibraryKeyword WHERE id_global = 'KEYWORD-9001-HOLODECK'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(keywords, 1, "HEAD is the live catalog");
    let before = head(&pool).await;
    pool.close().await;

    // Nothing new anywhere: no commit.
    let run = f.sync_with(&options(), Some(&live)).await?;
    assert!(run.mirrored.is_empty());
    assert_eq!(head(&f.read().await).await, before);
    Ok(())
}

/// Backups that turn up after the live catalog was mirrored are replayed
/// whatever their dates, and the live catalog goes back on top.
#[tokio::test]
async fn late_backups_replay_under_the_live_catalog() -> Result<()> {
    let f = Fixture::new();
    f.backup(
        "2021-03-01 0900",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[],
    )
    .await;
    let live = f.live(&[KEYWORD]).await;
    f.sync_with(&options(), Some(&live)).await?;

    f.backup(
        "2023-05-05 0500",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[RERATE],
    )
    .await;
    f.backup(
        "2025-02-02 0200",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[KEYWORD, RENAME],
    )
    .await;
    let run = f.sync_with(&options(), Some(&live)).await?;
    assert_eq!(run.mirrored, ["2023-05-05 0500", "2025-02-02 0200"]);
    assert!(run.problems.is_empty(), "{:?}", run.problems);
    assert!(run.live.is_some(), "the unchanged catalog goes back on top");

    let pool = f.read().await;
    let name: String = sqlx::query_scalar(
        "SELECT name FROM AgLibraryKeyword WHERE id_global = 'KEYWORD-9001-HOLODECK'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(name, "Holodeck", "the live catalog, not a backup, is HEAD");
    assert_eq!(rating_of_picard(&pool).await, Some(5));
    pool.close().await;
    Ok(())
}

/// With a live catalog, a changed filter reaches HEAD through it: the
/// newest backup is not mirrored again.
#[tokio::test]
async fn a_changed_filter_reaches_head_through_the_live_catalog() -> Result<()> {
    let f = Fixture::new();
    f.backup(
        "2021-03-01 0900",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[],
    )
    .await;
    let live = f.live(&[]).await;
    f.sync_with(&options(), Some(&live)).await?;

    let narrowed = MirrorOptions {
        exclude_tables: vec!["AgOz*".into()],
        ..options()
    };
    let run = f.sync_with(&narrowed, Some(&live)).await?;
    assert!(run.mirrored.is_empty(), "{:?}", run.mirrored);
    let pool = f.read().await;
    assert!(!table_exists(&pool, "AgOzSpaceIds").await);
    pool.close().await;
    Ok(())
}

/// The catalog is mirrored on every sync; unchanged, it commits nothing.
/// An edit, or a changed filter, lands.
#[tokio::test]
async fn an_unchanged_catalog_commits_nothing() -> Result<()> {
    let f = Fixture::new();
    let live = f.live(&[]).await;
    let run = f.sync_catalog(&options(), &live).await?;
    assert!(run.live.is_some(), "the first sync mirrors it");
    let before = head(&f.read().await).await;

    let run = f.sync_catalog(&options(), &live).await?;
    assert!(run.live.is_some(), "mirrored again: {run:?}");
    assert_eq!(head(&f.read().await).await, before);

    let live = f.live(&[RERATE]).await;
    let run = f.sync_catalog(&options(), &live).await?;
    assert!(run.live.is_some(), "an edit is mirrored");
    assert_eq!(rating_of_picard(&f.read().await).await, Some(1));

    let narrowed = MirrorOptions {
        exclude_tables: vec!["AgOz*".into()],
        ..options()
    };
    let run = f.sync_catalog(&narrowed, &live).await?;
    assert!(run.live.is_some(), "a changed filter is mirrored");
    assert!(!table_exists(&f.read().await, "AgOzSpaceIds").await);
    Ok(())
}

#[tokio::test]
async fn a_missing_catalog_fails_the_run() {
    let f = Fixture::new();
    let gone = f.dir.path().join("Gone.lrcat");
    let err = f.sync_catalog(&options(), &gone).await.unwrap_err();
    assert!(format!("{err:#}").contains("Gone.lrcat"), "{err:#}");
}

/// A backup is known by its bytes. Renaming its folder changes nothing;
/// rewriting its file makes it a backup the store does not hold, so it
/// is replayed.
#[tokio::test]
async fn backups_are_known_by_their_bytes() -> Result<()> {
    let f = Fixture::new();
    f.backup(
        "2021-03-01 0900",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[],
    )
    .await;
    f.sync(&options()).await?;
    let before = head(&f.read().await).await;

    std::fs::rename(
        f.backups().join("2021-03-01 0900"),
        f.backups().join("2021-03-01 0900 - first backup"),
    )?;
    let run = f.sync(&options()).await?;
    assert!(
        run.mirrored.is_empty() && run.problems.is_empty(),
        "{run:?}"
    );
    assert_eq!(head(&f.read().await).await, before);

    std::fs::remove_dir_all(f.backups().join("2021-03-01 0900 - first backup"))?;
    f.backup(
        "2021-03-01 0900",
        "TngCatalog.lrcat",
        Some("TngCatalog.zip"),
        &[RERATE],
    )
    .await;
    let run = f.sync(&options()).await?;
    assert_eq!(run.mirrored, ["2021-03-01 0900"], "{run:?}");
    assert!(run.problems.is_empty(), "{run:?}");
    assert_eq!(rating_of_picard(&f.read().await).await, Some(1));
    Ok(())
}
