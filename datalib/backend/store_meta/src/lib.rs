//! `_datalib_meta`: one table in every store, saying which build wrote
//! it and what shape it is in.
//!
//! Three facts, in order of use: the build (`datalib_version`,
//! `git_hash`) is what a downgrade guard reads and a bug report needs;
//! the shape (`schema_hash`, blake3 over the DDL the owner opened with,
//! and `index_hash` over the indexes it keeps apart from it) is what
//! detects a change and cannot be forgotten, because it is derived, and
//! what every reader checks before it reads a row ([`shape::require`]);
//! the ladder positions order migrations: `schema_version` on
//! the owner's ladder, `0` until a store has one, and
//! `shared_schema_version` on the framework's ladder for the tables every
//! store shares (`problems`). The owner writes on every open,
//! upserting only the rows whose value moved, so `written_at_utc` is when
//! this build first wrote the store rather than when it last opened it.
//! docs/dev/plans/completed/schema_migrations.md §3.1 is the design.

use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool};
use strum::{EnumString, IntoStaticStr, VariantArray};

pub mod guard;
pub mod ladder;
pub mod shape;
pub use guard::{inspect_root, refuse_if_newer, NewerBuild};
pub use ladder::{Ladder, Migration};
pub use shape::{other_shape, require, step_of, OtherShape, Shape};

pub const TABLE: &str = "_datalib_meta";

pub const DDL: &str = "CREATE TABLE IF NOT EXISTS _datalib_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    written_at_utc TEXT NOT NULL,
    tz_offset TEXT NULL
)";

/// What is stored in `git_hash` when the build cannot name its commit
/// (a bare `bazel run` outside the dev launcher). Read back as `None`.
const UNKNOWN: &str = "unknown";

/// Which of datalib's stores a file is. Stored as the snake_case word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
pub enum StoreKind {
    /// A source's entities and sync bookkeeping (`<group>/ingest/entities`).
    Raw,
    /// A source's render store (`<group>/render_markdown`).
    Render,
    /// A source's time series (`<group>/render_time_series`).
    TimeSeries,
    /// A calendar source's occurrences in a window of time
    /// (`<group>/render_occurrences`).
    Occurrences,
    /// Every source's time series rows, pointing at their stores
    /// (`unified_time_series_index/time_series_index`).
    TimeSeriesIndex,
    /// The grid index (`unified_grid_and_search_index/unified_grid_and_search_index`).
    GridIndex,
    /// Each chunk's vector under the model that made it and the hash of
    /// its prompt, and where each embedded document's chunks fall
    /// (`unified_grid_and_search_index/search_index`). Plain SQLite.
    InferenceCache,
    Feedback,
    /// `system/disk_stats.sqlite`: bytes under each tree of the root, and
    /// the free space on its disk, over time. Plain SQLite.
    DiskStats,
    /// `system/remote_media.doltlite_db`: what remote media a person
    /// let a document load, and the URLs fetched into the download CAS.
    RemoteMedia,
    /// `system/runs/runs.sqlite`: every run's step states, log and metrics.
    Runs,
    /// `system/supervisor.sqlite`: requests, steps turned off, and the loop's facts.
    Supervisor,
    /// `datalib_curated/datalib_contacts/contacts.doltlite_db`: contacts a person
    /// made and the handles they linked to them.
    Contacts,
    /// `datalib_curated/datalib_problem_mutes/mutes.doltlite_db`: the
    /// problem rows a person muted.
    ProblemMutes,
    /// `<step>/problems.sqlite`: the problems of a step that keeps no
    /// store of its own. Plain SQLite.
    StepProblems,
}

impl StoreKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
    /// Whether a store of this kind holds a `problems` table, in the shape
    /// its `shared_schema_version` names.
    pub fn holds_problems(self) -> bool {
        matches!(
            self,
            StoreKind::Raw
                | StoreKind::Render
                | StoreKind::TimeSeries
                | StoreKind::Occurrences
                | StoreKind::GridIndex
                | StoreKind::StepProblems
        )
    }
    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
enum Key {
    DatalibVersion,
    GitHash,
    DoltliteVersion,
    SchemaHash,
    IndexHash,
    SchemaVersion,
    SharedSchemaVersion,
    StoreKind,
}

