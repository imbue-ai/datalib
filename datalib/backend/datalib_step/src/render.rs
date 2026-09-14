//! The render step driver: one source's render wave, written to the tree
//! the step id names and read from the raw store its input names.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::processor::{Input, RenderCtx, RenderProcessor};
use datalib_schema::render_cursor::RenderCursorRow;

use crate::dispatch::{PlannedSource, Wave};
use crate::events::{Emitter, OutputClaim};
use crate::source::StepEnv;
use datalib_etl_render::indexed_markdown::{blocking, IndexedMarkdownStore};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    planned: PlannedSource,
    env: &StepEnv,
    raw_rel: &str,
    data_root: &Path,
    now: &str,
    emitter: &Emitter,
    control: &datalib_etl::control::DownloadControl,
) -> Result<Vec<OutputClaim>> {
    // A render store is a doltlite store, and a consumer that pins a commit
    // reads a stable view of it while this step keeps writing. That is P2 of
    // the sink contract, and it is what lets `grid_index` start before this
    // finishes. Said before any work, so the runner knows by the time the
    // first checkpoint arrives.
    emitter.declare_streams_output(true);
    let PlannedSource {
        name,
        processors,
        raw_path,
        ..
    } = planned;
    let Wave::Render(processors) = processors else {
        anyhow::bail!("the render driver was handed source {name:?}'s ingest wave");
    };
    let progress = emitter.progress();
    // The providers write under `render_markdown_root(data_root, name)`;
    // `StepEnv::from_env` checked that this is the same tree as the id.
    let rendered_root = data_root.join(&env.step);
    // What the source's mirror weighs. Measured out here because the
    // scan is async and `blocking()` cannot drive a future from inside
    // the `spawn_blocking` thread below.
    let measured = crate::introspect::scan(data_root, raw_rel)
        .await
        .with_context(|| format!("measure {}", name))?;
    // Every source gets a storage report, including the ones that
    // render no documents of their own — for `fsindex` and `media` it
    // is the only thing they put in the grid.
    let storage = crate::introspect::plan(data_root, &name, &env.step, measured, now)?;

    let source = RenderSource {
        name: name.clone(),
        data_root: data_root.to_path_buf(),
        rendered_root: rendered_root.clone(),
        // The run-pinned "now" (`--now` / `$DATALIB_DAG_NOW`), so every
        // row this render stamps carries one timestamp rather than each
        // renderer sampling its own clock.
        now: now.to_string(),
        cadence: control.checkpoint_cadence.unwrap_or_default(),
        storage,
        raw_db: Some(datalib_etl::doltlite_raw::db_path_for(&raw_path)),
        progress: progress.clone(),
    };
    // Render is synchronous work driven by `futures`' executor (NOT
    // tokio's — providers block_on their own internal futures); run it
    // on a blocking thread.
    let report = tokio::task::spawn_blocking(move || render_source(&processors, source))
        .await
        .context("render task panicked")??;

    tracing::info!(
        docs = report.docs,
        removed = report.removed,
        "render: docs (re)rendered"
    );
    progress.metric("documents_removed", &[], report.removed as i64);
    if report.removed > 0 {
        progress.set_message(&format!(
            "{} document(s) dropped — their source is gone upstream",
            report.removed
        ));
    }
    // Say out loud what the sink holds. A problem store nothing ever
    // reads is indistinguishable from one that is empty because
    // everything is fine — and the more dangerous of those two reads as
    // success. These are whole-store counts, not this-run counts: a
    // problem on a document this run skipped is still current, which is
    // the point of the per-document sweep.
    if !report.problems.is_empty() {
        let total: i64 = report.problems.values().sum();
        let dropped = report.problems.get("dropped").copied().unwrap_or(0);
        let nulled = report.problems.get("nulled").copied().unwrap_or(0);
        tracing::warn!(
            source = %name,
            total,
            dropped,
            nulled,
            "render: rows this source could not fully project \
             (see render_problems in its indexed_markdown.doltlite_db)"
        );
        progress.set_message(&format!(
            "{total} row(s) with render problems ({dropped} dropped, {nulled} degraded)"
        ));
    }
    // The whole tree re-renders from the raw store, so cache-aware
    // backups (`restic --exclude-caches` etc.) may skip it. No-op until
    // the first render materializes the dir.
    datalib_core::layout::mark_derived_cache(&rendered_root);
    // The store's HEAD is the tree's content version: doltlite advances
    // it only when a commit changed something, so a run that rewrote
    // nothing reports the same string. Without doltlite there is nothing
    // content-derived to vouch for, and the runner hashes the tree.
    Ok(report
        .head
        .map(|h| OutputClaim {
            path: env.step.clone(),
            version: format!("store:{h}"),
            rows: Some(report.unsealed),
        })
        .into_iter()
        .collect())
}

