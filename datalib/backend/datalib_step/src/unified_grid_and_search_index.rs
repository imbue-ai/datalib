//! The `unified_grid_and_search_index` function: every source's render store, stacked into
//! the unified grid table at `unified_grid_and_search_index/unified_grid_and_search_index`,
//! then each searched source's documents into the search index's keyword
//! tables in the same file. It keeps the inference cache to the vectors a
//! chunk still names, and reports what search holds of each source.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::stop::StopFlag;
use datalib_etl_render::grid_index::{build_grid_index_for, open_index};
use datalib_schema::problems::{Severity, METRIC};
use serde::Deserialize;

use crate::events::{Emitter, OutputClaim};
use crate::function::Function;
use crate::source::StepEnv;

/// The one tree this step writes, `unified_grid_and_search_index/unified_grid_and_search_index`, as the
/// applet that reads it resolves it from the data root.
pub fn out_rel() -> String {
    format!(
        "{}/{}",
        datalib_core::layout::UNIFIED_GRID_AND_SEARCH_INDEX_DIR,
        datalib_core::layout::INDEX_STEP_DIR
    )
}

/// What a config may say to `unified_grid_and_search_index`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Params {
    /// Sources kept out of search: in the grid, and not found by their
    /// words or their meaning. Every other source this step reads is
    /// searched.
    #[serde(default)]
    pub not_searched: Vec<String>,
}

impl Params {
    pub fn from_json(params: serde_json::Value) -> Result<Params> {
        if params.is_null() {
            return Ok(Params::default());
        }
        serde_json::from_value(params).context("read unified_grid_and_search_index's params")
    }

    /// `sources` less the ones kept out of search.
    fn searched(&self, sources: &[String]) -> Vec<String> {
        sources
            .iter()
            .filter(|s| !self.not_searched.contains(s))
            .cloned()
            .collect()
    }
}

/// The inputs whose render store this step indexes: not a source's
/// time-series or occurrences step, which an older config names here for
/// their problems, which a reader now finds in their own stores.
fn render_inputs(inputs: &[String]) -> Vec<String> {
    inputs
        .iter()
        .filter(|input| {
            let function = input.rsplit_once('/').map_or("", |(_, f)| f);
            !matches!(
                Function::parse(function),
                Some(Function::RenderTimeSeries | Function::RenderOccurrences)
            )
        })
        .cloned()
        .collect()
}

/// The step, with the embedding model's tokenizer put in place and opened
/// when a source is searched: the keyword pass cuts chunks in its tokens.
pub async fn run(
    data_root: &Path,
    env: &StepEnv,
    params: &Params,
    models_dir: Option<std::path::PathBuf>,
    now: Option<&str>,
    emitter: &Emitter,
    stop: &StopFlag,
) -> Result<Vec<OutputClaim>> {
    let render_inputs = render_inputs(&env.inputs);
    let searched = params.searched(&crate::index_steps::groups_from_inputs(&render_inputs));
    let model = match searched.is_empty() {
        true => None,
        false => Some(crate::index_steps::open_model(data_root, models_dir).await?),
    };
    let ends = |text: &str| match &model {
        Some((_, tokenizer)) => tokenizer.ends(text),
        None => anyhow::bail!("no source is searched, so no tokenizer was opened"),
    };
    run_with(data_root, env, params, &ends, now, emitter, stop).await
}

