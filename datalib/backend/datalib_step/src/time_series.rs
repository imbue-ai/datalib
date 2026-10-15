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

/// Every time series a source's raw store holds, read at its head.
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