/// One source's render, as the core takes it: everything the step shell
/// resolved from its environment, and nothing the step protocol knows.
pub struct RenderSource {
    pub name: String,
    pub data_root: PathBuf,
    /// The tree the store lives under — `<data_root>/<group>/render_markdown`.
    pub rendered_root: PathBuf,
    /// The run-pinned instant every stamp this render writes carries.
    pub now: String,
    /// The user's latency/history dial, the same one downloads take.
    pub cadence: datalib_etl::checkpointer::Cadence,
    /// The storage report to write beside the documents, if the source
    /// has one.
    pub storage: Option<crate::introspect::Measured>,
    /// The raw doltlite store the source renders from, if it has one:
    /// what the driver diffs for the reverse lookup in `render_inputs`.
    pub raw_db: Option<PathBuf>,
    pub progress: Progress,
}

/// What a render left behind, for the shell to report.
pub struct RenderReport {
    /// Documents written.
    pub docs: usize,
    pub removed: usize,
    /// Whole-store problem counts by outcome.
    pub problems: HashMap<String, i64>,
    /// The store's HEAD after the final commit. `None` without doltlite.
    pub head: Option<String>,
    /// Rows the final commit sealed beyond the last checkpoint — the
    /// last segment of a consumer's queue.
    pub unsealed: u64,
}

