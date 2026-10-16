//! The two time-series functions: a source's `render_time_series` and
//! the `time_series_index` that indexes every source's. The stores and
//! the passes are `datalib_etl_time_series`; this is the step around
//! them.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::doltlite_raw::{self, StoreKind};
use datalib_etl_time_series::{index, store, Summary};
use datalib_time_series_schema::TimeSeries;

use crate::events::{Emitter, OutputClaim};
use crate::source::StepEnv;
use crate::source_type::SourceType;

/// The step id the index must have: it writes this tree and nothing else.
pub fn index_rel() -> String {
    format!(
        "{}/{}",
        datalib_core::layout::UNIFIED_TIME_SERIES_INDEX_DIR,
        datalib_core::layout::TIME_SERIES_INDEX_DIR
    )
}

/// Every time series a source's raw store holds, read at its head. An arm
/// here is a type [`SourceType::renders_time_series`] says yes for.
fn read(source_type: SourceType, raw: &Path, source_id: &str) -> Result<Vec<TimeSeries>> {
    match source_type {
        SourceType::Garmin => {
            Ok(datalib_etl_garmin_render::render::time_series::read(raw, source_id, None)?.1)
        }
        other => anyhow::bail!(
            "a `{}` source renders no time series; take its `render_time_series` step out \
             of the config",
            other.as_str()
        ),
    }
}

pub async fn run_render(
    env: &StepEnv,
    data_root: &Path,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let source_type = SourceType::parse(env.source_type()?)
        .with_context(|| format!("unknown source type {:?}", env.group_type))?;
    let raw = data_root.join(env.raw_store_rel());
    let wanted = read(source_type, &raw, &env.group)?;
    let db = datalib_etl::layout::render_time_series_db(data_root, &env.group);
    let pool = store::open(&db, StoreKind::TimeSeries).await?;
    let written = datalib_etl_time_series::apply(&pool, &wanted, &env.group).await;
    let claim = seal(pool, written, "render_time_series", emitter).await?;
    Ok(vec![OutputClaim {
        path: env.step.clone(),
        ..claim
    }])
}

pub async fn run_index(
    env: &StepEnv,
    data_root: &Path,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let sources = crate::qmd_index::groups_from_inputs(&env.inputs);
    let mut stores = Vec::with_capacity(sources.len());
    for source in &sources {
        let db = datalib_etl::layout::render_time_series_db(data_root, source);
        let rel = db
            .strip_prefix(data_root)
            .unwrap_or(&db)
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let reader = if db.exists() {
            doltlite_raw::open_reader(&db, None)
                .await
                .with_context(|| format!("read {source}'s time series"))?
        } else {
            None
        };
        let commit = reader.as_ref().map(|r| r.pin().commit().to_string());
        stores.push((rel, reader, commit));
    }
    let inputs: Vec<index::Source<'_>> = sources
        .iter()
        .zip(&stores)
        .map(|(source_id, (rel, reader, commit))| index::Source {
            source_id,
            store: rel,
            read: reader
                .as_ref()
                .zip(commit.as_deref())
                .map(|(r, c)| index::Read {
                    pool: r.pool(),
                    commit_hash: c,
                }),
        })
        .collect();
    let pool = store::open(
        &datalib_core::layout::time_series_index_db(data_root),
        StoreKind::TimeSeriesIndex,
    )
    .await?;
    let written = index::index(&pool, &inputs).await;
    for (_, reader, _) in stores {
        if let Some(reader) = reader {
            reader.close().await;
        }
    }
    let claim = seal(pool, written, "time_series_index", emitter).await?;
    Ok(vec![OutputClaim {
        path: index_rel(),
        ..claim
    }])
}

