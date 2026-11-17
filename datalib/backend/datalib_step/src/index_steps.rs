//! The search's steps beside the grid step's keyword pass: each source's
//! `embed`, which fills the vectors its chunks lack (`search_index.rs`),
//! and `unified_grid_and_search_index/qmd_aggregator`, which runs after
//! every embed, sweeps the vectors no chunk names and reports on the
//! whole. The aggregator's id is qmd's, whose index this replaced; it
//! deletes what qmd left in its directory.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl::stop::StopFlag;

use crate::events::{Emitter, OutputClaim};
use crate::source::StepEnv;

/// The aggregator's id, which is also its directory under the
/// unified grid and search index.
pub fn aggregator_rel() -> String {
    format!(
        "{}/{}",
        datalib_core::layout::UNIFIED_GRID_AND_SEARCH_INDEX_DIR,
        crate::function::Function::QmdAggregator.as_str()
    )
}

/// The groups a fan-in reads, off its declared inputs.
///
/// An input is a step id, and a step id is the tree it writes, so each
/// one is `<group>/<function>` — the group is its first segment.
/// Taking the list from the graph rather than from a directory scan means
/// a source dropped from the config stops being indexed on the next run,
/// even while its rendered tree is still on disk.
pub(crate) fn groups_from_inputs(inputs: &[String]) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for input in inputs {
        if let Some(group) = input.split('/').next() {
            if !group.is_empty() {
                out.insert(group.to_string());
            }
        }
    }
    out.into_iter().collect()
}

/// Put the embedding model in place, sha256-verified, and answer its
/// directory. A no-op once it is.
fn provision_embed_model(
    root: &Path,
    models_dir: Option<PathBuf>,
    fetch: datalib_embedding_models::Fetch,
) -> Result<PathBuf> {
    let dir = models_dir.unwrap_or_else(|| datalib_embedding_models::models_dir_for(root));
    ensure_embed_model(&dir, fetch)?;
    Ok(dir)
}

/// [`provision_embed_model`] into exactly `dir`.
pub(crate) fn ensure_embed_model(dir: &Path, fetch: datalib_embedding_models::Fetch) -> Result<()> {
    let models = datalib_runtime::models::EMBED_MODELS;
    let outcomes = datalib_embedding_models::ensure_models(dir, models, fetch)
        .with_context(|| format!("provision the embedding model in {}", dir.display()))?;
    let missing = datalib_embedding_models::missing(models, &outcomes);
    if !missing.is_empty() {
        anyhow::bail!(
            "the embedding model is missing from {} and not fetched ({} is set): {}",
            dir.display(),
            datalib_embedding_models::NO_FETCH_ENV,
            missing.join(", ")
        );
    }
    Ok(())
}

/// What qmd kept under the aggregator's directory: its index, the
/// index's config and its link to the models. Nothing reads them now.
fn retire_qmd(root: &Path) -> Result<()> {
    let dir = datalib_runtime::models::qmd_state_dir(root);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {
            tracing::info!(path = %dir.display(), "removed qmd's index; search reads its own");
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", dir.display())),
    }
}

/// `unified_grid_and_search_index/qmd_aggregator`: keep only the vectors a
/// chunk still names, and report what each source holds.
pub async fn run_aggregator(
    data_root: &Path,
    env: &StepEnv,
    emitter: &Emitter,
    _stop: &StopFlag,
) -> Result<Vec<OutputClaim>> {
    let progress = emitter.progress();
    progress.set_message("sweeping the search index");
    retire_qmd(data_root)?;
    let (swept, sources) = crate::search_index::sweep(data_root).await?;
    tracing::info!(swept, "the search index is swept");
    let held = datalib_search_index::holdings(data_root, &sources).await?;
    for h in &held {
        let labels = [("source", h.source.as_str())];
        progress.metric("search_documents", &labels, h.documents as i64);
        progress.metric("search_chunks_not_embedded", &labels, h.not_embedded as i64);
    }
    progress.set_message(&holdings_summary(&held));
    // The index rebuilds from the render stores, so cache-aware backups
    // (`restic --exclude-caches` etc.) may skip it.
    datalib_core::layout::mark_derived_cache(
        &datalib_core::layout::unified_grid_and_search_index_dir(data_root),
    );
    Ok(claim_reads(env))
}

