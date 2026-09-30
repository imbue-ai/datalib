//! The qmd index, one collection per source, in one file
//! (`unified_index/qmd_aggregator/qmd/index.sqlite`): each source's
//! `keyword_index` and `embed` fill its own collection, and
//! `unified_index/qmd_aggregator`, which reads all of them, keeps the
//! collection set to the sources it reads and reports on the whole.
//!
//! One collection per source, so a search scoped to one source is a
//! filter qmd applies inside retrieval rather than one the applet applies
//! to a global top-N.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_qmd_indexer::{EmbedProgress, Index, Qmd, UpdateProgress};

use crate::events::{Emitter, OutputClaim};
use crate::source::StepEnv;

/// The aggregator's id, which is also the qmd index's directory
/// (`layout::QMD_DIR`), so the index's bytes count against this step.
pub fn aggregator_rel() -> String {
    format!(
        "{}/{}",
        datalib_core::layout::UNIFIED_INDEX_DIR,
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

fn open_index(root: &Path) -> Result<Index> {
    use datalib_runtime::legacy_qmd_dir::{move_to_aggregator_dir, Outcome};
    match move_to_aggregator_dir(root).context("move the qmd index to its new directory")? {
        Outcome::NothingToMove => {}
        Outcome::Moved { from, to } => {
            tracing::info!(from = %from.display(), to = %to.display(), "moved the qmd index");
        }
        Outcome::LeftBeside { old } => {
            tracing::warn!(old = %old.display(), "an old qmd index sits beside the one in use; delete it");
        }
    }
    Index::open(root, Qmd::pinned()?)
}

/// One call on a [`Progress`] handle. Named so the translation below can
/// be a pure function over values and tested without running qmd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProgressCall {
    SetLength(u64),
    Inc(u64),
    Message(String),
}

/// What one of qmd's `EmbedProgress` readings implies, given the reading
/// before it.
///
/// qmd reports absolute positions and the handle takes deltas, so the
/// previous reading is the whole state this needs. The bar counts
/// **documents**, which is what the Manage row's queued and done mean
/// for every other step. The document count moves only now and then
/// (`EmbedProgress`), so the chunks and bytes go in the message, which
/// moves on every reading.
///
/// A reading identical to the one before produces nothing. qmd repeats
/// its final reading, and a step whose message is rewritten with the
/// same text is a step the UI has to redraw for no reason.
pub(crate) fn embed_progress_calls(
    prev: Option<EmbedProgress>,
    now: EmbedProgress,
) -> Vec<ProgressCall> {
    if prev == Some(now) {
        return Vec::new();
    }
    let mut calls = Vec::new();
    if prev.map(|p| p.total_docs) != Some(now.total_docs) {
        calls.push(ProgressCall::SetLength(now.total_docs));
    }
    // `saturating_sub` rather than an assert: a position that went
    // backwards is qmd's business, and dropping an embed over it would
    // trade a wrong progress bar for a failed index.
    let advanced = now
        .docs_embedded
        .saturating_sub(prev.map_or(0, |p| p.docs_embedded));
    if advanced > 0 {
        calls.push(ProgressCall::Inc(advanced));
    }
    calls.push(ProgressCall::Message(embed_message(now)));
    calls
}

fn embed_message(p: EmbedProgress) -> String {
    // Both figures in the unit the *total* deserves, so they stay
    // comparable. Sized off the total and not off each number: early in
    // a small run, "0.0/0.7 MB" reads like nothing is happening.
    const MB: u64 = 1024 * 1024;
    let (unit, scale) = if p.total_bytes >= MB {
        ("MB", MB as f64)
    } else {
        ("KB", 1024.0)
    };
    let mut msg = format!("embedding: {} chunks", p.chunks_embedded);
    if p.total_bytes > 0 {
        msg.push_str(&format!(
            " · {:.1}/{:.1} {unit}",
            p.bytes_processed as f64 / scale,
            p.total_bytes as f64 / scale,
        ));
    }
    if p.errors > 0 {
        msg.push_str(&format!(" · {} retrying", p.errors));
    }
    msg
}

/// Bridge qmd's absolute readings onto the step's progress handle. The
/// previous reading is the only state, held here because the indexer
/// hands each one over as it arrives.
fn embed_progress_sink(progress: Progress) -> datalib_qmd_indexer::OnEmbedProgress {
    let prev: Mutex<Option<EmbedProgress>> = Mutex::new(None);
    Arc::new(move |now: EmbedProgress| {
        let mut prev = prev.lock().unwrap_or_else(|e| e.into_inner());
        for call in embed_progress_calls(*prev, now) {
            match call {
                ProgressCall::SetLength(total) => progress.set_length(Some(total)),
                ProgressCall::Inc(delta) => progress.inc(delta),
                ProgressCall::Message(msg) => progress.set_message(&msg),
            }
        }
        *prev = Some(now);
    })
}

/// Bridge a keyword update's readings onto the step's progress handle,
/// in files. The previous position is the only state.
fn update_progress_sink(progress: Progress) -> datalib_qmd_indexer::OnUpdateProgress {
    let prev: Mutex<UpdateProgress> = Mutex::new(UpdateProgress::default());
    Arc::new(move |now: UpdateProgress| {
        let mut prev = prev.lock().unwrap_or_else(|e| e.into_inner());
        if now.total != prev.total {
            progress.set_length(Some(now.total));
        }
        progress.inc(now.current.saturating_sub(prev.current));
        *prev = now;
    })
}

/// The embedding model alone, first in the pinned table: all an embed
/// loads. The search applet fetches the rest itself.
const EMBED_MODELS: &[datalib_runtime::qmd::PinnedModel] =
    datalib_qmd_models::PINNED_MODELS.split_at(1).0;

/// Put the embedding model in place, sha256-verified, and link the index
/// to it: qmd then finds it already there and never fetches it itself. A
/// no-op once it is.
fn provision_embed_model(index: &Index, root: &Path, models_dir: Option<PathBuf>) -> Result<()> {
    let models = EMBED_MODELS;
    let models_dir = models_dir.unwrap_or_else(datalib_qmd_indexer::default_models_dir);
    let effective = datalib_qmd_models::effective_models_dir(
        &datalib_runtime::qmd::qmd_state_dir(root),
        &models_dir,
    );
    let outcomes = datalib_qmd_models::ensure_models(
        &effective,
        models,
        datalib_qmd_models::Fetch::from_env(),
    )
    .with_context(|| format!("provision qmd models in {}", effective.display()))?;
    let missing = datalib_qmd_models::missing(models, &outcomes);
    if !missing.is_empty() {
        anyhow::bail!(
            "qmd models missing from {} and not fetched ({} is set): {}",
            effective.display(),
            datalib_qmd_models::NO_FETCH_ENV,
            missing.join(", ")
        );
    }
    index.link_models(&models_dir)
}

/// `unified_index/qmd_aggregator`: leave the index's collections exactly
/// the sources whose qmd steps it reads, retiring any other with its
/// documents, and report what each holds.
pub async fn run_aggregator(
    data_root: &Path,
    env: &StepEnv,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let progress = emitter.progress();
    progress.set_message("aggregating the qmd index");
    let groups = groups_from_inputs(&env.inputs);
    let root = data_root.to_path_buf();
    let (retired, collections) = tokio::task::spawn_blocking(move || {
        let index = open_index(&root)?;
        let retired = index.register(&groups)?;
        anyhow::Ok((retired, index.collections()?))
    })
    .await
    .context("qmd task panicked")??;
    if !retired.is_empty() {
        tracing::info!(collections = %retired.join(", "), "retired the collections no source claims");
    }
    for c in &collections {
        let labels = [("source", c.name.as_str())];
        progress.metric("qmd_documents", &labels, c.documents as i64);
        progress.metric("qmd_needs_embedding", &labels, c.needs_embedding as i64);
    }
    progress.set_message(&index_summary(&collections));
    // The index rebuilds from the render_markdown trees, so cache-aware
    // backups (`restic --exclude-caches` etc.) may skip it. Tag the
    // whole `unified_index/` tree for the same reason the grid step
    // does — one tag covers both indexes however they are ordered.
    datalib_core::layout::mark_derived_cache(&datalib_core::layout::unified_index_dir(data_root));
    Ok(claim_reads(env))
}

/// The aggregator's one-line account of the whole index.
fn index_summary(collections: &[datalib_qmd_indexer::Collection]) -> String {
    let documents: u64 = collections.iter().map(|c| c.documents).sum();
    let left: u64 = collections.iter().map(|c| c.needs_embedding).sum();
    format!(
        "{documents} documents in {} sources; {left} not embedded",
        collections.len()
    )
}

/// `<group>/keyword_index`: this source's collection, brought in line
/// with its rendered tree.
pub async fn run_keyword(
    data_root: &Path,
    env: &StepEnv,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let progress = emitter.progress();
    progress.set_message("keyword index");
    let sink = update_progress_sink(progress.clone());
    let root = data_root.to_path_buf();
    let group = env.group.clone();
    let updated = tokio::task::spawn_blocking(move || {
        open_index(&root)?.keyword_index(&[group], sink.as_ref())
    })
    .await
    .context("qmd task panicked")??;
    tracing::info!(%updated, "the keyword index is done");
    Ok(claim_reads(env))
}

/// `<group>/embed`: the vectors this source's collection is missing.
pub async fn run_embed(
    data_root: &Path,
    env: &StepEnv,
    models_dir: Option<PathBuf>,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let progress = emitter.progress();
    progress.set_message("embedding");
    let sink = embed_progress_sink(progress.clone());
    let root = data_root.to_path_buf();
    let group = env.group.clone();
    let embedded = tokio::task::spawn_blocking(move || {
        let index = open_index(&root)?;
        provision_embed_model(&index, &root, models_dir)?;
        index.embed(&[group], sink.as_ref())
    })
    .await
    .context("qmd task panicked")??;
    tracing::info!(%embedded, "the embeddings are done");
    Ok(claim_reads(env))
}

/// A per-source step's version is what it read: qmd hashes the files
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
    fn the_summary_adds_up_every_collection() {
        let c = |name: &str, documents, needs_embedding| datalib_qmd_indexer::Collection {
            name: name.to_string(),
            documents,
            needs_embedding,
        };
        assert_eq!(
            index_summary(&[c("mail", 12, 0), c("notes", 3, 3)]),
            "15 documents in 2 sources; 3 not embedded"
        );
    }

    fn at(bytes_processed: u64, total_bytes: u64, chunks_embedded: u64) -> EmbedProgress {
        EmbedProgress {
            docs_embedded: 0,
            total_docs: 0,
            chunks_embedded,
            total_chunks: 0,
            bytes_processed,
            total_bytes,
            errors: 0,
        }
    }

    fn docs(docs_embedded: u64, total_docs: u64) -> EmbedProgress {
        EmbedProgress {
            docs_embedded,
            total_docs,
            ..at(100, 1000, 8)
        }
    }

    /// The first reading has to declare the total, or the bar has no
    /// scale — and its documents are progress already, not a baseline.
    #[test]
    fn the_first_reading_sets_the_length_and_counts_its_own_documents() {
        assert_eq!(
            embed_progress_calls(None, docs(2, 10)),
            vec![
                ProgressCall::SetLength(10),
                ProgressCall::Inc(2),
                ProgressCall::Message("embedding: 8 chunks · 0.1/1.0 KB".to_string()),
            ]
        );
    }

    /// The script reports absolute counts; the handle takes deltas. This
    /// is the conversion, and getting it wrong double-counts every batch.
    #[test]
    fn a_later_reading_increments_by_the_difference() {
        let calls = embed_progress_calls(Some(docs(2, 10)), docs(5, 10));
        assert_eq!(calls[0], ProgressCall::Inc(3));
        assert_eq!(calls.len(), 2, "the total didn't change, so no SetLength");
    }

    /// The bar is documents, not bytes: the Manage row read "342,810
    /// queued" for a source of a few hundred documents when it was
    /// bytes. Bytes moving between two document counts move only the
    /// message.
    #[test]
    fn bytes_alone_do_not_move_the_bar() {
        let before = EmbedProgress {
            bytes_processed: 100,
            ..docs(2, 10)
        };
        let after = EmbedProgress {
            bytes_processed: 900,
            chunks_embedded: 40,
            ..before
        };
        assert_eq!(
            embed_progress_calls(Some(before), after),
            vec![ProgressCall::Message(
                "embedding: 40 chunks · 0.9/1.0 KB".to_string()
            )]
        );
    }

    /// qmd emits its final reading twice (observed on every run). A
    /// repeat must produce nothing at all — not a zero-Inc, not a
    /// redundant message the UI has to redraw.
    #[test]
    fn an_identical_reading_says_nothing() {
        let same = docs(10, 10);
        assert_eq!(embed_progress_calls(Some(same), same), Vec::new());
    }

    /// Defensive, and deliberately not a panic: a count that went
    /// backwards is qmd's business. Losing an index over a wrong
    /// progress bar would be the worse trade.
    #[test]
    fn a_count_that_went_backwards_does_not_underflow() {
        let calls = embed_progress_calls(Some(docs(5, 10)), docs(3, 10));
        assert!(
            !calls.iter().any(|c| matches!(c, ProgressCall::Inc(_))),
            "nothing advanced, so nothing should increment: {calls:?}"
        );
    }

    /// The script's first reading comes before qmd has said anything, so
    /// it has documents and no bytes; "0.0/0.0 KB" would say nothing.
    #[test]
    fn a_reading_with_no_bytes_yet_leaves_them_out() {
        assert_eq!(
            embed_message(EmbedProgress {
                total_docs: 10,
                ..EmbedProgress::default()
            }),
            "embedding: 0 chunks"
        );
    }

    /// The message is what the Manage row shows while the run is
    /// silent, so it carries the chunk count (a count — `total_chunks`
    /// climbs as qmd discovers chunks, so a ratio would read wrong) and
    /// the byte position, with retries only when there are some.
    #[test]
    fn the_message_reports_chunks_bytes_and_retries() {
        let mb = 1024 * 1024;
        assert_eq!(
            embed_message(at(3 * mb / 2, 7 * mb, 312)),
            "embedding: 312 chunks · 1.5/7.0 MB"
        );
        assert_eq!(
            embed_message(EmbedProgress {
                errors: 4,
                ..at(3 * mb / 2, 7 * mb, 312)
            }),
            "embedding: 312 chunks · 1.5/7.0 MB · 4 retrying"
        );
    }

    /// A corpus under a megabyte is reported in KB. In MB its whole
    /// run reads "0.0/0.7 MB", which looks like nothing is happening —
    /// the opposite of what this message is for.
    #[test]
    fn a_small_corpus_is_reported_in_kb() {
        assert_eq!(
            embed_message(at(34_070, 748_933, 32)),
            "embedding: 32 chunks · 33.3/731.4 KB"
        );
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

    /// Two steps under one group collapse to one collection, and the
    /// list is deduped and ordered so a config reshuffle doesn't churn
    /// the collection set.
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
