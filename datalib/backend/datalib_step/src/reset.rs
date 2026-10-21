//! The reset verb: empty what this step wrote, so the next run does its
//! work from the start. The runner invokes the step with `--reset`
//! naming the part to empty (`docs/dev/step_protocol.md` § Reset); the store's history keeps every row. It then reports the
//! counts a run reports, taken off the emptied store, because the Manage
//! row shows a step's newest count and the one from before the reset
//! would otherwise stand until the step next runs.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::doltlite_raw::reset_store;
use datalib_etl::raw_layout::entities_db;
use datalib_schema::problems::{Severity, METRIC};

use crate::events::{Emitter, OutputClaim};
use crate::function::Function;
use crate::source::StepEnv;

pub async fn run(
    env: &StepEnv,
    data_root: &Path,
    part: &str,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let tree = data_root.join(&env.step);
    anyhow::ensure!(
        part == "store" && env.function.resets(),
        "`{}` has no {part:?} to reset",
        env.function
    );
    match env.function {
        Function::Ingest => {
            reset_store(&entities_db(&tree)).await?;
            report_problems(emitter, &entities_db(&tree)).await?;
        }
        Function::RenderMarkdown => {
            let store = datalib_etl_render::indexed_markdown::path_for(&tree);
            reset_store(&store).await?;
            report_problems(emitter, &store).await?;
            crate::render::report_holdings(&emitter.progress(), Default::default());
            // The documents are files beside the store, one directory each.
            for entry in std::fs::read_dir(&tree).into_iter().flatten() {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    std::fs::remove_dir_all(entry.path())
                        .with_context(|| format!("remove {}", entry.path().display()))?;
                }
            }
        }
        Function::EmbeddingMap => crate::embedding_map::reset(data_root)?,
        other => unreachable!("`Function::resets` lets {other} through with no reset"),
    }
    tracing::info!(step = %env.step, part, "reset: emptied what the step wrote");
    Ok(Vec::new())
}

async fn report_problems(emitter: &Emitter, store: &Path) -> Result<()> {
    let counts = datalib_etl::doltlite_raw::problem_counts_at_path(store).await?;
    let progress = emitter.progress();
    for severity in [Severity::Error, Severity::Warning] {
        progress.metric(
            METRIC,
            &[severity.metric_label()],
            counts.get(&severity).copied().unwrap_or(0),
        );
    }
    Ok(())
}