/// The render itself: decide whether to diff or render everything, run
/// the processors with their documents batched into one transaction per
/// checkpoint, sweep what a full walk did not produce, record the cursor,
/// commit. Synchronous, and the whole of what the render step guarantees
/// — the shell above adds only the step protocol around it.
pub fn render_source(
    processors: &[Box<dyn RenderProcessor>],
    source: RenderSource,
) -> Result<RenderReport> {
    let RenderSource {
        name,
        data_root,
        rendered_root,
        now,
        cadence,
        storage,
        raw_db,
        progress,
    } = source;
    let declared = declared_render_versions(processors);
    let declared_params = declared_render_params(processors);
    let declared_params_text = declared_params.to_string();
    let store = IndexedMarkdownStore::open(&rendered_root)
        .map(|s| s.with_now(&now))
        .with_context(|| format!("open render store for {}", name))?;
    let on_disk = store.render_versions()?;
    let stored_cursor = store.cursor()?;

    // Everything again, in place: a renderer version or a param change
    // means every document is rendered fresh and the ones the walk did
    // not produce are swept at the end. The store — its history, and
    // the commit `grid_index` last consumed — is kept, so a re-keyed
    // document reaches the index as a deletion plus an addition rather
    // than as an old row nobody ever removes.
    let plan = RenderPlan::decide(
        stored_cursor.as_ref(),
        &declared_params,
        tree_is_from_an_older_renderer(&on_disk, declared.as_ref()),
    );
    if let RenderPlan::Everything(why) = &plan {
        progress.set_message(&format!("{why}; re-rendering this source in full"));
        tracing::info!(source = %name, why, "render: rendering every document");
    }
    let render_everything = matches!(plan, RenderPlan::Everything(_));
    let raw_cursor: Option<String> = match plan {
        RenderPlan::FromCursor(from) => from,
        RenderPlan::Everything(_) => None,
    };
    tracing::info!(
        source = %name,
        cursor = raw_cursor.as_deref().unwrap_or("none"),
        "render: starting"
    );
    let (raw_pin, stale_buckets) =
        reverse_lookup(&store, raw_db.as_deref(), raw_cursor.as_deref())?;

    let mut checkpointer = datalib_etl::checkpointer::Checkpointer::new(
        datalib_etl::checkpointer::Policy::Every(cadence),
    );
    let mut docs = 0usize;
    let mut removed = 0usize;
    // Every document this run emitted. On a full render it is what the
    // walk produced, and the sweep below keeps exactly this.
    let mut emitted: BTreeSet<String> = BTreeSet::new();
    // The documents between two checkpoints share one SQL transaction
    // (the batch), and each is written whole inside it — rows, edges,
    // markdown and problems together — so a commit landing between
    // two documents never publishes a fraction of one. The providers
    // hand us a `RenderedMarkdown` through `ctx.emit_doc`, the same
    // value `grid_index::apply_one` consumes.
    store.begin_batch()?;
    let mut on_doc = |md: RenderedMarkdown| -> Result<()> {
        // Every emitted document is written. One that came out the same
        // writes the same rows, and doltlite's content-addressed tables
        // then carry no diff for it — nothing here decides "unchanged".
        store
            .put_document(&data_root, &md)
            .with_context(|| format!("store document {}", md.markdown_uuid))?;
        emitted.insert(md.markdown_uuid);
        docs += 1;
        progress.metric("documents_rendered", &[], docs as i64);
        // What a consumer reading a checkpoint may see is a document
        // this run is about to sweep. That is stale, not torn: the
        // sweep's deletions reach the consumer through the same diff
        // on its next pass.
        checkpointer.wrote(1);
        if checkpointer.should_seal() {
            store.commit_batch()?;
            let sealed =
                store.commit(&format!("render {name}: checkpoint at {docs} document(s)"))?;
            let rows = checkpointer.pending();
            checkpointer.sealed();
            // `None` means nothing was dirty after all, so no
            // version moved and there is nothing to announce.
            if let Some(hash) = sealed {
                progress.checkpoint_rows(&hash, rows);
            }
            store.begin_batch()?;
        }
        Ok(())
    };
    // The other half of the sink: a conversation the raw store no
    // longer has takes its rendered documents with it. Without this
    // the deletion stops at the raw store — the `.md` stays on disk
    // and `grid_index`, which only ever learns of a removal from
    // this store's own diff, never hears about it.
    let mut on_remove = |conversation_uuid: &str| -> Result<usize> {
        let gone = store.documents_for_conversation(conversation_uuid)?;
        for uuid in &gone {
            store
                .remove_document(&data_root, uuid)
                .with_context(|| format!("remove document {uuid}"))?;
        }
        if !gone.is_empty() {
            removed += gone.len();
            tracing::info!(
                conversation = conversation_uuid,
                documents = gone.len(),
                "render: conversation is gone from the raw store; dropped its documents",
            );
        }
        Ok(gone.len())
    };
    // The buckets this run rendered. What the store holds under one of
    // them that this run did not emit is gone. Their inputs land in the
    // open batch beside their documents.
    let mut buckets: BTreeSet<String> = BTreeSet::new();
    let mut on_declare = |bucket: &str, inputs: &[Input]| -> Result<()> {
        store
            .put_inputs(bucket, inputs)
            .with_context(|| format!("record inputs of bucket {bucket}"))?;
        buckets.insert(bucket.to_string());
        Ok(())
    };
    // The raw commit each processor rendered from, or `None` for one
    // that read no store.
    let mut consumed: Vec<Option<String>> = Vec::with_capacity(processors.len());
    let ran = (|| -> Result<()> {
        for proc in processors {
            let ctx = RenderCtx::new(
                &name,
                &data_root,
                &now,
                &progress,
                raw_cursor.as_deref(),
                raw_pin.as_deref(),
                stale_buckets.as_ref(),
                &mut on_doc,
                &mut on_remove,
                &mut on_declare,
            );
            futures::executor::block_on(proc.run(&ctx))
                .with_context(|| format!("processor {}", proc.id()))?;
            consumed.push(ctx.consumed_commit());
        }
        Ok(())
    })();
    // A failed processor takes the open batch with it: what it wrote
    // since the last checkpoint is neither complete nor described by
    // any cursor, and the next run renders it again.
    if let Err(e) = ran {
        let _ = store.rollback_batch();
        return Err(e);
    }
    store.commit_batch()?;
    let raw_commit = one_consumed_commit(&name, &consumed);

    // A full render in which every processor read its store walked
    // everything, so whatever it did not produce is gone. A processor
    // that read nothing (no raw store on disk, nothing committed) says
    // nothing about what should exist, and nothing is swept.
    let full_walk =
        render_everything && !consumed.is_empty() && consumed.iter().all(Option::is_some);
    let mut keep = emitted;
    // The storage report's id goes in `keep`: the provider's processors
    // know nothing about it, and the sweep would otherwise drop it on
    // any run where no number moved.
    if let Some(m) = storage.as_ref() {
        keep.insert(m.doc.markdown_uuid.clone());
    }

    // The sweep runs only on a run that got through every processor
    // — a render that failed partway named a fraction of what it
    // holds, and `?` above already returned.
    let sealed = seal_run(
        &store,
        &data_root,
        RunEnd {
            sweep: full_walk,
            keep: &keep,
            declared: &buckets,
            storage,
            // The cursor is rewritten only when it moves: its row carries
            // a per-run stamp, and rewriting it unchanged would give
            // every steady-state run a commit.
            cursor: raw_commit
                .filter(|raw_commit| {
                    stored_cursor.as_ref().is_none_or(|c| {
                        c.raw_commit != *raw_commit || c.params != declared_params_text
                    })
                })
                .map(|raw_commit| {
                    let stamp = datalib_time::split_stamp(&now);
                    RenderCursorRow {
                        source_id: name.clone(),
                        raw_commit,
                        params: declared_params_text.clone(),
                        rendered_at_utc: stamp.utc,
                        tz_offset: stamp.tz_offset,
                    }
                }),
        },
    )?;
    docs += sealed.stored;
    removed += sealed.removed;
    if !buckets.is_empty() {
        tracing::info!(
            source = %name,
            buckets = buckets.len(),
            "render: buckets declared with their inputs"
        );
    }

    // One commit for the whole render. Per-document commits would
    // put thousands of entries in `dolt_log` per run; committing
    // once is also what makes `dolt_diff` over this store answer
    // "what did this render change?".
    let msg = if removed == 0 {
        format!("render {name}: {docs} document(s)")
    } else {
        format!("render {name}: {docs} document(s), {removed} removed upstream")
    };
    store
        .commit(&msg)
        .with_context(|| format!("commit render store for {}", name))?;
    // Read back from the store that just wrote them, before `close`
    // consumes it.
    let versions = store.render_versions()?;
    let problems = store.problem_counts()?;
    let head = store.head()?;
    store.close();
    every_stored_version_must_be_declared(&name, &rendered_root, &versions, declared.as_ref())?;
    Ok(RenderReport {
        docs,
        removed,
        problems,
        head,
        // What the final commit sealed beyond the last checkpoint.
        unsealed: checkpointer.pending(),
    })
}