impl Key {
    fn as_str(self) -> &'static str {
        self.into()
    }
}

/// What a store's `_datalib_meta` says, read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub datalib_version: String,
    /// `None` when the build that wrote it could not name its commit.
    pub git_hash: Option<String>,
    /// `None` when the writer was not linked against doltlite (a stock
    /// SQLite test build, or the plain-SQLite run store).
    pub doltlite_version: Option<String>,
    pub schema_hash: String,
    /// `None` for a store that keeps every index in `schema_hash`.
    pub index_hash: Option<String>,
    pub schema_version: u32,
    pub shared_schema_version: u32,
    /// `None` for a kind this build does not know.
    pub store_kind: Option<StoreKind>,
    /// The latest of the rows' stamps: when this build first wrote the
    /// store, or when its shape last moved.
    pub written_at_utc: String,
}

/// blake3 over the DDL statements in order, hex. Two builds that would
/// open a store with the same statements agree on it; any change to
/// any statement moves it.
pub fn schema_hash<'a>(ddl: impl IntoIterator<Item = &'a str>) -> String {
    let mut hasher = blake3::Hasher::new();
    for stmt in ddl {
        hasher.update(stmt.as_bytes());
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex().to_string()
}

/// Where a store stands on its two ladders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Versions {
    pub schema: u32,
    pub shared: u32,
}

/// Create the table if it is missing and bring every row up to what this
/// build would write: `shape` is the store as its owner just made it.
/// Returns whether any row changed, so an owner that commits can commit
/// exactly when there is something to commit.
pub async fn write(pool: &SqlitePool, kind: StoreKind, shape: &Shape) -> Result<bool> {
    sqlx::query(DDL)
        .execute(pool)
        .await
        .context("create _datalib_meta")?;
    let doltlite_version = doltlite_version(pool).await;
    let git_hash = datalib_runtime::build_id::git_hash().unwrap_or_else(|| UNKNOWN.to_string());
    let mut wanted: Vec<(Key, String)> = vec![
        (
            Key::DatalibVersion,
            datalib_runtime::build_id::DATALIB_VERSION.to_string(),
        ),
        (Key::GitHash, git_hash),
        (
            Key::DoltliteVersion,
            doltlite_version.unwrap_or_else(|| UNKNOWN.to_string()),
        ),
        (Key::SchemaHash, shape.tables.clone()),
        (Key::SchemaVersion, shape.versions.schema.to_string()),
        (Key::SharedSchemaVersion, shape.versions.shared.to_string()),
        (Key::StoreKind, kind.as_str().to_string()),
    ];
    if let Some(indexes) = &shape.indexes {
        wanted.push((Key::IndexHash, indexes.clone()));
    }
    let current = rows(pool).await?;
    let (now, tz_offset) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
    let mut changed = false;
    for (key, value) in &wanted {
        if current.get(key.as_str()).map(|(v, _)| v) == Some(value) {
            continue;
        }
        sqlx::query(
            "INSERT INTO _datalib_meta (key, value, written_at_utc, tz_offset) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
             written_at_utc = excluded.written_at_utc, tz_offset = excluded.tz_offset",
        )
        .bind(key.as_str())
        .bind(value)
        .bind(&now)
        .bind(&tz_offset)
        .execute(pool)
        .await
        .with_context(|| format!("write _datalib_meta.{}", key.as_str()))?;
        changed = true;
    }
    if shape.indexes.is_none() && current.contains_key(Key::IndexHash.as_str()) {
        sqlx::query("DELETE FROM _datalib_meta WHERE key = ?")
            .bind(Key::IndexHash.as_str())
            .execute(pool)
            .await
            .context("clear _datalib_meta.index_hash")?;
        changed = true;
    }
    Ok(changed)
}

/// What the store says about itself, or `None` for a store with no
/// `_datalib_meta` — one written before the table existed. Reads the
/// working set, which for a store an owner has opened is HEAD.
pub async fn read(pool: &SqlitePool) -> Result<Option<Meta>> {
    let mut conn = pool
        .acquire()
        .await
        .context("acquire to read _datalib_meta")?;
    read_in(&mut conn).await
}

