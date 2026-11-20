//! The prepare verb (`--prepare`), sent each time the runner starts: bring
//! what this step wrote to this build's shape where that can be done in
//! place, fetching nothing; say when it cannot (`needs_rerun`); and refuse,
//! as failure kind `incompatible`, a store a newer build wrote
//! (`docs/dev/step_protocol.md` § Verbs). The runner asks at every
//! start, so a store this build has already opened costs a read of its
//! `_datalib_meta` and nothing else; only a store an earlier build left,
//! or a build that cannot name its commit, makes an ingest build a
//! scratch store to compare shapes with. What each function writes is its own
//! business: the runner only hears the answer.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::raw_layout::entities_db;
use datalib_store_meta::Meta;

use crate::events::{Emitter, OutputClaim};
use crate::function::Function;
use crate::source::StepEnv;

pub async fn run(env: &StepEnv, data_root: &Path, emitter: &Emitter) -> Result<Vec<OutputClaim>> {
    let tree = data_root.join(&env.step);
    match env.function {
        Function::Ingest => prepare_raw_store(env, &tree, emitter).await,
        // A derived store is a function of another; one in another shape is
        // rebuilt by running the step, never migrated in place. Only its
        // indexes moving is made good here, by its owner's open.
        Function::RenderMarkdown => {
            let store = datalib_etl_render::indexed_markdown::path_for(&tree);
            let want = datalib_etl_render::indexed_markdown::shape();
            if bring_to_shape(env, emitter, &store, &want).await? == Standing::IndexesOnly {
                let tree = tree.clone();
                tokio::task::spawn_blocking(move || {
                    datalib_etl_render::indexed_markdown::IndexedMarkdownStore::open(&tree)
                        .map(|s| s.close())
                })
                .await
                .context("render store open panicked")??;
            }
            Ok(Vec::new())
        }
        Function::UnifiedGridAndSearchIndex => {
            let store = datalib_core::layout::grid_index_db(data_root);
            let retired = datalib_core::layout::index_step_dir(data_root)
                .join(datalib_core::layout::RETIRED_GRID_DB);
            if retired.exists() {
                tracing::info!(
                    step = %env.step,
                    "prepare: the grid index is still the doltlite store; it needs a rerun"
                );
                emitter.needs_rerun();
                return Ok(Vec::new());
            }
            let want = datalib_etl_render::grid_index::shape();
            if bring_to_shape(env, emitter, &store, &want).await? == Standing::IndexesOnly {
                let pool = datalib_etl_render::grid_index::open_index(&store).await?;
                datalib_etl_render::grid_index::close_index(pool).await?;
            }
            if search_index_is_stale(env, data_root).await? {
                tracing::info!(
                    step = %env.step,
                    "prepare: the search index is in another shape or was drawn by another \
                     version; it needs a rerun"
                );
                emitter.needs_rerun();
            }
            Ok(Vec::new())
        }
        Function::RenderTimeSeries | Function::TimeSeriesIndex => {
            let store = crate::published::time_series_store(env, data_root);
            let kind = if env.function == Function::TimeSeriesIndex {
                datalib_etl::doltlite_raw::StoreKind::TimeSeriesIndex
            } else {
                datalib_etl::doltlite_raw::StoreKind::TimeSeries
            };
            let want = datalib_etl_time_series::store::shape(kind);
            if bring_to_shape(env, emitter, &store, &want).await? == Standing::IndexesOnly {
                datalib_etl_time_series::store::open(&store, kind)
                    .await?
                    .close()
                    .await;
            }
            Ok(Vec::new())
        }
        Function::RenderOccurrences => {
            let store = datalib_etl::layout::render_occurrences_db(data_root, &env.group);
            let want = datalib_etl_calendar_render::occurrences::store::shape();
            if bring_to_shape(env, emitter, &store, &want).await? == Standing::IndexesOnly {
                datalib_etl_calendar_render::occurrences::store::open(&store)
                    .await?
                    .close()
                    .await;
            }
            Ok(Vec::new())
        }
        Function::Embed | Function::EmbeddingMap => {
            tracing::debug!(step = %env.step, "prepare: nothing of ours to bring up to date");
            Ok(Vec::new())
        }
    }
}

/// Whether the search index's keyword tables are in another shape, or a
/// source the step reads was drawn by another version of the index text:
/// a build that draws or chunks otherwise asks for the keyword pass,
/// whose renders did not move.
async fn search_index_is_stale(env: &StepEnv, data_root: &Path) -> Result<bool> {
    let index = datalib_core::layout::grid_index_db(data_root);
    let cache = datalib_core::layout::inference_cache_db(data_root);
    let Some(reader) = datalib_search_index::Reader::open(&index, &cache).await? else {
        return Ok(false);
    };
    let stale = stale_in(&reader, env).await;
    reader.close().await;
    stale
}