/// What closes a run: the sweep (`Some(keep)` deletes every document not
/// in it), the storage report, and the cursor to record.
struct RunEnd<'a> {
    /// Whether the run walked everything, so a document not in `keep` is
    /// one the source no longer produces.
    sweep: bool,
    /// Every document this run emitted or owns.
    keep: &'a BTreeSet<String>,
    /// Buckets the run rendered: what the store holds under them beyond
    /// `keep` is gone.
    declared: &'a BTreeSet<String>,
    storage: Option<crate::introspect::Measured>,
    cursor: Option<RenderCursorRow>,
}

struct Sealed {
    stored: usize,
    removed: usize,
}

/// The sweep, the storage report and the cursor land as one transaction,
/// so the cursor can never claim a range the store's rows do not reflect.
fn seal_run(store: &IndexedMarkdownStore, data_root: &Path, end: RunEnd<'_>) -> Result<Sealed> {
    store.transaction(|| {
        let mut sealed = Sealed {
            stored: 0,
            removed: 0,
        };
        if end.sweep {
            for uuid in store.all_document_uuids()? {
                if end.keep.contains(&uuid) {
                    continue;
                }
                store
                    .remove_document(data_root, &uuid)
                    .with_context(|| format!("remove document {uuid}"))?;
                sealed.removed += 1;
                tracing::info!(
                    document = %uuid,
                    "render: this source no longer produces this document; dropped it",
                );
            }
        }
        // A bucket the run rendered produces exactly what it emitted; a
        // document the store still holds for it is from a period that
        // emptied or a thread whose messages went. Positive evidence
        // only: buckets the run never looked at are not here.
        let buckets: Vec<&str> = end.declared.iter().map(String::as_str).collect();
        for (bucket, uuid) in store.documents_for_buckets(&buckets)? {
            if end.keep.contains(&uuid) {
                continue;
            }
            store
                .remove_document(data_root, &uuid)
                .with_context(|| format!("remove document {uuid}"))?;
            sealed.removed += 1;
            tracing::info!(
                document = %uuid,
                bucket,
                "render: this bucket no longer produces this document; dropped it",
            );
        }
        // The report is skipped whole when no count moved: its byte
        // sizes wobble from run to run, so a rewrite would be a diff for
        // the index to re-read and a sample saying "still the same" for
        // the series to grow by. The counts it measured last time are
        // the store's own `source_measurements`.
        if let Some(m) = end.storage {
            let same_version =
                store.document_version(&m.doc.markdown_uuid)? == Some(m.doc.render_version);
            let same_counts =
                crate::introspect::counts_unchanged(&store.latest_items()?, &m.samples);
            if same_version && same_counts {
                tracing::debug!("render: storage unchanged since the last run");
            } else {
                m.write_report().context("write the storage report")?;
                store
                    .put_document(data_root, &m.doc)
                    .context("store storage report")?;
                store
                    .put_measurements(&m.samples)
                    .context("append measurements")?;
                sealed.stored += 1;
            }
        }
        if let Some(cursor) = &end.cursor {
            store.write_cursor(cursor)?;
        }
        Ok(sealed)
    })
}

