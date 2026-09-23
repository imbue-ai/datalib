//! The `qmd_index` function: the qmd search index over every
//! `render_markdown` tree, written to `unified_index/qmd_index`.
//!
//! One qmd collection per group, so a search scoped to one source is a
//! filter qmd applies inside retrieval rather than one the applet applies
//! to a global top-N.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_qmd_indexer::EmbedProgress;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;

use crate::events::{Emitter, OutputClaim};
use crate::source::StepEnv;

/// The one tree this step writes, as the applet that reads it resolves
/// it from the data root.
pub fn out_rel() -> String {
    format!(
        "{}/{}",
        datalib_core::layout::UNIFIED_INDEX_DIR,
        datalib_core::layout::QMD_DIR
    )
}

/// The groups this step indexes, read off its declared inputs.
///
/// An input is a step id, and a step id is the tree it writes, so each
/// one reads `<group>/render_markdown` — the group is its first segment.
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

/// Collections the index still holds that no group claims any more:
/// the pre-per-source `mirror`, and any source since removed from the
/// config. Read from qmd's own registry table.
///
/// An unreadable or absent index yields none. This runs before qmd does,
/// so the answer is only ever used to *retire* a collection, and a
/// missed one costs a stale collection until the next run — not a wrong
/// index. Failing the step over it would be worse.
async fn collections_to_retire(data_root: &Path, keep: &[String]) -> Vec<String> {
    let path = datalib_runtime::qmd::qmd_index_path(data_root);
    if !path.exists() {
        return Vec::new();
    }
    let found = match read_collection_names(&path).await {
        Ok(names) => names,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "could not read qmd's collections");
            return Vec::new();
        }
    };
    let keep: BTreeSet<&str> = keep.iter().map(String::as_str).collect();
    found
        .into_iter()
        .filter(|name| !keep.contains(name.as_str()))
        .collect()
}

