//! The compact verb (`--compact`): fold the history of a store nothing
//! reads the history of into one commit, and give back the disk it held
//! (`docs/dev/step_protocol.md` § Compact). That is a render store and the
//! grid index, both rebuilt from what they read. A raw store's history is
//! the record of what upstream once held, and a time-series store's
//! commits are what its index points at, so neither is touched.

use std::path::Path;

use anyhow::Result;
use datalib_dag::disk_space::{human_bytes, space};

use crate::events::OutputClaim;
use crate::function::Function;
use crate::source::StepEnv;

pub async fn run(env: &StepEnv, data_root: &Path) -> Result<Vec<OutputClaim>> {
    let store = match env.function {
        Function::RenderMarkdown => {
            datalib_etl_render::indexed_markdown::path_for(&data_root.join(&env.step))
        }
        Function::GridIndex => datalib_core::layout::grid_index_db(data_root),
        _ => {
            tracing::info!(step = %env.step, "compact: no history of ours to fold");
            return Ok(Vec::new());
        }
    };
    let Ok(before) = std::fs::metadata(&store).map(|m| m.len()) else {
        tracing::info!(step = %env.step, "compact: nothing written yet");
        return Ok(Vec::new());
    };
    // gc writes the compacted store beside the old one before it drops it.
    let free = space(&store)?.available;
    anyhow::ensure!(
        free > before,
        "compacting {} needs about its size again in free disk, {}; {} is free",
        store.display(),
        human_bytes(before),
        human_bytes(free)
    );
    match datalib_etl::doltlite_raw::fold_history(&store).await? {
        Some(folded) => {
            let after = std::fs::metadata(&store)?.len();
            tracing::info!(
                step = %env.step,
                commits = folded.commits_before,
                before = %human_bytes(before),
                after = %human_bytes(after),
                "compact: folded the store's history into one commit"
            );
        }
        None => tracing::info!(step = %env.step, "compact: the history is one commit already"),
    }
    Ok(crate::published::claims(env, data_root).await)
}