/// [`read`] on one connection, inside whatever transaction it holds: a
/// reader that pins a snapshot checks the shape of that snapshot.
pub async fn read_in(conn: &mut sqlx::SqliteConnection) -> Result<Option<Meta>> {
    if !table_exists(&mut *conn).await? {
        return Ok(None);
    }
    let rows = rows(conn).await?;
    let get = |k: Key| rows.get(k.as_str()).map(|(v, _)| v.clone());
    let Some(datalib_version) = get(Key::DatalibVersion) else {
        return Ok(None);
    };
    let known = |v: Option<String>| v.filter(|s| s != UNKNOWN);
    let written_at_utc = rows
        .values()
        .map(|(_, at)| at.clone())
        .max()
        .unwrap_or_default();
    Ok(Some(Meta {
        datalib_version,
        git_hash: known(get(Key::GitHash)),
        doltlite_version: known(get(Key::DoltliteVersion)),
        schema_hash: get(Key::SchemaHash).unwrap_or_default(),
        index_hash: get(Key::IndexHash),
        schema_version: get(Key::SchemaVersion)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        shared_schema_version: get(Key::SharedSchemaVersion)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        store_kind: get(Key::StoreKind).and_then(|s| StoreKind::parse(&s)),
        written_at_utc,
    }))
}

async fn table_exists(conn: &mut sqlx::SqliteConnection) -> Result<bool> {
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(TABLE)
            .fetch_one(conn)
            .await
            .context("probe _datalib_meta")?;
    Ok(n > 0)
}

/// key → (value, written_at_utc).
async fn rows<'c, E>(conn: E) -> Result<std::collections::BTreeMap<String, (String, String)>>
where
    E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
{
    let rows = sqlx::query("SELECT key, value, written_at_utc FROM _datalib_meta")
        .fetch_all(conn)
        .await
        .context("read _datalib_meta")?;
    rows.into_iter()
        .map(|r| {
            Ok((
                r.try_get::<String, _>("key")?,
                (
                    r.try_get::<String, _>("value")?,
                    r.try_get::<String, _>("written_at_utc")?,
                ),
            ))
        })
        .collect()
}