/// The driver's half of the scan: the buckets whose declared inputs
/// changed between the cursor and the raw store's HEAD, from `dolt_diff`
/// over every table `render_inputs` mentions and a reverse lookup. Also
/// the commit that HEAD was, for the provider to pin. `(None, None)`
/// when there is nothing to say: no cursor, no raw doltlite store,
/// nothing declared yet, or a range this store cannot resolve — then the
/// provider's own scan decides alone, as it did before `render_inputs`.
fn reverse_lookup(
    store: &IndexedMarkdownStore,
    raw_db: Option<&Path>,
    raw_cursor: Option<&str>,
) -> Result<(Option<String>, Option<std::collections::HashSet<String>>)> {
    let (Some(from), Some(raw_db)) = (raw_cursor, raw_db) else {
        return Ok((None, None));
    };
    if !raw_db.exists() {
        return Ok((None, None));
    }
    let tables = store.input_tables()?;
    if tables.is_empty() {
        return Ok((None, None));
    }
    let pool = blocking(datalib_etl::doltlite_raw::open_reader(raw_db))
        .with_context(|| format!("open {} for the reverse lookup", raw_db.display()))?;
    let result = (|| -> Result<_> {
        let Some(pin) = blocking(datalib_etl::pin::head(&pool))? else {
            return Ok((None, None));
        };
        let to = pin.commit().to_string();
        let mut changed: Vec<Input> = Vec::new();
        for table in &tables {
            match blocking(datalib_etl::doltlite_raw::changed_keys(
                &pool, table, from, &to,
            )) {
                Ok(keys) => changed.extend(keys.into_iter().map(|k| Input::new(table.clone(), k))),
                // A cursor this store cannot resolve — the file was
                // replaced by hand — or a table a schema change dropped.
                // Either way the range is gone; the provider cold-starts.
                Err(e) => {
                    tracing::warn!(
                        table,
                        from,
                        error = %format!("{e:#}"),
                        "render: reverse lookup could not diff this table; leaving the scan to the provider"
                    );
                    return Ok((Some(to), None));
                }
            }
        }
        let stale = store.buckets_reading(&changed)?;
        tracing::info!(
            changed_rows = changed.len(),
            stale_buckets = stale.len(),
            "render: reverse lookup"
        );
        Ok((Some(to), Some(stale)))
    })();
    blocking(pool.close());
    result
}

/// Whether this run diffs from the stored cursor or renders every bucket.
#[derive(Debug, PartialEq, Eq)]
enum RenderPlan {
    /// Diff from this raw-store commit — or from nothing, when there is
    /// no cursor yet: the provider then reads its whole store, which is
    /// also the steady state of a renderer that never records a cursor.
    FromCursor(Option<String>),
    /// Render everything, for the reason given. The stored cursor is
    /// kept: the sweep at the end of the run is what removes documents
    /// the new version or params no longer produce, and the range is
    /// still the one `grid_index` diffs the store over.
    Everything(&'static str),
}

impl RenderPlan {
    fn decide(
        stored: Option<&RenderCursorRow>,
        declared_params: &serde_json::Value,
        version_changed: bool,
    ) -> RenderPlan {
        if version_changed {
            return RenderPlan::Everything("renderer version changed");
        }
        let Some(stored) = stored else {
            return RenderPlan::FromCursor(None);
        };
        let stored_params: serde_json::Value =
            serde_json::from_str(&stored.params).unwrap_or(serde_json::Value::Null);
        if &stored_params != declared_params {
            return RenderPlan::Everything("render params changed");
        }
        RenderPlan::FromCursor(Some(stored.raw_commit.clone()))
    }
}

/// Every processor's params under its id, so one source's cursor carries
/// all of them and a change to any one re-renders the source.
fn declared_render_params(processors: &[Box<dyn RenderProcessor>]) -> serde_json::Value {
    processors
        .iter()
        .map(|p| (p.id().to_string(), p.render_params()))
        .collect::<serde_json::Map<String, serde_json::Value>>()
        .into()
}

/// The one raw commit this run rendered from. Every processor of a source
/// reads the same store, so they agree unless a writer committed between
/// their pins; then the earliest report wins, since a cursor past what any
/// processor rendered would skip a range.
fn one_consumed_commit(source: &str, consumed: &[Option<String>]) -> Option<String> {
    let mut reported = consumed.iter().flatten();
    let first = reported.next().cloned()?;
    for other in reported {
        if *other != first {
            tracing::warn!(
                source,
                first,
                other,
                "render: processors pinned different raw commits; the cursor takes the first"
            );
        }
    }
    Some(first)
}

fn tree_is_from_an_older_renderer(
    on_disk: &BTreeSet<u32>,
    current: Option<&BTreeSet<u32>>,
) -> bool {
    let Some(current) = current else {
        return false;
    };
    if on_disk.is_empty() || on_disk.is_subset(current) {
        return false;
    }
    tracing::warn!(
        ?on_disk,
        ?current,
        "render: rendered tree came from a different renderer version; \
         rendering every document again and sweeping what the walk does not produce"
    );
    true
}

fn every_stored_version_must_be_declared(
    source: &str,
    rendered_root: &Path,
    on_disk: &BTreeSet<u32>,
    declared: Option<&BTreeSet<u32>>,
) -> Result<()> {
    if on_disk.is_empty() {
        return Ok(());
    }
    let Some(declared) = declared else {
        anyhow::bail!(
            concat!(
                "source `{source}` wrote rendered documents (render_version {on_disk:?}) ",
                "but none of its processors implement `DataProcessor::render_version`. Every ",
                "renderer must: it is what lets the next run tell a tree this build produced ",
                "from one an older build left behind, and a tree whose ids were re-keyed cannot ",
                "be merged into, only replaced. Return the same constant the render path passes ",
                "into each `RenderedMarkdown`."
            ),
            source = source,
            on_disk = on_disk,
        );
    };
    let undeclared: Vec<u32> = on_disk.difference(declared).copied().collect();
    if !undeclared.is_empty() {
        anyhow::bail!(
            concat!(
                "source `{source}`: documents under {root} carry render_version {undeclared:?}, ",
                "which none of its processors declare (declared: {declared:?}). Either a ",
                "processor reports one version and writes another — which would re-render this ",
                "source in full on every run — or the source's raw store could not be read on ",
                "the run that was meant to replace the older documents, so they are still here."
            ),
            source = source,
            root = rendered_root.display(),
            undeclared = undeclared,
            declared = declared,
        );
    }
    Ok(())
}

fn declared_render_versions(processors: &[Box<dyn RenderProcessor>]) -> Option<BTreeSet<u32>> {
    let versions: BTreeSet<u32> = processors
        .iter()
        .map(|p| p.render_version())
        .collect::<Option<_>>()?;
    (!versions.is_empty()).then_some(versions)
}

#[cfg(test)]
mod plan_tests {
    use super::*;
    use serde_json::json;

