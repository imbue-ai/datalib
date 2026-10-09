//! The migrate verb (`--migrate`): bring what this step wrote to this
//! build's shape where that can be done in place, fetching nothing, and say
//! when it cannot. A launch asks every built-in step once per build,
//! before it takes any request (`docs/dev/step_protocol.md` § Migrate).
//! What each function writes is its own business: the runner only hears
//! the answer.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::raw_layout::entities_db;

use crate::events::{Emitter, OutputClaim};
use crate::function::Function;
use crate::source::StepEnv;

pub async fn run(env: &StepEnv, data_root: &Path, emitter: &Emitter) -> Result<Vec<OutputClaim>> {
    let tree = data_root.join(&env.step);
    match env.function {
        Function::Ingest => migrate_raw_store(env, &tree).await,
        // A derived store is a function of another; one in another shape is
        // rebuilt by running the step, never migrated in place.
        Function::RenderMarkdown => {
            let store = datalib_etl_render::indexed_markdown::path_for(&tree);
            let shape = datalib_etl_render::indexed_markdown::schema_hash();
            ask_for_a_rerun_if(env, emitter, &store, &shape).await?;
            Ok(Vec::new())
        }
        Function::GridIndex => {
            let store = datalib_core::layout::grid_index_db(data_root);
            let shape = datalib_etl_render::grid_index::schema_hash();
            ask_for_a_rerun_if(env, emitter, &store, &shape).await?;
            Ok(Vec::new())
        }
        // The qmd index is qmd's, and the map is redrawn from it.
        Function::QmdAggregator
        | Function::KeywordIndex
        | Function::Embed
        | Function::EmbeddingMap => {
            tracing::info!(step = %env.step, "migrate: nothing of ours to bring up to date");
            Ok(Vec::new())
        }
    }
}

async fn migrate_raw_store(env: &StepEnv, tree: &Path) -> Result<Vec<OutputClaim>> {
    // A source that has never downloaded is created at this build's shape
    // by its first download; opening it here would only make it empty.
    if !entities_db(tree).exists() {
        tracing::info!(step = %env.step, "migrate: no raw store yet");
        return Ok(Vec::new());
    }
    crate::dispatch::migrate(env.source_type()?, tree).await?;
    tracing::info!(step = %env.step, "migrate: the raw store is in this build's shape");
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

/// Answer `needs_rerun` when `store` exists and its `_datalib_meta` names
/// another shape than `shape`, or none. A store not written yet is in no
/// shape at all.
async fn ask_for_a_rerun_if(
    env: &StepEnv,
    emitter: &Emitter,
    store: &Path,
    shape: &str,
) -> Result<()> {
    if !store.exists() {
        tracing::info!(step = %env.step, "migrate: no store yet");
        return Ok(());
    }
    let meta = datalib_store_meta::guard::read_at(store)
        .await
        .with_context(|| format!("read {}'s _datalib_meta", store.display()))?;
    if meta.is_none_or(|m| m.schema_hash != shape) {
        tracing::info!(step = %env.step, "migrate: the store is in another shape; it needs a rerun");
        emitter.needs_rerun();
    } else {
        tracing::info!(step = %env.step, "migrate: the store is in this build's shape");
    }
    Ok(())
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
        datalib_store_meta::write(&pool, StoreKind::Raw, &shape.schema_hash, versions)
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
    /// by this build's migrate, ends in this build's shape. A non-additive
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
                if let Err(e) = crate::dispatch::migrate(&source_type, &raw).await {
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

    /// Migrating a store already in this build's shape commits nothing, so
    /// a launch on an up-to-date root moves no version and re-renders
    /// nothing.
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
        let (group, _) = step.split_once('/').unwrap();
        StepEnv {
            step: step.into(),
            group: group.into(),
            group_type: Some("email".into()),
            source_group: None,
            source_group_type: None,
            function,
            inputs: Vec::new(),
        }
    }

    /// What `_datalib_meta` says this store's shape is, as an older build
    /// would have left it.
    async fn written_by_an_older_build(db: &Path) {
        let pool = plain_pool(db).await;
        sqlx::query("UPDATE _datalib_meta SET value = 'an older shape' WHERE key = 'schema_hash'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("SELECT dolt_commit('-Am', 'an older build')")
            .execute(&pool)
            .await
            .unwrap();
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

    /// The same for the grid index, whose store sits at its own fixed path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_grid_index_in_another_shape_asks_for_a_rerun() {
        let root = tempfile::tempdir().unwrap();
        let env = env_of("unified_index/grid_index", Function::GridIndex);
        let ask = || async {
            let emitter = Emitter::new(env.step.clone());
            run(&env, root.path(), &emitter).await.unwrap();
            emitter.asked_for_a_rerun()
        };
        assert!(!ask().await, "no index yet");

        let db = datalib_core::layout::grid_index_db(root.path());
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let pool = plain_pool(&db).await;
        datalib_store_meta::write(
            &pool,
            StoreKind::GridIndex,
            &datalib_etl_render::grid_index::schema_hash(),
            Versions::default(),
        )
        .await
        .unwrap();
        sqlx::query("SELECT dolt_commit('-Am', 'an index')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        assert!(!ask().await, "this build's shape");

        written_by_an_older_build(&db).await;
        assert!(ask().await, "an older build's shape");
    }

    /// A source that has never downloaded gets no store from a migrate:
    /// its first download creates one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_source_with_no_store_is_left_without_one() {
        let root = tempfile::tempdir().unwrap();
        let env = StepEnv {
            step: "mail/ingest".into(),
            group: "mail".into(),
            group_type: Some("email".into()),
            source_group: None,
            source_group_type: None,
            function: Function::Ingest,
            inputs: Vec::new(),
        };
        let claims = run(&env, root.path(), &Emitter::new(env.step.clone()))
            .await
            .unwrap();
        assert!(claims.is_empty());
        assert!(!root.path().join("mail").exists());
    }
}