/// `dolt_version()` is a scalar function only doltlite has; stock SQLite
/// errors on it, and that is the answer.
async fn doltlite_version(pool: &SqlitePool) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT dolt_version()")
        .fetch_one(pool)
        .await
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    async fn pool(path: &std::path::Path) -> SqlitePool {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true);
        SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .unwrap()
    }

    /// strum and the stored spelling agree, both ways, for every kind.
    #[test]
    fn store_kind_round_trips_through_its_spelling() {
        for kind in StoreKind::VARIANTS {
            assert_eq!(StoreKind::parse(kind.as_str()), Some(*kind));
        }
        assert_eq!(StoreKind::GridIndex.as_str(), "grid_index");
        assert_eq!(StoreKind::parse("wormhole"), None);
    }

    /// The hash is a function of the statements and their order, and of
    /// nothing else.
    #[test]
    fn schema_hash_moves_with_any_statement() {
        let a = schema_hash(["CREATE TABLE t (x INT)", "CREATE INDEX i ON t (x)"]);
        let same = schema_hash(["CREATE TABLE t (x INT)", "CREATE INDEX i ON t (x)"]);
        let reordered = schema_hash(["CREATE INDEX i ON t (x)", "CREATE TABLE t (x INT)"]);
        let widened = schema_hash(["CREATE TABLE t (x INT, y INT)", "CREATE INDEX i ON t (x)"]);
        assert_eq!(a, same);
        assert_ne!(a, reordered);
        assert_ne!(a, widened);
        assert_eq!(a.len(), 64);
    }

    /// A store with no table reads as `None`, not as an error: every
    /// store written before this crate existed is that store.
    #[tokio::test]
    async fn a_store_without_the_table_reads_as_none() {
        let td = tempfile::tempdir().unwrap();
        let p = pool(&td.path().join("s.db")).await;
        assert_eq!(read(&p).await.unwrap(), None);
    }

    /// The first write creates the rows; a second write by the same
    /// build with the same shape changes nothing and says so; a shape
    /// change rewrites only its row, and the stamps show which.
    #[tokio::test]
    async fn write_is_idempotent_and_a_shape_change_moves_only_its_row() {
        let td = tempfile::tempdir().unwrap();
        let p = pool(&td.path().join("s.db")).await;
        let hash1 = schema_hash(["CREATE TABLE t (x INT)"]);

        assert!(write(&p, StoreKind::Raw, &Shape::of_tables(hash1.clone()))
            .await
            .unwrap());
        let first = read(&p).await.unwrap().expect("rows");
        assert_eq!(
            first.datalib_version,
            datalib_runtime::build_id::DATALIB_VERSION
        );
        assert_eq!(first.schema_hash, hash1);
        assert_eq!(first.schema_version, 0);
        assert_eq!(first.store_kind, Some(StoreKind::Raw));
        // A test binary is not linked with a doltlite that answers
        // `dolt_version()` through this plain pool on every host; both
        // answers are legal, and the row exists either way.
        let stored: String =
            sqlx::query_scalar("SELECT value FROM _datalib_meta WHERE key = 'doltlite_version'")
                .fetch_one(&p)
                .await
                .unwrap();
        assert!(!stored.is_empty());

        assert!(!write(&p, StoreKind::Raw, &Shape::of_tables(hash1.clone()))
            .await
            .unwrap());
        assert_eq!(read(&p).await.unwrap().unwrap(), first);

        // Stamps are microsecond UTC, so make the second write land later.
        let stamps_before: Vec<(String, String)> =
            sqlx::query_as("SELECT key, written_at_utc FROM _datalib_meta ORDER BY key")
                .fetch_all(&p)
                .await
                .unwrap();
        let hash2 = schema_hash(["CREATE TABLE t (x INT, y INT)"]);
        assert!(write(
            &p,
            StoreKind::Raw,
            &Shape {
                tables: hash2.clone(),
                indexes: None,
                versions: Versions {
                    schema: 1,
                    shared: 0
                },
            }
        )
        .await
        .unwrap());
        let after = read(&p).await.unwrap().unwrap();
        assert_eq!(after.schema_hash, hash2);
        assert_eq!(after.schema_version, 1);
        let stamps_after: Vec<(String, String)> =
            sqlx::query_as("SELECT key, written_at_utc FROM _datalib_meta ORDER BY key")
                .fetch_all(&p)
                .await
                .unwrap();
        for ((k, before), (_, now)) in stamps_before.iter().zip(&stamps_after) {
            if k == "schema_hash" || k == "schema_version" {
                assert!(now >= before, "{k}: rewritten row carries a fresh stamp");
            } else {
                assert_eq!(before, now, "{k}: an unchanged row keeps its stamp");
            }
        }
    }

    /// A store that keeps indexes apart records their hash and reads it
    /// back in its shape, and loses the row when its owner keeps none
    /// apart.
    #[tokio::test]
    async fn the_index_hash_is_part_of_the_shape_read_back() {
        let td = tempfile::tempdir().unwrap();
        let p = pool(&td.path().join("s.db")).await;
        let with = Shape {
            tables: "t".into(),
            indexes: Some("i".into()),
            versions: Versions::default(),
        };
        write(&p, StoreKind::Render, &with).await.unwrap();
        assert_eq!(read(&p).await.unwrap().unwrap().shape(), with);
        let without = Shape {
            indexes: None,
            ..with
        };
        assert!(write(&p, StoreKind::Render, &without).await.unwrap());
        assert_eq!(read(&p).await.unwrap().unwrap().index_hash, None);
    }

    /// A kind or a hash written by a newer build reads as `None` /
    /// verbatim rather than as a guess.
    #[tokio::test]
    async fn a_newer_builds_spelling_is_none_not_a_guess() {
        let td = tempfile::tempdir().unwrap();
        let p = pool(&td.path().join("s.db")).await;
        write(&p, StoreKind::DiskStats, &Shape::of_tables("h".into()))
            .await
            .unwrap();
        sqlx::query("UPDATE _datalib_meta SET value = 'holodeck' WHERE key = 'store_kind'")
            .execute(&p)
            .await
            .unwrap();
        sqlx::query("UPDATE _datalib_meta SET value = 'unknown' WHERE key = 'git_hash'")
            .execute(&p)
            .await
            .unwrap();
        let m = read(&p).await.unwrap().unwrap();
        assert_eq!(m.store_kind, None);
        assert_eq!(m.git_hash, None);
    }
}