    fn cursor(raw_commit: &str, params: serde_json::Value) -> RenderCursorRow {
        RenderCursorRow {
            source_id: "src".into(),
            raw_commit: raw_commit.into(),
            params: params.to_string(),
            rendered_at_utc: "2026-01-01T00:00:00.000000Z".into(),
            tz_offset: Some("+00:00".into()),
        }
    }

    /// The steady state: the same params as last time diff from the
    /// stored commit.
    #[test]
    fn unchanged_params_diff_from_the_stored_commit() {
        let stored = cursor("commit-a", json!({"p": {"period": "month"}}));
        assert_eq!(
            RenderPlan::decide(Some(&stored), &json!({"p": {"period": "month"}}), false),
            RenderPlan::FromCursor(Some("commit-a".into()))
        );
    }

    /// A param change re-renders everything and does not forget where
    /// it was — "render every bucket" and "lose the range" are different
    /// requests, and only the first is wanted here.
    #[test]
    fn a_param_change_renders_everything() {
        let stored = cursor("commit-a", json!({"p": {"period": "month"}}));
        assert_eq!(
            RenderPlan::decide(Some(&stored), &json!({"p": {"period": "day"}}), false),
            RenderPlan::Everything("render params changed")
        );
    }

    /// A renderer version bump is the same request, and it wins over a
    /// usable cursor.
    #[test]
    fn a_version_bump_renders_everything() {
        let stored = cursor("commit-a", json!({}));
        assert_eq!(
            RenderPlan::decide(Some(&stored), &json!({}), true),
            RenderPlan::Everything("renderer version changed")
        );
    }

    /// No cursor is not "render everything": a renderer that never
    /// records one — perseus, reading files — sweeps through the bucket
    /// it declares, and the driver's full-walk sweep must not run over
    /// it.
    #[test]
    fn no_cursor_is_not_a_full_render() {
        assert_eq!(
            RenderPlan::decide(None, &json!({}), false),
            RenderPlan::FromCursor(None)
        );
    }

    fn write_doc(root: &Path, uuid: &str) {
        let store = IndexedMarkdownStore::open(root).unwrap();
        let row = datalib_schema::grid_rows::GridRow::builder()
            .uuid(uuid)
            .provider(datalib_schema::providers::Provider::Test)
            .kind("Test")
            .source_label("Test")
            .conversation_uuid(uuid)
            .entire_chat(format!("/chat/{uuid}"))
            .text("body")
            .markdown_uuid(Some(uuid.to_string()))
            .build()
            .unwrap();
        store
            .put_document(
                root,
                &RenderedMarkdown {
                    markdown_uuid: uuid.to_string(),
                    source_id: "src".into(),
                    upstream_cursor: None,
                    bucket_key: None,
                    md_path: root.join(uuid).join("all.md"),
                    render_version: 5,
                    rows: vec![row],
                    edges: Vec::new(),
                    problems: Vec::new(),
                },
            )
            .unwrap();
        store.close();
    }

