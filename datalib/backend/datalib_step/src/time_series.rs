//! The two time-series functions: a source's `render_time_series` and
//! the `time_series_index` that stacks every source's. The store and the
//! passes are `datalib_etl_time_series`; this is the step around them.

use std::collections::BTreeSet;
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
    let written =
        datalib_etl_time_series::apply(&pool, &wanted, &BTreeSet::from([env.group.as_str()])).await;
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
    let mut readers = Vec::with_capacity(sources.len());
    for source in &sources {
        let db = datalib_etl::layout::render_time_series_db(data_root, source);
        let reader = if db.exists() {
            doltlite_raw::open_reader(&db, None)
                .await
                .with_context(|| format!("read {source}'s time series"))?
        } else {
            None
        };
        readers.push(reader);
    }
    let inputs: Vec<index::Source<'_>> = sources
        .iter()
        .zip(&readers)
        .map(|(source_id, reader)| index::Source {
            source_id,
            store: reader.as_ref().map(|r| r.pool()),
        })
        .collect();
    let pool = store::open(
        &datalib_core::layout::time_series_index_db(data_root),
        StoreKind::TimeSeriesIndex,
    )
    .await?;
    let written = index::index(&pool, &inputs).await;
    for reader in readers.into_iter().flatten() {
        reader.close().await;
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
                "{what}: {} written, {} with new points, {} removed, {} unchanged",
                written.written, written.points_replaced, written.removed, written.unchanged
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
        points_replaced = written.points_replaced,
        removed = written.removed,
        unchanged = written.unchanged,
        "the time series are current"
    );
    let progress = emitter.progress();
    for (name, n) in [
        ("last_pass_time_series_written", written.written),
        (
            "last_pass_time_series_points_replaced",
            written.points_replaced,
        ),
        ("last_pass_time_series_removed", written.removed),
        ("last_pass_time_series_unchanged", written.unchanged),
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
    /// writes it, seals, and reports the head it sealed; a source named in
    /// `inputs` that has rendered nothing yet is no error.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_index_step_stacks_the_stores_its_inputs_name() {
        use datalib_time_series_schema::{Point, TimeKind, TimeSeries};

        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let weight = TimeSeries {
            time_series_id: "w".into(),
            source_id: "riker".into(),
            entity: "account".into(),
            metric: "weight".into(),
            label: "Weight".into(),
            unit: "kg".into(),
            time_kind: TimeKind::Instant,
            points: vec![Point::value(1, 80.0)],
        };
        let store = store::open(
            &datalib_etl::layout::render_time_series_db(root, "riker"),
            StoreKind::TimeSeries,
        )
        .await
        .unwrap();
        datalib_etl_time_series::apply(&store, &[weight], &BTreeSet::from(["riker"]))
            .await
            .unwrap();
        doltlite_raw::commit_run(&store, "test").await.unwrap();
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
            rows.iter().map(|r| r.metric.as_str()).collect::<Vec<_>>(),
            ["weight"]
        );
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