/// The aggregator's one-line account of the whole index.
fn holdings_summary(held: &[datalib_search_index::Holding]) -> String {
    let documents: usize = held.iter().map(|h| h.documents).sum();
    let left: usize = held.iter().map(|h| h.not_embedded).sum();
    format!(
        "{documents} documents in {} sources; {left} chunks not embedded",
        held.len()
    )
}

/// The pinned model, provisioned, and its tokenizer, read without the
/// weights: the grid step's keyword pass and each embed cut chunks counted
/// in its tokens.
pub(crate) async fn open_model(
    data_root: &Path,
    models_dir: Option<PathBuf>,
) -> Result<(PathBuf, datalib_embedder::Tokenizer)> {
    let root = data_root.to_path_buf();
    let fetch = datalib_embedding_models::Fetch::from_env();
    tokio::task::spawn_blocking(move || {
        let models = provision_embed_model(&root, models_dir, fetch)?;
        let model = crate::search_index::model_file(&models);
        let tokenizer = datalib_embedder::Tokenizer::open(&model)?;
        Ok((model, tokenizer))
    })
    .await
    .context("model provisioning panicked")?
}

/// `<group>/embed`: the vectors this source's chunks are missing from the
/// inference cache.
pub async fn run_embed(
    data_root: &Path,
    env: &StepEnv,
    models_dir: Option<PathBuf>,
    emitter: &Emitter,
    stop: &StopFlag,
) -> Result<Vec<OutputClaim>> {
    let progress = emitter.progress();
    progress.set_message("embedding");
    let (model, tokenizer) = open_model(data_root, models_dir).await?;
    let ends = |text: &str| tokenizer.ends(text);
    let done =
        crate::search_index::embed(data_root, &env.group, &model, &ends, &progress, stop).await?;
    report_unsettled(&progress, &env.group, done.unsettled);
    progress.metric("embedded_prompt_bytes", &[], done.prompt_bytes as i64);
    progress.metric("embedding_ms", &[], done.embed_time.as_millis() as i64);
    let (chunks_per_sec, kb_per_sec) = done.rate().unwrap_or_default();
    tracing::info!(
        embedded = done.embedded,
        unsettled = done.unsettled,
        truncated = done.truncated,
        prompt_bytes = done.prompt_bytes,
        embed_secs = format!("{:.1}", done.embed_time.as_secs_f64()),
        chunks_per_sec = format!("{chunks_per_sec:.1}"),
        kb_per_sec = format!("{kb_per_sec:.1}"),
        "the search index's embeddings are done: {chunks_per_sec:.1} chunks/s, \
         {kb_per_sec:.1} KB/s of prompt text"
    );
    Ok(claim_reads(env))
}

/// Documents left for the next pass, as a metric the Manage row shows: a
/// document dropped from this pass leaves a trail.
fn report_unsettled(progress: &Progress, group: &str, unsettled: usize) {
    progress.metric(
        "search_index_unsettled",
        &[("source", group)],
        unsettled as i64,
    );
}

/// A per-source step's version is what it read: the step compares hashes
/// itself, so the runner only has to know when to ask again. Run by
/// hand, with nothing from the runner, it claims nothing, and a runner,
/// if any, takes every success as new.
fn claim_reads(env: &StepEnv) -> Vec<OutputClaim> {
    let reads = std::env::var(datalib_dag::subprocess::ENV_READS).unwrap_or_default();
    version_of_reads(&reads)
        .map(|version| OutputClaim {
            path: env.step.clone(),
            version,
            rows: None,
        })
        .into_iter()
        .collect()
}