async fn stale_in(reader: &datalib_search_index::Reader, env: &StepEnv) -> Result<bool> {
    if !reader.in_this_shape().await? {
        return Ok(true);
    }
    for source in crate::index_steps::groups_from_inputs(&env.inputs) {
        if reader.drawn_by_another_version(&source).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// What `store`'s `_datalib_meta` says, or a refusal when a newer line of
/// datalib wrote it: opening it would drop what that build knew.
async fn meta_of(store: &Path) -> Result<Option<Meta>> {
    let meta = datalib_store_meta::guard::read_at(store)
        .await
        .with_context(|| format!("read {}'s _datalib_meta", store.display()))?;
    datalib_store_meta::refuse_if_newer(store, meta.as_ref())?;
    Ok(meta)
}

/// Where an older build kept a source's blob store, which opening the raw
/// store converts (`datalib_etl::blob_cas`, whose name for it is private).
const DOLTLITE_BLOB_STORE: &str = "blobs.doltlite_db";

async fn prepare_raw_store(
    env: &StepEnv,
    tree: &Path,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let db = entities_db(tree);
    // A source that has never downloaded is created at this build's shape
    // by its first download; opening it here would only make it empty.
    if !db.exists() {
        tracing::debug!(step = %env.step, "prepare: no raw store yet");
        return Ok(Vec::new());
    }
    let source_type = env.source_type()?;
    let meta = meta_of(&db).await?;
    // The raw store's meta says nothing of the blob store beside it, which
    // an older build left in doltlite's format and the open converts.
    let blobs_to_convert = tree.join(DOLTLITE_BLOB_STORE).exists();
    if let (Some(meta), false) = (&meta, blobs_to_convert) {
        if opened_by_this_build(meta) {
            tracing::debug!(step = %env.step, "prepare: this build has opened the raw store");
            return Ok(Vec::new());
        }
        if in_this_builds_shape(meta, &raw_shape(source_type).await?) {
            tracing::debug!(step = %env.step, "prepare: the raw store is in this build's shape");
            return Ok(Vec::new());
        }
    }
    // This step's name says download; its log has to say otherwise from
    // the start, or a long migration reads as one.
    const UPDATING: &str = "updating the raw store for this build; nothing is downloaded";
    tracing::info!(step = %env.step, "prepare: {UPDATING}");
    emitter.progress().set_message(UPDATING);
    bring_raw_store_up_to_date(source_type, &env.group, tree)
        .await
        .with_context(|| format!("prepare {}", env.step))?;
    tracing::info!(step = %env.step, "prepare: the raw store is in this build's shape");
    Ok(crate::ingest::raw_store_version(tree)
        .await?
        .map(|version| OutputClaim {
            path: env.step.clone(),
            version,
            rows: None,
        })
        .into_iter()
        .collect())
}

/// Whether this very build has opened the store: the owner rewrites the
/// build's version and commit into `_datalib_meta` on every open, after
/// its migrations, so a store that names both is in this build's shape.
/// A build that cannot name its commit cannot tell, and compares shapes.
pub(crate) fn opened_by_this_build(stored: &Meta) -> bool {
    let commit = datalib_runtime::build_id::git_hash();
    commit.is_some()
        && stored.git_hash == commit
        && stored.datalib_version == datalib_runtime::build_id::DATALIB_VERSION
}

/// The shape a raw store of `source_type` is in at this build: read off a
/// new one made in a scratch directory, since only the provider's open
/// knows its DDL and its ladders. Made once per process and type. What a
/// render requires of the raw store it reads (`crate::render`).
pub(crate) async fn raw_shape(source_type: &str) -> Result<datalib_store_meta::Shape> {
    static MADE: std::sync::Mutex<std::collections::BTreeMap<String, datalib_store_meta::Shape>> =
        std::sync::Mutex::new(std::collections::BTreeMap::new());
    if let Some(shape) = MADE.lock().expect("unpoisoned").get(source_type) {
        return Ok(shape.clone());
    }
    let scratch = tempfile::tempdir().context("a scratch directory for this build's shape")?;
    let raw = scratch.path().join("ingest");
    crate::dispatch::migrate(source_type, &raw).await?;
    let shape = datalib_store_meta::guard::read_at(&entities_db(&raw))
        .await?
        .with_context(|| format!("a new {source_type} store records no _datalib_meta"))?
        .shape();
    MADE.lock()
        .expect("unpoisoned")
        .insert(source_type.to_string(), shape.clone());
    Ok(shape)
}

/// Whether a store whose meta says `stored` needs nothing from this build:
/// this build's shape, written by this release. A store an older release
/// wrote in the same shape is still opened once, since a provider's open
/// may also fill in what that release left out (contacts' photos).
fn in_this_builds_shape(stored: &Meta, this_build: &datalib_store_meta::Shape) -> bool {
    stored.shape() == *this_build
        && stored.datalib_version == datalib_runtime::build_id::DATALIB_VERSION
}

/// The source's own migration, then its older problems named.
async fn bring_raw_store_up_to_date(source_type: &str, group: &str, tree: &Path) -> Result<()> {
    crate::dispatch::migrate(source_type, tree).await?;
    adopt_sourceless_problems(group, tree).await
}

/// Name the source on the problems an older build wrote naming none, so
/// they count on the source's row from launch rather than from its next
/// sync. Committed only when there was one to name.
async fn adopt_sourceless_problems(group: &str, tree: &Path) -> Result<()> {
    // Bare: only the provider's own open knows its DDL and ladder, and
    // one without them reads a store its ladder has climbed as newer.
    let pool = datalib_etl::doltlite_raw::open_bare(&entities_db(tree)).await?;
    let adopted = async {
        let n = datalib_etl::doltlite_raw::adopt_sourceless_problems(&pool, group).await?;
        if n > 0 {
            datalib_etl::doltlite_raw::commit_run(
                &pool,
                &format!("prepare {group}: name the source on {n} problems"),
            )
            .await?;
        }
        anyhow::Ok(n)
    }
    .await;
    pool.close().await;
    let n = adopted.with_context(|| format!("name {group}'s source on its problems"))?;
    if n > 0 {
        tracing::info!(
            group,
            problems = n,
            "prepare: named the source on older problems"
        );
    }
    Ok(())
}

/// Where a derived store stands against the shape this build writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Standing {
    /// Not written yet: in no shape at all.
    Absent,
    Current,
    /// Its tables are this build's and its indexes are not: its owner's
    /// open makes them, which costs creating them rather than a rebuild.
    IndexesOnly,
    /// Its tables are another shape, or it records none: its step
    /// rebuilds it.
    Other,
}

fn standing(
    have: Option<&datalib_store_meta::Shape>,
    want: &datalib_store_meta::Shape,
) -> Standing {
    match have {
        Some(have) if have == want => Standing::Current,
        Some(have) if have.tables == want.tables && have.versions == want.versions => {
            Standing::IndexesOnly
        }
        _ => Standing::Other,
    }
}

/// How `store` stands against `want`, answering `needs_rerun` when only
/// its step can bring it there. The caller opens it when only its indexes
/// moved.
async fn bring_to_shape(
    env: &StepEnv,
    emitter: &Emitter,
    store: &Path,
    want: &datalib_store_meta::Shape,
) -> Result<Standing> {
    if !store.exists() {
        tracing::debug!(step = %env.step, "prepare: no store yet");
        return Ok(Standing::Absent);
    }
    let meta = meta_of(store).await?;
    let standing = standing(meta.map(|m| m.shape()).as_ref(), want);
    match standing {
        Standing::Other => {
            tracing::info!(step = %env.step, "prepare: the store is in another shape; it needs a rerun");
            emitter.needs_rerun();
        }
        Standing::IndexesOnly => {
            tracing::info!(step = %env.step, "prepare: making the store's new indexes")
        }
        Standing::Current | Standing::Absent => {
            tracing::debug!(step = %env.step, "prepare: the store is in this build's shape")
        }
    }
    Ok(standing)
}

/// The raw-store shape of every source type, by release: what a store a
/// past build left looks like, so a test can build one and migrate it.
/// `raw_shapes/current.json` is this tree's; a release copies it to
/// `raw_shapes/<version>.json` (`raw_shapes/README.md`).
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use datalib_store_meta::{StoreKind, Versions};
    use serde::{Deserialize, Serialize};
    use strum::VariantArray;

    use super::*;
    use crate::source_type::SourceType;

    const SHAPES_DIR: &str = "datalib/backend/datalib_step/raw_shapes";

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    struct Shape {
        schema_hash: String,
        schema_version: u32,
        shared_schema_version: u32,
        /// Every table and index the store declares, as `sqlite_master`
        /// holds it, by name.
        sql: Vec<String>,
    }

    type Shapes = BTreeMap<String, Shape>;

    async fn plain_pool(path: &Path) -> sqlx::SqlitePool {
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap()
    }

    async fn shape_of(db: &Path) -> Shape {
        let meta = datalib_store_meta::guard::read_at(db)
            .await
            .unwrap()
            .expect("a migrated store has _datalib_meta");
        let reader = datalib_etl::doltlite_raw::open_reader(db, None)
            .await
            .unwrap()
            .expect("a migrated store has a commit");
        let sql: Vec<String> = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE sql IS NOT NULL \
             AND name NOT LIKE 'sqlite_%' AND name != '_datalib_meta' ORDER BY name",
        )
        .fetch_all(reader.pool())
        .await
        .unwrap();
        reader.pool().close().await;
        Shape {
            schema_hash: meta.schema_hash,
            schema_version: meta.schema_version,
            shared_schema_version: meta.shared_schema_version,
            sql,
        }
    }

    /// A store as this build creates it, in a directory that lives as long
    /// as the handle.
    async fn fresh(source_type: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("ingest");
        std::fs::create_dir_all(&raw).unwrap();
        crate::dispatch::migrate(source_type, &raw).await.unwrap();
        (dir, raw)
    }

    /// Each source type's store as this build creates it. A type with no
    /// raw store (Perseus) has no entry.
    async fn current_shapes() -> Shapes {
        let mut shapes = Shapes::new();
        for &t in SourceType::VARIANTS {
            let (_dir, raw) = fresh(t.as_str()).await;
            if entities_db(&raw).exists() {
                shapes.insert(t.as_str().to_string(), shape_of(&entities_db(&raw)).await);
            }
        }
        shapes
    }

    /// Every table's columns as SQLite reports them, and every index by
    /// name: what two stores must agree on to be in one shape, however
    /// each got there (`CREATE` or `ALTER`).
    async fn columns_of(db: &Path) -> BTreeMap<String, Vec<String>> {
        let reader = datalib_etl::doltlite_raw::open_reader(db, None)
            .await
            .unwrap()
            .expect("a migrated store has a commit");
        let objects: Vec<(String, String)> = sqlx::query_as(
            "SELECT type, name FROM sqlite_master WHERE type IN ('table', 'index') \
             AND name NOT LIKE 'sqlite_%' AND name != '_datalib_meta' ORDER BY name",
        )
        .fetch_all(reader.pool())
        .await
        .unwrap();
        let mut out = BTreeMap::new();
        for (kind, name) in objects {
            let columns: Vec<String> = if kind == "table" {
                sqlx::query_scalar(
                    "SELECT name || ' ' || type || ' notnull=' || \"notnull\" || ' default=' || \
                     ifnull(dflt_value, '-') || ' pk=' || pk FROM pragma_table_info(?) ORDER BY cid",
                )
                .bind(&name)
                .fetch_all(reader.pool())
                .await
                .unwrap()
            } else {
                Vec::new()
            };
            out.insert(format!("{kind} {name}"), columns);
        }
        reader.pool().close().await;
        out
    }

    /// A store as the build of `release` left it: the tables it declared,
    /// one made-up row in each, at the ladder heights it recorded. The row
    /// matters: an open rebuilds an empty table whatever shape it is in,
    /// so only a table with something in it shows a missing rung.
    async fn store_at(dir: &Path, shape: &Shape, release: &str) -> PathBuf {
        let raw = dir.join("ingest");
        std::fs::create_dir_all(&raw).unwrap();
        let db = entities_db(&raw);
        let pool = plain_pool(&db).await;
        let (tables, rest): (Vec<_>, Vec<_>) = shape
            .sql
            .iter()
            .partition(|s| s.starts_with("CREATE TABLE"));
        for stmt in tables.into_iter().chain(rest) {
            // Audited: the SQL is checked in under raw_shapes/, written by
            // this test from a store this repo's own code created.
            sqlx::query(sqlx::AssertSqlSafe(stmt.clone()))
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("{stmt}: {e}"));
        }
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        for table in names {
            one_row(&pool, &table).await;
        }
        let versions = Versions {
            schema: shape.schema_version,
            shared: shape.shared_schema_version,
        };
        let recorded = datalib_store_meta::Shape {
            tables: shape.schema_hash.clone(),
            indexes: None,
            versions,
        };
        datalib_store_meta::write(&pool, StoreKind::Raw, &recorded)
            .await
            .unwrap();
        sqlx::query("UPDATE _datalib_meta SET value = ? WHERE key = 'datalib_version'")
            .bind(release)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("SELECT dolt_commit('-Am', ?)")
            .bind(format!("a store as {release} left it"))
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        raw
    }

    /// Insert a row whose every column holds a value of its declared type.
    /// A column named for a stamp holds one; other text is `{}`, which
    /// passes a JSON check, and on a second try 64 hex digits, which pass
    /// a hash-length check.
    async fn one_row(pool: &sqlx::SqlitePool, table: &str) {
        let columns: Vec<(String, String)> =
            sqlx::query_as("SELECT name, upper(type) FROM pragma_table_info(?) ORDER BY cid")
                .bind(table)
                .fetch_all(pool)
                .await
                .unwrap();
        let mut last_error = None;
        for text in ["'{}'", &format!("'{}'", "a".repeat(64))] {
            let values: Vec<String> = columns
                .iter()
                .map(|(name, ty)| match ty.as_str() {
                    _ if name.ends_with("_at") || name.ends_with("_utc") => {
                        "'2024-03-20T18:28:51Z'".to_string()
                    }
                    t if t.contains("INT") => "1".to_string(),
                    t if t.contains("REAL") || t.contains("FLOA") || t.contains("DOUB") => {
                        "1.5".to_string()
                    }
                    t if t.contains("BLOB") => "x'00'".to_string(),
                    _ => text.to_string(),
                })
                .collect();
            let names: Vec<String> = columns.iter().map(|(n, _)| format!("\"{n}\"")).collect();
            let sql = format!(
                "INSERT INTO \"{table}\" ({}) VALUES ({})",
                names.join(", "),
                values.join(", ")
            );
            // Audited: table and column names come from sqlite_master and
            // pragma_table_info of a store this test just built, quoted.
            match sqlx::query(sqlx::AssertSqlSafe(sql)).execute(pool).await {
                Ok(_) => return,
                Err(e) => last_error = Some(e),
            }
        }
        panic!("no made-up row fits {table}: {last_error:?}");
    }

    fn shapes_dir() -> PathBuf {
        runfiles::Runfiles::create()
            .expect("runfiles tree")
            .rlocation(format!("_main/{SHAPES_DIR}"))
            .unwrap_or_else(|| panic!("rlocation for {SHAPES_DIR}"))
    }

    fn recorded(dir: &Path) -> BTreeMap<String, Shapes> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .map(|p| {
                let release = p.file_stem().unwrap().to_string_lossy().into_owned();
                let text = std::fs::read_to_string(&p).unwrap();
                let shapes =
                    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
                (release, shapes)
            })
            .collect()
    }

    /// `raw_shapes/current.json` is what this tree creates. Regenerate with
    /// `bazel run //datalib/backend/datalib_step:raw_shapes.update`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_current_raw_shapes_are_recorded() {
        let fresh = format!(
            "{}\n",
            serde_json::to_string_pretty(&current_shapes().await).unwrap()
        );
        let rel = format!("{SHAPES_DIR}/current.json");
        if std::env::var("INSTA_UPDATE").as_deref() == Ok("always") {
            let root = std::env::var("INSTA_WORKSPACE_ROOT").unwrap_or_else(|_| ".".into());
            let path = Path::new(&root).join(&rel);
            std::fs::write(&path, fresh)
                .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
            return;
        }
        let on_disk = std::fs::read_to_string(shapes_dir().join("current.json")).unwrap();
        assert_eq!(
            on_disk, fresh,
            "{rel} is out of date with the providers' raw DDL and ladders; run \
             `bazel run //datalib/backend/datalib_step:raw_shapes.update`"
        );
    }

    /// The upgrade a person makes: a store every past release left, opened
    /// by this build's migrate as a launch runs it, ends in this build's
    /// shape and records it. A later open that skips the ladder fails here
    /// too, as a newer store or a wrong shape. A non-additive
    /// change with no rung on the provider's ladder fails here, naming the
    /// release and the source type, instead of in someone's data root.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_recorded_raw_shape_migrates_to_this_build() {
        let mut failures = Vec::new();
        for (release, shapes) in recorded(&shapes_dir()) {
            for (source_type, shape) in shapes {
                if SourceType::parse(&source_type).is_none() {
                    // A source type this build dropped has nothing to open it.
                    continue;
                }
                let (_fresh_dir, fresh_raw) = fresh(&source_type).await;
                let want = columns_of(&entities_db(&fresh_raw)).await;
                let dir = tempfile::tempdir().unwrap();
                let raw = store_at(dir.path(), &shape, &release).await;
                if let Err(e) = bring_raw_store_up_to_date(&source_type, "enterprise", &raw).await {
                    failures.push(format!("{source_type} from {release}: {e:#}"));
                    continue;
                }
                let got = columns_of(&entities_db(&raw)).await;
                for (object, columns) in &want {
                    if got.get(object) != Some(columns) {
                        failures.push(format!(
                            "{source_type} from {release}: {object} migrated to {:?}, this build \
                             creates {columns:?}",
                            got.get(object)
                        ));
                    }
                }
                for object in got.keys().filter(|o| !want.contains_key(*o)) {
                    failures.push(format!(
                        "{source_type} from {release}: {object} is left over; this build has none"
                    ));
                }
                let (got, want) = (
                    shape_of(&entities_db(&raw)).await,
                    shape_of(&entities_db(&fresh_raw)).await,
                );
                if got.schema_hash != want.schema_hash {
                    failures.push(format!(
                        "{source_type} from {release}: recorded shape {}, this build's {}",
                        got.schema_hash, want.schema_hash
                    ));
                }
                if (got.schema_version, got.shared_schema_version)
                    != (want.schema_version, want.shared_schema_version)
                {
                    failures.push(format!(
                        "{source_type} from {release}: ladders at {:?}, this build's at {:?}",
                        (got.schema_version, got.shared_schema_version),
                        (want.schema_version, want.shared_schema_version)
                    ));
                }
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// A contacts store every past release left holds cards whose photos
    /// were never put in the blob store, since only this build's download
    /// puts them there; migrating it puts them there, so each card's page
    /// has its photo to link.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_contacts_store_from_every_release_puts_its_cards_photos_in_the_blob_store() {
        const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABAQMAAAAl21bKAAAAA1BMVEX/AAAZ4gk3AAAAAXRSTlMAQObYZgAAAApJREFUCNdjYAAAAAIAAeIhvDMAAAAASUVORK5CYII=";
        let card = format!(
            "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:tng-picard\r\nFN:Jean-Luc Picard\r\n\
             PHOTO;ENCODING=b;TYPE=PNG:{PNG_B64}\r\nEND:VCARD\r\n"
        );
        let photo = datalib_etl_contacts::ingest::photo::card_photo(&card)
            .0
            .expect("the card carries a photo");
        let hash = datalib_etl::blob_cas::blake3_hex(&photo.bytes);
        let releases: Vec<(String, Shape)> = recorded(&shapes_dir())
            .into_iter()
            .filter_map(|(release, mut shapes)| Some((release, shapes.remove("contacts")?)))
            .collect();
        assert!(!releases.is_empty(), "no release recorded a contacts store");
        for (release, shape) in releases {
            let dir = tempfile::tempdir().unwrap();
            let raw = store_at(dir.path(), &shape, &release).await;
            let pool = plain_pool(&entities_db(&raw)).await;
            sqlx::query("UPDATE contacts SET payload = json_object('vcard', ?)")
                .bind(&card)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("SELECT dolt_commit('-Am', 'a card with a photo')")
                .execute(&pool)
                .await
                .unwrap();
            pool.close().await;

            crate::dispatch::migrate("contacts", &raw).await.unwrap();

            let cas = datalib_etl::blob_cas::BlobCas::open_reader(
                &raw.join(datalib_etl::raw_layout::BLOBS_DB),
            )
            .await
            .unwrap();
            let held = cas.describe(&hash).await.unwrap();
            cas.close().await;
            assert_eq!(
                held.and_then(|h| h.content_type).as_deref(),
                Some("image/png"),
                "contacts from {release}: the card's photo is not in the blob store"
            );
        }
    }

    /// Migrating a store already in this build's shape commits nothing, so
    /// a launch on an up-to-date root moves no version and re-renders
    /// nothing.
    /// The shortcut holds only for this very build, and only when it can
    /// name its commit: a store another commit or release opened, or one
    /// stamped `unknown`, still has its shape compared.
    #[test]
    fn only_a_store_this_build_opened_skips_the_comparison() {
        let mine = Meta {
            datalib_version: datalib_runtime::build_id::DATALIB_VERSION.to_string(),
            git_hash: datalib_runtime::build_id::git_hash(),
            doltlite_version: None,
            schema_hash: "any".into(),
            index_hash: None,
            schema_version: 0,
            shared_schema_version: 0,
            store_kind: None,
            written_at_utc: "2369-04-15T00:00:00Z".into(),
        };
        assert_eq!(
            opened_by_this_build(&mine),
            datalib_runtime::build_id::git_hash().is_some()
        );
        let other_commit = Meta {
            git_hash: Some("not-this-commit".into()),
            ..mine.clone()
        };
        assert!(!opened_by_this_build(&other_commit));
        let other_release = Meta {
            datalib_version: "0.0.0-elsewhere".into(),
            ..mine.clone()
        };
        assert!(!opened_by_this_build(&other_release));
        let unstamped = Meta {
            git_hash: None,
            ..mine
        };
        assert!(!opened_by_this_build(&unstamped));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_store_in_this_builds_shape_migrates_to_the_same_commit() {
        for &t in SourceType::VARIANTS {
            let (_dir, raw) = fresh(t.as_str()).await;
            let before = crate::ingest::raw_store_version(&raw).await.unwrap();
            crate::dispatch::migrate(t.as_str(), &raw).await.unwrap();
            let after = crate::ingest::raw_store_version(&raw).await.unwrap();
            assert_eq!(before, after, "{t}: a second migrate committed");
        }
    }

    fn env_of(step: &str, function: Function) -> StepEnv {
        let args = crate::source::RunArgs {
            function: function.as_str().into(),
            group_type: Some("email".into()),
            source_type: None,
        };
        StepEnv::new(step.into(), &args, Vec::new()).unwrap()
    }

    /// Set one `_datalib_meta` row of a store and commit it.
    async fn set_meta(db: &Path, key: &str, value: &str) {
        let pool = plain_pool(db).await;
        sqlx::query("UPDATE _datalib_meta SET value = ? WHERE key = ?")
            .bind(value)
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("SELECT dolt_commit('-Am', 'set a meta row')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }

    /// A render reads a raw store only in this build's shape: one this
    /// build opened passes on that alone, one another build wrote in the
    /// same shape passes on the comparison, and one in another shape is
    /// refused, naming the download step that brings it up to date.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_render_refuses_a_raw_store_in_another_shape() {
        let root = tempfile::tempdir().unwrap();
        let tree = root.path().join("mail/ingest");
        crate::dispatch::migrate("email", &tree).await.unwrap();
        let db = entities_db(&tree);
        crate::render::require_raw_shape(&db, "email")
            .await
            .unwrap();
        set_meta(&db, "git_hash", "another-commit").await;
        crate::render::require_raw_shape(&db, "email")
            .await
            .unwrap();
        set_meta(&db, "schema_hash", "an older shape").await;
        let e = crate::render::require_raw_shape(&db, "email")
            .await
            .unwrap_err();
        let refused = datalib_store_meta::other_shape(&e).expect("refused for its shape");
        assert_eq!(refused.step().as_deref(), Some("mail/ingest"));
    }

    /// The runner asks at every start, so a raw store this release already
    /// shaped is left alone: no claim, no commit, even where opening it
    /// would have moved a meta row (another commit of the same release).
    /// A store an older release wrote is opened once, and its new version
    /// reported.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_raw_store_this_release_shaped_is_not_opened() {
        let root = tempfile::tempdir().unwrap();
        let env = env_of("mail/ingest", Function::Ingest);
        let tree = root.path().join(&env.step);
        crate::dispatch::migrate("email", &tree).await.unwrap();
        let db = entities_db(&tree);
        set_meta(&db, "git_hash", "another commit of this release").await;
        let before = crate::ingest::raw_store_version(&tree).await.unwrap();
        let emitter = Emitter::new(env.step.clone());
        let claims = run(&env, root.path(), &emitter).await.unwrap();
        assert!(claims.is_empty(), "{claims:?}");
        assert!(!emitter.asked_for_a_rerun());
        assert_eq!(
            crate::ingest::raw_store_version(&tree).await.unwrap(),
            before,
            "nothing sealed"
        );

        set_meta(&db, "datalib_version", "0.1.0").await;
        let claims = run(&env, root.path(), &Emitter::new(env.step.clone()))
            .await
            .unwrap();
        assert_eq!(claims.len(), 1, "an older release's store is opened");
        let meta = datalib_store_meta::guard::read_at(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            meta.datalib_version,
            datalib_runtime::build_id::DATALIB_VERSION
        );
    }

    /// A raw store in this release's shape is still opened while an older
    /// build's doltlite blob store sits beside it, since the open is what
    /// moves those blobs into the plain file; skipped, they would read as
    /// missing for good.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_old_blob_store_beside_a_current_raw_store_is_converted() {
        let root = tempfile::tempdir().unwrap();
        let env = env_of("mail/ingest", Function::Ingest);
        let tree = root.path().join(&env.step);
        crate::dispatch::migrate("email", &tree).await.unwrap();
        let plain = tree.join(datalib_etl::raw_layout::BLOBS_DB);
        if plain.exists() {
            std::fs::remove_file(&plain).unwrap();
        }
        let old = tree.join(DOLTLITE_BLOB_STORE);
        let pool = datalib_etl::doltlite_raw::open(&old, &[datalib_etl::blob_cas::CAS_OBJECTS_DDL])
            .await
            .unwrap();
        let hash = "a".repeat(64);
        sqlx::query("INSERT INTO cas_objects VALUES (?, 5, 'text/plain', x'68656c6c6f')")
            .bind(&hash)
            .execute(&pool)
            .await
            .unwrap();
        datalib_etl::doltlite_raw::commit_run(&pool, "download: blobs")
            .await
            .unwrap();
        pool.close().await;

        run(&env, root.path(), &Emitter::new(env.step.clone()))
            .await
            .unwrap();
        assert!(!old.exists(), "the old blob store is converted and removed");
        let cas = datalib_etl::blob_cas::BlobCas::open_reader(&plain)
            .await
            .unwrap();
        assert!(
            cas.describe(&hash).await.unwrap().is_some(),
            "the blob moved"
        );
        cas.close().await;
    }

    /// A store a newer line of datalib wrote is refused, whichever step
    /// owns it, as `incompatible`, and is left as it is.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_store_a_newer_build_wrote_is_refused_as_incompatible() {
        let root = tempfile::tempdir().unwrap();
        let ingest = env_of("mail/ingest", Function::Ingest);
        let tree = root.path().join(&ingest.step);
        crate::dispatch::migrate("email", &tree).await.unwrap();
        let db = entities_db(&tree);
        set_meta(&db, "datalib_version", "99.0.0").await;
        let before = crate::ingest::raw_store_version(&tree).await.unwrap();
        let err = run(&ingest, root.path(), &Emitter::new(ingest.step.clone()))
            .await
            .unwrap_err();
        assert_eq!(
            crate::hints::classify(&err),
            datalib_dag::FailureKind::Incompatible,
            "{err:#}"
        );
        assert_eq!(
            crate::ingest::raw_store_version(&tree).await.unwrap(),
            before
        );

        let render = env_of("mail/render_markdown", Function::RenderMarkdown);
        let tree = root.path().join(&render.step);
        tokio::task::spawn_blocking({
            let tree = tree.clone();
            move || {
                datalib_etl_render::indexed_markdown::IndexedMarkdownStore::open(&tree)
                    .unwrap()
                    .close()
            }
        })
        .await
        .unwrap();
        set_meta(
            &datalib_etl_render::indexed_markdown::path_for(&tree),
            "datalib_version",
            "99.0.0",
        )
        .await;
        let err = run(&render, root.path(), &Emitter::new(render.step.clone()))
            .await
            .unwrap_err();
        assert_eq!(
            crate::hints::classify(&err),
            datalib_dag::FailureKind::Incompatible
        );
    }

    /// What `_datalib_meta` says this store's shape is, as an older build
    /// would have left it.
    async fn written_by_an_older_build(db: &Path) {
        let pool = plain_pool(db).await;
        sqlx::query("UPDATE _datalib_meta SET value = 'an older shape' WHERE key = 'schema_hash'")
            .execute(&pool)
            .await
            .unwrap();
        // The grid index is plain SQLite, and has no commit to make.
        if db.extension().is_some_and(|e| e == "doltlite_db") {
            sqlx::query("SELECT dolt_commit('-Am', 'an older build')")
                .execute(&pool)
                .await
                .unwrap();
        }
        pool.close().await;
    }

    /// A render store in this build's shape answers nothing; one an older
    /// build wrote in another shape answers `needs_rerun`, and is not
    /// touched. A render that has never run has no shape to be behind in.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_render_store_in_another_shape_asks_for_a_rerun() {
        let root = tempfile::tempdir().unwrap();
        let env = env_of("mail/render_markdown", Function::RenderMarkdown);
        let ask = || async {
            let emitter = Emitter::new(env.step.clone());
            run(&env, root.path(), &emitter).await.unwrap();
            emitter.asked_for_a_rerun()
        };
        assert!(!ask().await, "no store yet");

        let tree = root.path().join(&env.step);
        tokio::task::spawn_blocking({
            let tree = tree.clone();
            move || {
                datalib_etl_render::indexed_markdown::IndexedMarkdownStore::open(&tree)
                    .unwrap()
                    .close()
            }
        })
        .await
        .unwrap();
        assert!(!ask().await, "this build's shape");

        let db = datalib_etl_render::indexed_markdown::path_for(&tree);
        written_by_an_older_build(&db).await;
        assert!(ask().await, "an older build's shape");
    }

    /// A render store whose tables are this build's and whose indexes are
    /// not is brought up to date by prepare itself, which makes the
    /// indexes, rather than asking for a re-render: the store then reads
    /// as this build's shape.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_store_missing_only_an_index_gets_it_without_a_rerun() {
        let root = tempfile::tempdir().unwrap();
        let env = env_of("mail/render_markdown", Function::RenderMarkdown);
        let tree = root.path().join(&env.step);
        tokio::task::spawn_blocking({
            let tree = tree.clone();
            move || {
                datalib_etl_render::indexed_markdown::IndexedMarkdownStore::open(&tree)
                    .unwrap()
                    .close()
            }
        })
        .await
        .unwrap();
        let db = datalib_etl_render::indexed_markdown::path_for(&tree);
        let pool = plain_pool(&db).await;
        sqlx::query("DROP INDEX markdowns_by_render_unit")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE _datalib_meta SET value = 'fewer' WHERE key = 'index_hash'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("SELECT dolt_commit('-Am', 'an older build, one index short')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let emitter = Emitter::new(env.step.clone());
        run(&env, root.path(), &emitter).await.unwrap();
        assert!(!emitter.asked_for_a_rerun());
        let meta = datalib_store_meta::guard::read_at(&db).await.unwrap();
        assert_eq!(
            meta.map(|m| m.shape()),
            Some(datalib_etl_render::indexed_markdown::shape())
        );
        let pool = plain_pool(&db).await;
        let made: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' \
             AND name = 'markdowns_by_render_unit'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        pool.close().await;
        assert_eq!(made, 1);
    }

    /// Only the indexes moving is made good in place; tables or ladders
    /// moving, or no recorded shape, is a rebuild.
    #[test]
    fn a_store_stands_by_its_whole_shape() {
        let want = datalib_store_meta::Shape {
            tables: "t".into(),
            indexes: Some("i".into()),
            versions: Versions {
                schema: 0,
                shared: 1,
            },
        };
        let with = |f: &dyn Fn(&mut datalib_store_meta::Shape)| {
            let mut s = want.clone();
            f(&mut s);
            s
        };
        assert_eq!(standing(Some(&want), &want), Standing::Current);
        assert_eq!(
            standing(Some(&with(&|s| s.indexes = None)), &want),
            Standing::IndexesOnly
        );
        assert_eq!(
            standing(Some(&with(&|s| s.tables = "older".into())), &want),
            Standing::Other
        );
        assert_eq!(
            standing(Some(&with(&|s| s.versions.shared = 0)), &want),
            Standing::Other
        );
        assert_eq!(standing(None, &want), Standing::Other);
    }

    /// The same for the grid index, whose store sits at its own fixed path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_grid_index_in_another_shape_asks_for_a_rerun() {
        let root = tempfile::tempdir().unwrap();
        let env = env_of(
            "unified_grid_and_search_index/unified_grid_and_search_index",
            Function::UnifiedGridAndSearchIndex,
        );
        let ask = || async {
            let emitter = Emitter::new(env.step.clone());
            run(&env, root.path(), &emitter).await.unwrap();
            emitter.asked_for_a_rerun()
        };
        assert!(!ask().await, "no index yet");

        let db = datalib_core::layout::grid_index_db(root.path());
        let pool = datalib_etl_render::grid_index::open_index(&db)
            .await
            .unwrap();
        datalib_etl_render::grid_index::close_index(pool)
            .await
            .unwrap();
        assert!(!ask().await, "this build's shape");

        written_by_an_older_build(&db).await;
        assert!(ask().await, "an older build's shape");
    }

    /// A new version of the index's drawing or chunking moves nothing the
    /// `unified_grid_and_search_index` step's keyword pass reads, so the prepare verb asks for it:
    /// when a source it reads was drawn by another version, and not before
    /// there is an index or once it is this build's; and when the index's
    /// tables are in another shape, which its next writer empties.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_search_index_drawn_by_another_version_asks_for_the_keyword_pass() {
        let root = tempfile::tempdir().unwrap();
        let env = StepEnv {
            inputs: vec!["mail/render_markdown".into()],
            ..env_of(
                "unified_grid_and_search_index/unified_grid_and_search_index",
                Function::UnifiedGridAndSearchIndex,
            )
        };
        let ask = || async {
            let emitter = Emitter::new(env.step.clone());
            run(&env, root.path(), &emitter).await.unwrap();
            emitter.asked_for_a_rerun()
        };
        assert!(!ask().await, "no index yet");
        let grid = datalib_core::layout::grid_index_db(root.path());
        let pool = datalib_etl_render::grid_index::open_index(&grid)
            .await
            .unwrap();
        datalib_etl_render::grid_index::close_index(pool)
            .await
            .unwrap();
        assert!(
            !ask().await,
            "a grid in this build's shape, nothing searched yet"
        );
        let put = |key: String| async {
            let path = datalib_core::layout::grid_index_db(root.path());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let index = datalib_search_index::Index::open(&path).await.unwrap();
            index
                .put(&[datalib_search_index::IndexDoc {
                    source_id: "mail".into(),
                    markdown_uuid: "d".into(),
                    content_hash: key,
                    title: "Ten Forward".into(),
                    head: None,
                    sections: Vec::new(),
                }])
                .await
                .unwrap();
            index.close().await;
        };
        put("h.t0".to_string()).await;
        assert!(ask().await, "drawn by an older version");
        put(format!("h{}", datalib_search_index::text::key_suffix())).await;
        assert!(!ask().await, "this build's version");
        let path = datalib_core::layout::grid_index_db(root.path());
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
        let moved = sqlx::query("UPDATE _datalib_meta SET value = 'an older shape' WHERE key = ?")
            .bind(datalib_store_meta::part_key(
                datalib_store_meta::StoreKind::SearchIndex,
            ))
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        assert_eq!(moved.rows_affected(), 1, "the index records its shape");
        assert!(ask().await, "tables in an older shape");
    }

    /// A source that has never downloaded gets no store from a prepare:
    /// its first download creates one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_source_with_no_store_is_left_without_one() {
        let root = tempfile::tempdir().unwrap();
        let env = env_of("mail/ingest", Function::Ingest);
        let claims = run(&env, root.path(), &Emitter::new(env.step.clone()))
            .await
            .unwrap();
        assert!(claims.is_empty());
        assert!(!root.path().join("mail").exists());
    }
}