async fn read_collection_names(path: &Path) -> Result<Vec<String>> {
    // qmd's index is a plain SQLite database, unlike every `.doltlite_db`
    // in the tree. Read-only: the file belongs to the qmd subprocess this
    // step is about to run.
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(false)
        .read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    let rows = sqlx::query("SELECT name FROM store_collections")
        .fetch_all(&pool)
        .await;
    pool.close().await;
    Ok(rows
        .context("read store_collections")?
        .iter()
        .filter_map(|r| r.try_get::<String, _>("name").ok())
        .collect())
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
/// previous reading is the whole state this needs. Progress is counted
/// in **input bytes**: `total_chunks` climbs as qmd discovers chunks
/// batch by batch, so a chunk ratio would read wrong — the chunk counts
/// go in the message, where they are a count rather than a fraction.
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
    if prev.map(|p| p.total_bytes) != Some(now.total_bytes) {
        calls.push(ProgressCall::SetLength(now.total_bytes));
    }
    // `saturating_sub` rather than an assert: a position that went
    // backwards is qmd's business, and dropping an embed over it would
    // trade a wrong progress bar for a failed index.
    let advanced = now
        .bytes_processed
        .saturating_sub(prev.map_or(0, |p| p.bytes_processed));
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
    let mut msg = format!(
        "embedding: {} chunks · {:.1}/{:.1} {unit}",
        p.chunks_embedded,
        p.bytes_processed as f64 / scale,
        p.total_bytes as f64 / scale,
    );
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

pub async fn run(
    data_root: &Path,
    env: &StepEnv,
    models_dir: Option<PathBuf>,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let progress = emitter.progress();
    progress.set_message("qmd index");
    let groups = groups_from_inputs(&env.inputs);
    let retire = collections_to_retire(data_root, &groups).await;
    if !retire.is_empty() {
        tracing::info!(
            collections = %retire.join(", "),
            "retiring the collections no group claims"
        );
    }
    let mut opts = datalib_qmd_indexer::IndexOptions::new(data_root);
    opts.groups = groups;
    opts.retire_collections = retire;
    if let Some(d) = models_dir {
        opts.models_dir = d;
    }
    opts.on_embed_progress = Some(embed_progress_sink(progress.clone()));
    // Models first, then the index: qmd finds every pinned GGUF already
    // in place and never fetches one itself. run_index shells out to
    // qmd; blocking work.
    let outcome = tokio::task::spawn_blocking(move || {
        let effective = datalib_qmd_models::effective_models_dir(
            &datalib_runtime::qmd::qmd_state_dir(&opts.root),
            &opts.models_dir,
        );
        let models = datalib_qmd_models::PINNED_MODELS;
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
        datalib_qmd_indexer::run_index(&opts)
    })
    .await
    .context("qmd task panicked")??;
    tracing::info!(index = %outcome.index_path.display(), "the qmd index is done");
    // The index rebuilds from the render_markdown trees, so cache-aware
    // backups (`restic --exclude-caches` etc.) may skip it. Tag the
    // whole `unified_index/` tree for the same reason the grid step
    // does — one tag covers both indexes however they are ordered.
    datalib_core::layout::mark_derived_cache(&datalib_core::layout::unified_index_dir(data_root));

    // Not qmd's sqlite, which is touched on every pass: what was indexed.
    // Exact, because the runner never lets a render write while this step
    // globs its `.md` files. Run by hand, with nothing from the runner, it
    // reports nothing and the runner, if any, hashes the tree.
    let reads = std::env::var(datalib_dag::subprocess::ENV_READS).unwrap_or_default();
    Ok(version_of_reads(&reads)
        .map(|version| OutputClaim {
            path: out_rel(),
            version,
            rows: None,
        })
        .into_iter()
        .collect())
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

    /// The index's version follows the render versions it indexed, and
    /// nothing else: a run with nothing from the runner claims none.
    #[test]
    fn the_version_is_what_was_indexed() {
        let a = r#"{"mail/render_markdown":"indexed_markdown.doltlite_db:aa"}"#;
        let b = r#"{"mail/render_markdown":"indexed_markdown.doltlite_db:bb"}"#;
        assert_eq!(version_of_reads(a), version_of_reads(a));
        assert_ne!(version_of_reads(a), version_of_reads(b));
        assert_eq!(version_of_reads(""), None);
        assert_eq!(version_of_reads("{}"), None);
    }

    fn at(bytes_processed: u64, total_bytes: u64, chunks_embedded: u64) -> EmbedProgress {
        EmbedProgress {
            chunks_embedded,
            total_chunks: 0,
            bytes_processed,
            total_bytes,
            errors: 0,
        }
    }

    /// The first reading has to declare the total, or the bar has no
    /// scale — and its bytes are progress already, not a baseline.
    #[test]
    fn the_first_reading_sets_the_length_and_counts_its_own_bytes() {
        assert_eq!(
            embed_progress_calls(None, at(100, 1000, 8)),
            vec![
                ProgressCall::SetLength(1000),
                ProgressCall::Inc(100),
                ProgressCall::Message("embedding: 8 chunks · 0.1/1.0 KB".to_string()),
            ]
        );
    }

    /// qmd reports absolute positions; the handle takes deltas. This is
    /// the conversion, and getting it wrong double-counts every batch.
    #[test]
    fn a_later_reading_increments_by_the_difference() {
        let calls = embed_progress_calls(Some(at(100, 1000, 8)), at(250, 1000, 20));
        assert_eq!(calls[0], ProgressCall::Inc(150));
        assert_eq!(calls.len(), 2, "the total didn't change, so no SetLength");
    }

    /// qmd emits its final reading twice (observed on every run). A
    /// repeat must produce nothing at all — not a zero-Inc, not a
    /// redundant message the UI has to redraw.
    #[test]
    fn an_identical_reading_says_nothing() {
        let same = at(1000, 1000, 60);
        assert_eq!(embed_progress_calls(Some(same), same), Vec::new());
    }

    /// Defensive, and deliberately not a panic: a position that went
    /// backwards is qmd's business. Losing an index over a wrong
    /// progress bar would be the worse trade.
    #[test]
    fn a_position_that_went_backwards_does_not_underflow() {
        let calls = embed_progress_calls(Some(at(500, 1000, 40)), at(200, 1000, 40));
        assert!(
            !calls.iter().any(|c| matches!(c, ProgressCall::Inc(_))),
            "nothing advanced, so nothing should increment: {calls:?}"
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

    /// A data root that has never synced has no index to read, and that
    /// is a normal state — nothing to retire, no error.
    #[tokio::test]
    async fn no_index_means_nothing_to_retire() {
        let td = tempfile::tempdir().unwrap();
        assert!(collections_to_retire(td.path(), &["a".to_string()])
            .await
            .is_empty());
    }

    /// The migration this exists for: a root indexed before per-source
    /// collections carries `mirror`, which no group claims.
    #[tokio::test]
    async fn legacy_and_orphaned_collections_are_retired() {
        let td = tempfile::tempdir().unwrap();
        let path = datalib_runtime::qmd::qmd_index_path(td.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE store_collections (name TEXT PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        for name in ["mirror", "slack_imbue", "deleted_source"] {
            sqlx::query("INSERT INTO store_collections (name) VALUES (?)")
                .bind(name)
                .execute(&pool)
                .await
                .unwrap();
        }
        pool.close().await;

        let mut retire = collections_to_retire(td.path(), &["slack_imbue".to_string()]).await;
        retire.sort();
        assert_eq!(
            retire,
            vec!["deleted_source".to_string(), "mirror".to_string()]
        );
    }
}