/// The runner writes `DATALIB_READS` from a sorted map, so the same
/// render versions are the same bytes.
fn version_of_reads(reads: &str) -> Option<String> {
    let reads = reads.trim();
    (!reads.is_empty() && reads != "{}")
        .then(|| format!("reads:{}", blake3::hash(reads.as_bytes()).to_hex()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A per-source step's version follows what it read, and nothing
    /// else: a run with nothing from the runner claims none.
    #[test]
    fn the_version_is_what_was_read() {
        let a = r#"{"mail/render_markdown":"indexed_markdown.doltlite_db:aa"}"#;
        let b = r#"{"mail/render_markdown":"indexed_markdown.doltlite_db:bb"}"#;
        assert_eq!(version_of_reads(a), version_of_reads(a));
        assert_ne!(version_of_reads(a), version_of_reads(b));
        assert_eq!(version_of_reads(""), None);
        assert_eq!(version_of_reads("{}"), None);
    }

    /// What the aggregator's Manage row says about the whole index.
    #[test]
    fn the_summary_adds_up_every_source() {
        let h = |source: &str, documents, not_embedded| datalib_search_index::Holding {
            source: source.to_string(),
            documents,
            not_embedded,
        };
        assert_eq!(
            holdings_summary(&[h("mail", 12, 0), h("notes", 3, 7)]),
            "15 documents in 2 sources; 7 chunks not embedded"
        );
    }

    /// the search index, its config and its link to the shared models go; the
    /// models themselves, which the link pointed at, stay.
    #[test]
    fn qmd_leaves_nothing_but_the_models_it_linked() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let models = tmp.path().join("models");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("m.gguf"), b"weights").unwrap();
        let qmd = datalib_runtime::models::qmd_state_dir(root);
        std::fs::create_dir_all(&qmd).unwrap();
        std::fs::write(qmd.join("index.sqlite"), b"x").unwrap();
        std::fs::write(qmd.join("index.yml"), b"x").unwrap();
        std::os::unix::fs::symlink(&models, qmd.join("models")).unwrap();
        retire_qmd(root).unwrap();
        assert!(!qmd.exists());
        assert!(models.join("m.gguf").exists());
        retire_qmd(root).unwrap();
    }

    /// With fetching off, a missing model is an error that says where it
    /// should be and why it was not fetched, and nothing is written there.
    #[test]
    fn a_missing_model_with_fetching_off_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("models");
        let err = provision_embed_model(
            tmp.path(),
            Some(dir.clone()),
            datalib_embedding_models::Fetch::Never,
        )
        .unwrap_err();
        let why = format!("{err:#}");
        assert!(why.contains("the embedding model is missing from"), "{why}");
        assert!(why.contains(&dir.display().to_string()), "{why}");
        assert!(
            why.contains(datalib_runtime::models::EMBED_MODELS[0].file),
            "{why}"
        );
        let written = std::fs::read_dir(&dir)
            .map(|d| d.count())
            .unwrap_or_default();
        assert_eq!(written, 0, "nothing fetched into {}", dir.display());
    }

    /// An input is a step id, which is also the tree it writes. The
    /// group is its first segment — not the whole string, and not a
    /// directory listing.
    #[test]
    fn groups_come_from_the_first_segment_of_each_input() {
        let inputs = vec![
            "slack_imbue/render_markdown".to_string(),
            "claude_personal/render_markdown".to_string(),
        ];
        assert_eq!(
            groups_from_inputs(&inputs),
            vec!["claude_personal".to_string(), "slack_imbue".to_string()]
        );
    }

    /// Two steps under one group collapse to one source, and the list is
    /// deduped and ordered so a config reshuffle doesn't churn it.
    #[test]
    fn groups_are_deduped_and_sorted() {
        let inputs = vec![
            "b/render_markdown".to_string(),
            "a/render_markdown".to_string(),
            "a/ingest".to_string(),
            String::new(),
        ];
        assert_eq!(
            groups_from_inputs(&inputs),
            vec!["a".to_string(), "b".to_string()]
        );
    }
}