/// [`run`] with `ends` the tokenizer's: a test's stand-in, or the model's.
pub(crate) async fn run_with(
    data_root: &Path,
    env: &StepEnv,
    params: &Params,
    ends: datalib_search_index::chunk::TokenEnds<'_>,
    now: Option<&str>,
    emitter: &Emitter,
    stop: &StopFlag,
) -> Result<Vec<OutputClaim>> {
    let db_path = datalib_core::layout::grid_index_db(data_root);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
        // Tag the whole `unified_grid_and_search_index/` tree, not just this step's own
        // directory: every index under it is rebuilt from the sidecar
        // trees, so cache-aware backups (`restic --exclude-caches` etc.)
        // may skip all of it. Tagging the parent also means the tag is
        // right before the search step has ever run. Nothing precious lives
        // here — feedback and the loop's record are under `system/`, which is
        // never tagged.
        datalib_core::layout::mark_derived_cache(
            &datalib_core::layout::unified_grid_and_search_index_dir(data_root),
        );
    }
    let pool = open_index(&db_path).await?;

    // The stores to read come from the graph, not from a directory scan:
    // the same rule the search step follows, so the two indexes agree on
    // what a source is.
    let render_inputs = render_inputs(&env.inputs);
    let sources = crate::index_steps::groups_from_inputs(&render_inputs);
    tracing::debug!(sources = %sources.join(", "), "the render stores the graph names");
    let progress = emitter.progress();
    let summary = build_grid_index_for(
        &pool,
        data_root,
        &sources,
        |m| progress.set_message(m),
        now,
        stop,
    )
    .await
    .context("stack the per-source render stores")?;
    tracing::info!(
        // `read` is the one that says whether the cursors are working:
        // it is how many documents were pulled out of the per-source
        // stores at all, and on a steady-state run it should be 0.
        read = summary.markdowns_total,
        loaded = summary.markdowns_loaded,
        removed = summary.markdowns_removed,
        rows = summary.rows_inserted,
        "the index is built"
    );

    // What this pass did, not a running total: the next pass reports
    // its own, so each is a gauge, and its name says which pass.
    for (name, n) in [
        ("last_pass_markdowns_read", summary.markdowns_total),
        ("last_pass_markdowns_loaded", summary.markdowns_loaded),
        ("last_pass_markdowns_removed", summary.markdowns_removed),
        ("last_pass_rows_inserted", summary.rows_inserted),
        ("last_pass_search_terms_written", summary.terms_written),
        (
            "last_pass_search_indexes_remade",
            summary.search_indexes_remade,
        ),
    ] {
        progress.metric(name, &[], n as i64);
    }
    // The problems the index recorded itself, the way every step reports
    // what it found: what the Manage row for the index shows.
    let counts = datalib_etl_render::grid_index::own_problem_counts(&pool).await?;
    for severity in [Severity::Error, Severity::Warning] {
        progress.metric(
            METRIC,
            &[severity.metric_label()],
            counts.get(&severity).copied().unwrap_or(0),
        );
    }

    datalib_etl_render::grid_index::close_index(pool).await?;

    let searched = params.searched(&sources);
    for source in &searched {
        if stop.requested() {
            break;
        }
        let done = crate::search_index::update_keywords(data_root, source, ends, &progress, stop)
            .await
            .with_context(|| format!("keyword-index {source}"))?;
        progress.metric(
            "search_index_unsettled",
            &[("source", source.as_str())],
            done.unsettled as i64,
        );
        if done != crate::search_index::Done::default() {
            tracing::info!(
                source = %source,
                put = done.put,
                removed = done.removed,
                unsettled = done.unsettled,
                "the source's keywords are done"
            );
        }
    }
    if !stop.requested() {
        let dropped = crate::search_index::keep_searched(data_root, &searched).await?;
        if dropped > 0 {
            tracing::info!(
                dropped,
                "documents of sources no longer searched left the search index"
            );
        }
        // No embed runs beside this step: each reads it, off disk, so the
        // runner keeps this step waiting while one does.
        let swept = crate::search_index::sweep(data_root).await?;
        if swept > 0 {
            tracing::info!(swept, "vectors no chunk names left the inference cache");
        }
        let held = datalib_search_index::holdings(data_root, &searched).await?;
        for h in &held {
            let labels = [("source", h.source.as_str())];
            progress.metric("search_documents", &labels, h.documents as i64);
            progress.metric("search_chunks_not_embedded", &labels, h.not_embedded as i64);
        }
        progress.set_message(&holdings_summary(&held));
        retire_aggregator_tree(data_root)?;
    }

    // The version, whether or not this pass changed anything: a pass that
    // moved nothing must still report the version it left, or the runner
    // takes it as new.
    let version = version(data_root).await?;

    // Every other source is sealed by now; this one is still a failure,
    // and the Manage row should say so.
    if !summary.sources_failed.is_empty() {
        let failed: Vec<String> = summary
            .sources_failed
            .iter()
            .map(|(source, why)| format!("{source}: {why}"))
            .collect();
        anyhow::bail!(
            "indexed every other source, but could not read {}:\n{}",
            summary.sources_failed.len(),
            failed.join("\n")
        );
    }

    match version {
        Some(version) => Ok(vec![OutputClaim {
            path: out_rel(),
            version,
            rows: None,
        }]),
        None => Ok(vec![]),
    }
}

/// The step's one-line account of what search holds.
fn holdings_summary(held: &[datalib_search_index::Holding]) -> String {
    let documents: usize = held.iter().map(|h| h.documents).sum();
    let left: usize = held.iter().map(|h| h.not_embedded).sum();
    format!(
        "{documents} documents in {} searched sources; {left} chunks not embedded",
        held.len()
    )
}