    /// A full render sweeps what the walk did not produce and records
    /// the cursor with it: the store afterwards holds exactly the
    /// emitted set, and the cursor names the commit the walk consumed.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_full_walk_sweeps_what_it_did_not_produce_and_records_the_cursor() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("src/render_markdown");
        write_doc(&root, "kept");
        write_doc(&root, "stale");

        let store = IndexedMarkdownStore::open(&root).unwrap();
        let keep: BTreeSet<String> = ["kept".to_string()].into_iter().collect();
        let sealed = seal_run(
            &store,
            td.path(),
            RunEnd {
                sweep: true,
                keep: &keep,
                declared: &BTreeSet::new(),
                storage: None,
                cursor: Some(cursor("raw-head", json!({}))),
            },
        )
        .unwrap();
        assert_eq!(sealed.removed, 1);
        assert_eq!(
            store.all_document_uuids().unwrap(),
            vec!["kept".to_string()]
        );
        assert_eq!(
            store.cursor().unwrap().map(|c| c.raw_commit).as_deref(),
            Some("raw-head")
        );
        store.close();
    }

    /// No sweep: an incremental run, or a full render in which a
    /// processor read no store, leaves every document alone.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_without_a_sweep_deletes_nothing() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("src/render_markdown");
        write_doc(&root, "a");
        write_doc(&root, "b");

        let store = IndexedMarkdownStore::open(&root).unwrap();
        let sealed = seal_run(
            &store,
            td.path(),
            RunEnd {
                sweep: false,
                keep: &BTreeSet::new(),
                declared: &BTreeSet::new(),
                storage: None,
                cursor: None,
            },
        )
        .unwrap();
        assert_eq!(sealed.removed, 0);
        assert_eq!(store.all_document_uuids().unwrap().len(), 2);
        assert!(store.cursor().unwrap().is_none());
        store.close();
    }

    #[test]
    fn the_cursor_takes_the_first_reported_commit() {
        assert_eq!(one_consumed_commit("src", &[]), None);
        assert_eq!(one_consumed_commit("src", &[None]), None);
        assert_eq!(
            one_consumed_commit("src", &[None, Some("a".into()), Some("b".into())]),
            Some("a".into())
        );
    }
}

#[cfg(test)]
mod stale_tree_tests {
    //! A rendered tree written by a different renderer version is
    //! rendered again in full, and every stored version has to be one a
    //! processor declares.

    use std::collections::BTreeSet;
    use std::path::Path;

    use anyhow::Result;
    use datalib_etl_render::grid_index::RenderedMarkdown;
    use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;
    use datalib_etl_render::processor::{RenderCtx, RenderProcessor};
    use datalib_schema::grid_rows::GridRow;

    use super::{
        declared_render_versions, every_stored_version_must_be_declared,
        tree_is_from_an_older_renderer,
    };
    use datalib_schema::providers::Provider;

    fn write_doc(root: &Path, chat_uuid: &str, version: u32) {
        let store = IndexedMarkdownStore::open(root).unwrap();
        let row = GridRow::builder()
            .uuid(chat_uuid)
            .provider(Provider::Test)
            .kind("Test")
            .source_label("Test")
            .conversation_uuid(chat_uuid)
            .entire_chat(format!("/chat/{chat_uuid}"))
            .text("body")
            .markdown_uuid(Some(chat_uuid.to_string()))
            .build()
            .unwrap();
        store
            .put_document(
                root,
                &RenderedMarkdown {
                    markdown_uuid: chat_uuid.to_string(),
                    source_id: "claude_web".into(),
                    upstream_cursor: None,
                    bucket_key: None,
                    md_path: root.join(chat_uuid).join("all.md"),
                    render_version: version,
                    rows: vec![row],
                    edges: Vec::new(),
                    problems: Vec::new(),
                },
            )
            .unwrap();
        store.close();
    }

    fn stored_versions(root: &Path) -> BTreeSet<u32> {
        let store = IndexedMarkdownStore::open(root).unwrap();
        let v = store.render_versions().unwrap();
        store.close();
        v
    }

    fn document_count(root: &Path) -> usize {
        let store = IndexedMarkdownStore::open(root).unwrap();
        let n = store.all_document_uuids().unwrap().len();
        store.close();
        n
    }

    fn versions(vs: &[u32]) -> BTreeSet<u32> {
        vs.iter().copied().collect()
    }

    /// A tree at an older version is rendered again in full. The store
    /// itself is kept — its history is what `grid_index` diffs over, so
    /// a re-keyed document reaches the index as a deletion plus an
    /// addition rather than as an old row nobody removes.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tree_from_an_older_renderer_is_rendered_again_in_place() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("claude_web/render_markdown");
        write_doc(&root, "old-uuid", 4);
        assert_eq!(document_count(&root), 1, "the fixture must be readable");