/// Commit what the pass wrote, report it, and close the store, on the
/// error path too.
async fn seal(
    pool: sqlx::SqlitePool,
    written: Result<Summary>,
    what: &str,
    emitter: &Emitter,
) -> Result<OutputClaim> {
    let written = match written {
        Ok(w) => w,
        Err(e) => {
            pool.close().await;
            return Err(e);
        }
    };
    let sealed = async {
        doltlite_raw::commit_run(
            &pool,
            &format!(
                "{what}: {} written, {} removed, {} unchanged; {} chunks written, {} removed",
                written.written,
                written.removed,
                written.unchanged,
                written.chunks_written,
                written.chunks_removed
            ),
        )
        .await?;
        doltlite_raw::head_commit(&pool).await
    }
    .await;
    pool.close().await;
    let head = sealed?.context("the time-series store has no commit after its seal")?;
    tracing::info!(
        written = written.written,
        removed = written.removed,
        unchanged = written.unchanged,
        chunks_written = written.chunks_written,
        chunks_removed = written.chunks_removed,
        "the time series are current"
    );
    let progress = emitter.progress();
    for (name, n) in [
        ("last_pass_time_series_written", written.written),
        ("last_pass_time_series_removed", written.removed),
        ("last_pass_time_series_unchanged", written.unchanged),
        (
            "last_pass_time_series_chunks_written",
            written.chunks_written,
        ),
        (
            "last_pass_time_series_chunks_removed",
            written.chunks_removed,
        ),
    ] {
        progress.metric(name, &[], n as i64);
    }
    Ok(OutputClaim {
        path: String::new(),
        version: head,
        rows: None,
    })
}

#[cfg(test)]
mod tests {
    use strum::VariantArray;

    use super::*;

    /// The index step reads each source's store where its render step
    /// writes it, records the store's path and the commit it read, seals,
    /// and reports the head it sealed; a source named in `inputs` that
    /// has rendered nothing yet is no error.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_index_step_records_where_each_sources_points_are() {
        use datalib_time_series_schema::{Points, TimeKind, TimeSeries};

        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let weight = TimeSeries {
            time_series_id: "w".into(),
            source_id: "riker".into(),
            about_uuid: None,
            name: "weight".into(),
            human_name: "Weight".into(),
            unit: "kg".into(),
            time_kind: TimeKind::Instant,
            points: Points::floats([(1, 80.0)]),
        };
        let db = datalib_etl::layout::render_time_series_db(root, "riker");
        let store = store::open(&db, StoreKind::TimeSeries).await.unwrap();
        datalib_etl_time_series::apply(&store, &[weight], "riker")
            .await
            .unwrap();
        doltlite_raw::commit_run(&store, "test").await.unwrap();
        let riker_head = doltlite_raw::head_commit(&store).await.unwrap().unwrap();
        store.close().await;

        let env = StepEnv {
            step: index_rel(),
            group: datalib_core::layout::UNIFIED_TIME_SERIES_INDEX_DIR.into(),
            group_type: None,
            source_group: None,
            source_group_type: None,
            function: crate::function::Function::TimeSeriesIndex,
            inputs: vec![
                "riker/render_time_series".into(),
                "troi/render_time_series".into(),
            ],
        };
        let claims = run_index(&env, root, &Emitter::new("test".into()))
            .await
            .unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].path, index_rel());

        let index =
            doltlite_raw::open_reader(&datalib_core::layout::time_series_index_db(root), None)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(index.pin().commit(), claims[0].version);
        let rows = store::rows(index.pool()).await.unwrap();
        assert_eq!(
            rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["weight"]
        );
        let sources = store::sources(index.pool()).await.unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].commit_hash, riker_head);
        assert_eq!(root.join(&sources[0].store), db);
        index.close().await;
    }

    /// The config side adds `render_time_series` steps for the types its
    /// own list names; this binary must agree on which those are.
    #[test]
    fn the_config_and_this_binary_agree_on_which_types_have_time_series() {
        for &t in SourceType::VARIANTS {
            assert_eq!(
                t.renders_time_series(),
                datalib_dag::config::TIME_SERIES_SOURCE_TYPES.contains(&t.as_str()),
                "{}",
                t.as_str()
            );
        }
    }
}