/// What the retired `qmd_aggregator` step kept under its tree: qmd's index,
/// its config and its link to the models. Nothing reads any of it; the
/// models the link names stay where they are.
fn retire_aggregator_tree(root: &Path) -> Result<()> {
    let dir = datalib_core::layout::qmd_dir(root);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {
            tracing::info!(path = %dir.display(), "removed the retired aggregator's tree");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", dir.display())),
    }
}

/// What the step has published: the grid's generation, which the builder
/// raises once per source that moved, and the search index's, which every
/// keyword write raises. Each embed reads this step, so a keyword change
/// alone must move it too. `None` before the grid has a file.
pub async fn version(data_root: &Path) -> Result<Option<String>> {
    let path = datalib_core::layout::grid_index_db(data_root);
    if !path.is_file() {
        return Ok(None);
    }
    let pool = datalib_pin::open_reader(&path).await?;
    let grid = datalib_pin::generation(&pool).await;
    pool.close().await;
    let Some(grid) = grid? else {
        return Ok(None);
    };
    let reader = datalib_search_index::Reader::open(
        &path,
        &datalib_core::layout::inference_cache_db(data_root),
    )
    .await?;
    let search = match reader {
        Some(reader) => {
            let generation = reader.generation().await;
            reader.close().await;
            Some(generation?)
        }
        None => None,
    };
    Ok(Some(match search {
        Some(search) => format!("{grid}+search{search}"),
        None => grid,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the step's Manage row says about the whole search index.
    #[test]
    fn the_summary_adds_up_every_source() {
        let h = |source: &str, documents, not_embedded| datalib_search_index::Holding {
            source: source.to_string(),
            documents,
            not_embedded,
        };
        assert_eq!(
            holdings_summary(&[h("mail", 12, 0), h("notes", 3, 7)]),
            "15 documents in 2 searched sources; 7 chunks not embedded"
        );
    }

    /// The retired aggregator's tree goes, qmd's index and its link to the
    /// shared models with it; the models the link pointed at stay.
    #[test]
    fn the_aggregators_tree_goes_and_the_models_it_linked_stay() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let models = tmp.path().join("models");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("m.gguf"), b"weights").unwrap();
        let qmd = datalib_runtime::models::qmd_state_dir(root);
        std::fs::create_dir_all(&qmd).unwrap();
        std::fs::write(qmd.join("index.sqlite"), b"x").unwrap();
        std::os::unix::fs::symlink(&models, qmd.join("models")).unwrap();
        retire_aggregator_tree(root).unwrap();
        assert!(!datalib_core::layout::qmd_dir(root).exists());
        assert!(models.join("m.gguf").exists());
        retire_aggregator_tree(root).unwrap();
    }

    /// The step marks the whole `unified_grid_and_search_index/` tree as derived cache, and
    /// leaves `system/` alone.
    #[tokio::test]
    async fn grid_index_marks_the_index_tree_as_derived_cache() {
        let td = tempfile::tempdir().unwrap();
        let data_root = td.path();

        let env = StepEnv {
            step: "unified_grid_and_search_index/unified_grid_and_search_index".into(),
            group: "unified_grid_and_search_index".into(),
            group_type: None,
            source_group: None,
            source_group_type: None,
            function: crate::function::Function::UnifiedGridAndSearchIndex,
            inputs: Vec::new(),
        };
        run(
            data_root,
            &env,
            &Params::default(),
            None,
            Some("2026-01-01T00:00:00+00:00"),
            &Emitter::new("test".into()),
            &StopFlag::new(),
        )
        .await
        .expect("grid_index over an empty data root should succeed");

        assert!(
            data_root
                .join("unified_grid_and_search_index/CACHEDIR.TAG")
                .is_file(),
            "unified_grid_and_search_index/ must be tagged, so one tag covers both index trees"
        );
        assert!(
            !data_root
                .join("unified_grid_and_search_index/unified_grid_and_search_index/CACHEDIR.TAG")
                .exists(),
            "the per-index tag was replaced by the one on the parent"
        );
        // `system/` is operational history — feedback, the loop's record — and is
        // not rebuildable from raw, so it must never be swept up by
        // `--exclude-caches`.
        assert!(
            !data_root.join("system/CACHEDIR.TAG").exists(),
            "system/ must never be tagged as cache"
        );
    }
}
