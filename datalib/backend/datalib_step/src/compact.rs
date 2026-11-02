//! The two verbs that give disk back (`docs/dev/step_protocol.md` §
//! Compact and collect). `--collect` runs `dolt_gc` on the step's store,
//! which deletes only chunks no commit reaches, so every commit stays
//! readable. `--compact` first folds the history of a store nothing reads
//! the history of — a render store and the grid index, both rebuilt from
//! what they read — into one commit, which loses the older commits; any
//! other store it collects. A raw store's history is the record of what
//! upstream once held, and a time-series store's commits are what its
//! index points at, so neither is ever folded.

use std::path::Path;

use anyhow::Result;
use datalib_dag::disk_space::{human_bytes, space};

use crate::events::OutputClaim;
use crate::function::Function;
use crate::source::StepEnv;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Collect,
    Compact,
}

pub async fn run(env: &StepEnv, data_root: &Path, verb: Verb) -> Result<Vec<OutputClaim>> {
    let Some(store) = crate::published::store(env, data_root) else {
        tracing::info!(step = %env.step, "no store of ours to shrink");
        return Ok(Vec::new());
    };
    let Ok(before) = std::fs::metadata(&store).map(|m| m.len()) else {
        tracing::info!(step = %env.step, "nothing written yet");
        return Ok(Vec::new());
    };
    // gc writes the compacted store beside the old one before it drops it.
    let free = space(&store)?.available;
    anyhow::ensure!(
        free > before,
        "shrinking {} needs about its size again in free disk, {}; {} is free",
        store.display(),
        human_bytes(before),
        human_bytes(free)
    );
    let folds = verb == Verb::Compact
        && matches!(env.function, Function::RenderMarkdown | Function::GridIndex);
    let folded = if folds {
        datalib_etl::doltlite_raw::fold_history(&store).await?
    } else {
        None
    };
    if folded.is_none() {
        datalib_etl::doltlite_raw::collect_garbage(&store).await?;
    }
    let after = std::fs::metadata(&store)?.len();
    let size = (human_bytes(before), human_bytes(after));
    match folded {
        Some(f) => tracing::info!(
            step = %env.step,
            commits = f.commits_before,
            before = %size.0,
            after = %size.1,
            "folded the store's history into one commit and collected unused space"
        ),
        None => tracing::info!(
            step = %env.step,
            before = %size.0,
            after = %size.1,
            "collected unused space"
        ),
    }
    Ok(crate::published::claims(env, data_root).await)
}