        let on_disk = stored_versions(&root);
        assert!(tree_is_from_an_older_renderer(
            &on_disk,
            Some(&versions(&[5]))
        ));
        assert!(root.exists(), "the store is kept, not discarded");
    }

    /// A tree at the current version is left alone. Without this, every
    /// run would delete and re-render the whole source — correct output,
    /// and the incrementality silently gone.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_current_tree_is_kept() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("claude_web/render_markdown");
        write_doc(&root, "uuid-a", 5);
        write_doc(&root, "uuid-b", 5);

        let on_disk = stored_versions(&root);
        assert!(!tree_is_from_an_older_renderer(
            &on_disk,
            Some(&versions(&[5]))
        ));
        assert_eq!(document_count(&root), 2);
    }

    /// An empty tree — a first run — is not "stale".
    #[tokio::test(flavor = "multi_thread")]
    async fn a_first_run_has_nothing_to_discard() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("claude_web/render_markdown");
        assert!(!tree_is_from_an_older_renderer(
            &stored_versions(&root),
            Some(&versions(&[5]))
        ));
    }

    /// A source whose processors don't all declare a version deletes
    /// nothing, whatever is in its tree — acting on a partial
    /// declaration is how you delete a live document. The run still
    /// fails, at the post-render check rather than here.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_undeclared_version_deletes_nothing() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("claude_web/render_markdown");
        write_doc(&root, "old-uuid", 4);
        assert!(!tree_is_from_an_older_renderer(
            &stored_versions(&root),
            None
        ));
        assert!(root.exists());
    }

    /// A renderer that writes documents and declares nothing fails the
    /// step rather than silently opting its source out of the staleness
    /// check. This is the assertion that makes the trait method
    /// mandatory in practice — without it, "every provider declares one"
    /// is a convention that a new provider breaks by doing nothing.
    #[test]
    fn writing_documents_without_declaring_a_version_is_an_error() {
        let err = every_stored_version_must_be_declared(
            "claude_web",
            Path::new("/tmp/claude_web/render_markdown"),
            &versions(&[5]),
            None,
        )
        .expect_err("a source with documents and no declaration must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("claude_web"), "{msg}");
        assert!(msg.contains("render_version"), "{msg}");
    }

    /// Declaring one version and writing another fails on the first run,
    /// naming both. Left undetected it re-renders the source from
    /// scratch forever, which produces correct output and so shows up
    /// only as a pipeline that stopped being incremental.
    #[test]
    fn declaring_a_version_the_renderer_does_not_write_is_an_error() {
        let err = every_stored_version_must_be_declared(
            "claude_web",
            Path::new("/tmp/claude_web/render_markdown"),
            &versions(&[4]),
            Some(&versions(&[5])),
        )
        .expect_err("declared 5, wrote 4");
        let msg = format!("{err:#}");
        assert!(msg.contains('4') && msg.contains('5'), "{msg}");
    }

    /// A source that declares correctly passes, and one that rendered
    /// nothing at all has nothing to check.
    #[test]
    fn a_matching_declaration_and_an_empty_tree_both_pass() {
        every_stored_version_must_be_declared(
            "claude_web",
            Path::new("/tmp/claude_web/render_markdown"),
            &versions(&[5]),
            Some(&versions(&[5])),
        )
        .expect("declared 5, wrote 5");
        every_stored_version_must_be_declared(
            "empty",
            Path::new("/tmp/empty/render_markdown"),
            &BTreeSet::new(),
            None,
        )
        .expect("nothing written, nothing to declare");
    }

    struct Stub(Option<u32>);

    #[async_trait::async_trait]
    impl RenderProcessor for Stub {
        fn id(&self) -> &str {
            "stub"
        }
        async fn run(&self, _ctx: &RenderCtx<'_>) -> Result<String> {
            Ok(String::new())
        }
        fn render_version(&self) -> Option<u32> {
            self.0
        }
    }

    /// One abstaining processor disables the check for the whole
    /// source. Reporting only its siblings' versions would make that
    /// processor's own documents look foreign, so the tree — including
    /// the documents it had just written — would be deleted and
    /// re-rendered on every single run.
    #[test]
    fn one_abstaining_processor_disables_the_check_for_the_source() {
        let mixed: Vec<Box<dyn RenderProcessor>> =
            vec![Box::new(Stub(Some(5))), Box::new(Stub(None))];
        assert_eq!(declared_render_versions(&mixed), None);

        let all_declared: Vec<Box<dyn RenderProcessor>> =
            vec![Box::new(Stub(Some(5))), Box::new(Stub(Some(5)))];
        assert_eq!(
            declared_render_versions(&all_declared),
            Some(versions(&[5]))
        );

        assert_eq!(declared_render_versions(&[]), None);
    }
}
