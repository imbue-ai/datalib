//! The reset verb: empty what this step wrote, so the next run does its
//! work from the start. The runner invokes the step with
//! `DATALIB_DAG_RESET` naming the part to empty (`docs/dev/step_protocol.md`
//! § Reset); the store's history keeps every row.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::doltlite_raw::reset_store;
use datalib_etl::raw_layout::{blobs_db, entities_db};

use crate::events::OutputClaim;
use crate::function::Function;
use crate::source::StepEnv;

pub async fn run(env: &StepEnv, data_root: &Path, part: &str) -> Result<Vec<OutputClaim>> {
    let tree = data_root.join(&env.step);
    match (env.function, part) {
        (Function::Ingest, "store") => reset_store(&entities_db(&tree)).await?,
        // The CAS only ever goes with the entities: an edge row that
        // names bytes the CAS no longer has would be a store that lies.
        (Function::Ingest, "blobs") => {
            reset_store(&entities_db(&tree)).await?;
            reset_store(&blobs_db(&tree)).await?;
        }
        (Function::RenderMarkdown, "store") => {
            reset_store(&datalib_etl_render::indexed_markdown::path_for(&tree)).await?;
            // The documents are files beside the store, one directory each.
            for entry in std::fs::read_dir(&tree).into_iter().flatten() {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    std::fs::remove_dir_all(entry.path())
                        .with_context(|| format!("remove {}", entry.path().display()))?;
                }
            }
        }
        (function, part) => anyhow::bail!("`{function}` has no {part:?} to reset"),
    }
    tracing::info!(step = %env.step, part, "reset: emptied what the step wrote");
    Ok(Vec::new())
}
