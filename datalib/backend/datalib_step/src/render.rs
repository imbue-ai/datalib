//! The render step driver: one source's render processors, written to
//! the tree the step id names and read from the raw store its input
//! names.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::pages::PageSize;
use datalib_etl_render::processor::{Input, ReadScope, RenderCtx, RenderProcessor, RenderUnitEnd};
use datalib_schema::problems::{ProblemRow, Severity, Stage, METRIC};
use datalib_schema::render_cursor::RenderCursorRow;

use crate::dispatch::{PlannedSource, Wave};
use crate::events::{Emitter, OutputClaim};
use crate::source::StepEnv;
use datalib_etl_render::indexed_markdown::{blocking, Holdings, IndexedMarkdownStore, WHOLE_TABLE};
use datalib_sql::sql;

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
    // reads a stable view of it while this step keeps writing. That is P2
    // in `datalib/backend/dag/README.md` §"What a sink owes its consumers",
    // and it is what lets `unified_grid_and_search_index` start before this
    // finishes. Said before any work, so the runner knows by the time the
    // first checkpoint arrives.
    emitter.declare_streams_output(true);
    let PlannedSource {
        name,
        source_type,
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
    let item_table = source_type
        .item_table()
        .map(|t| (t, crate::introspect::table_rows(&measured, t)));
    // Every source gets a storage report, including the ones that
    // render no documents of their own — for `file_inventory` and `media` it
    // is the only thing they put in the grid.
    let storage = crate::introspect::plan(&name, measured, now)?;
    let raw_db = datalib_etl::doltlite_raw::db_path_for(&raw_path);
    require_raw_shape(&raw_db, source_type.as_str()).await?;

    let source = RenderSource {
        name: name.clone(),
        data_root: data_root.to_path_buf(),
        rendered_root: rendered_root.clone(),
        // The run-pinned "now" (`--now` / `$DATALIB_DAG_NOW`), so every
        // row this render stamps carries one timestamp rather than each
        // renderer sampling its own clock.
        now: now.to_string(),
        cadence: control.checkpoint_cadence.unwrap_or_default(),
        page_size: page_size(),
        storage,
        raw_db: Some(raw_db),
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
        "docs (re)rendered"
    );
    progress.metric("documents_removed_total", &[], report.removed as i64);
    // The last word on what the source holds, after the sweep: a run
    // that deleted more than it wrote leaves the checkpoints' last
    // number too high, and this is the one that stands between runs.
    match item_table {
        None => report_holdings(&progress, report.holdings),
        Some((_, Some(rows))) => report_holdings(
            &progress,
            Holdings {
                items: rows,
                ..report.holdings
            },
        ),
        // Before its first download, or a mirrored library with no such
        // table: nothing counted, so no count is reported.
        Some((table, None)) => {
            tracing::info!(table, "no {table} table to count items in");
            progress.metric(datalib_metrics::DOCUMENTS, &[], report.holdings.documents);
        }
    }
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
    // the point of the per-document sweep. They leave out the
    // download's rows copied in beside render's own: the download's row
    // counts those. The metrics are what the
    // Manage row's errors/warnings cell reads, so they are reported
    // every run, zero included: a missing series means "never counted",
    // not "clean".
    let errors = report.problems.get(&Severity::Error).copied().unwrap_or(0);
    let warnings = report
        .problems
        .get(&Severity::Warning)
        .copied()
        .unwrap_or(0);
    progress.metric(METRIC, &[Severity::Error.metric_label()], errors);
    progress.metric(METRIC, &[Severity::Warning.metric_label()], warnings);
    if errors + warnings > 0 {
        tracing::debug!(
            source = %name,
            errors,
            warnings,
            "rows this source could not fully project \
             (see `problems` in its indexed_markdown.doltlite_db)"
        );
        progress.set_message(&format!(
            "{errors} error(s) and {warnings} warning(s) rendering this source"
        ));
    }
    Ok(claims(&env.step, &report))
}

/// The Manage screen's Documents and Items, as one pair so they never
/// disagree about which moment they describe.
pub(crate) fn report_holdings(progress: &Progress, h: Holdings) {
    progress.metric(datalib_metrics::DOCUMENTS, &[], h.documents);
    progress.metric(datalib_metrics::ITEMS, &[], h.items);
}

/// What a render reports: its store's content version, the hash of its
/// HEAD's tree. A run that rewrote nothing reports the same string, and it
/// is spelled as each seal spells its commit's tree, so finishing on the
/// commit last sealed moves nothing downstream. Without doltlite there is nothing content-derived
/// to vouch for, and every success reads as new.
pub fn claims(step: &str, report: &RenderReport) -> Vec<OutputClaim> {
    report
        .head
        .iter()
        .map(|h| OutputClaim {
            path: step.to_string(),
            version: h.clone(),
            rows: Some(report.unsealed),
        })
        .collect()
}

/// The config's `DATALIB_STEP_RENDER_PAGES` over the render's defaults.
/// One the step cannot read is logged and the defaults stand.
pub(crate) fn page_size() -> PageSize {
    const ENV: &str = datalib_env::DATALIB_STEP_RENDER_PAGES;
    let Ok(raw) = std::env::var(ENV) else {
        return PageSize::DEFAULT;
    };
    match decode_render_pages(&raw) {
        Some((render_units, blob_mb)) => PageSize {
            render_units: render_units.map_or(PageSize::DEFAULT.render_units, |n| n as usize),
            bytes: blob_mb.map_or(PageSize::DEFAULT.bytes, |mb| mb * 1024 * 1024),
        },
        None => {
            tracing::warn!(raw = %raw, "{ENV} is not readable; using the default page size");
            PageSize::DEFAULT
        }
    }
}

/// `<render units>,<blob_mb>`, either side empty for the render's default.
/// `None` for anything this build cannot read, a zero included.
fn decode_render_pages(s: &str) -> Option<(Option<u64>, Option<u64>)> {
    let (render_units, blob_mb) = s.trim().split_once(',')?;
    let side = |v: &str| -> Option<Option<u64>> {
        match v.trim() {
            "" => Some(None),
            n => n.parse::<u64>().ok().filter(|n| *n > 0).map(Some),
        }
    };
    Some((side(render_units)?, side(blob_mb)?))
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
    /// How much of the raw store each processor reads at once.
    pub page_size: PageSize,
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
    /// What the store holds afterwards, storage report excluded — what
    /// the source has, not what this run did.
    pub holdings: Holdings,
    /// Whole-store counts by severity of the problems render found,
    /// the download's copied rows left out.
    pub problems: HashMap<Severity, i64>,
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
        page_size,
        storage,
        raw_db,
        progress,
    } = source;
    let render_started = std::time::Instant::now();
    let writing_micros = std::sync::atomic::AtomicU64::new(0);
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
    // the commit `unified_grid_and_search_index` last consumed — is kept, so a re-keyed
    // document reaches the index as a deletion plus an addition rather
    // than as an old row nobody ever removes.
    let plan = RenderPlan::decide(
        stored_cursor.as_ref(),
        &declared_params,
        tree_is_from_an_older_renderer(&on_disk, declared.as_ref()),
    );
    if let RenderPlan::Everything(why) = &plan {
        progress.set_message(&format!("{why}; re-rendering this source in full"));
        let changed = stored_cursor
            .as_ref()
            .map(|c| changed_params(&c.params, &declared_params).join(", "))
            .unwrap_or_default();
        tracing::info!(source = %name, why, changed, "rendering every document");
    }
    let render_everything = matches!(plan, RenderPlan::Everything(_));
    let raw_cursor: Option<String> = match plan {
        RenderPlan::FromCursor(from) => from,
        RenderPlan::Everything(_) => None,
    };
    tracing::info!(
        source = %name,
        cursor = raw_cursor.as_deref().unwrap_or("none"),
        "starting the render"
    );
    // The diff runs from the stored cursor even on a full walk: the
    // walk renders everything, but what left since the cursor is still
    // the only evidence that a render unit declared with nothing is gone.
    let ReverseLookup {
        pin: raw_pin,
        stale,
        removed: removed_rows,
        bookkeeping_only,
    } = reverse_lookup(
        &store,
        raw_db.as_deref(),
        stored_cursor.as_ref().map(|c| c.raw_commit.as_str()),
    )?;
    let stale_render_units = stale.filter(|_| !render_everything);

    let mut checkpointer = datalib_etl::checkpointer::Checkpointer::new(cadence);
    let mut docs = 0usize;
    let mut removed = 0usize;
    // The storage report is datalib writing *about* the source, not a
    // document out of it, so it is left out of every count of what the
    // source holds. Taken now because `storage` itself moves into the
    // seal below.
    let storage_uuid = storage.as_ref().map(|m| m.doc.markdown_uuid.clone());
    // Every document this run emitted. On a full render it is what the
    // walk produced, and the sweep below keeps exactly this.
    let mut emitted: BTreeSet<String> = BTreeSet::new();
    let mut emitted_under: BTreeSet<String> = BTreeSet::new();
    // The documents between two checkpoints share one SQL transaction
    // (the batch), and each is written whole inside it — rows, edges,
    // markdown and problems together — so a commit landing between
    // two documents never publishes a fraction of one. The providers
    // hand us a `RenderedMarkdown` through `ctx.emit_doc`, the same
    // value `grid_index::apply_one` consumes.
    store.begin_batch()?;
    let mut on_doc = |md: RenderedMarkdown| -> Result<()> {
        let write_started = std::time::Instant::now();
        // Every emitted document is written. One that came out the same
        // writes the same rows, and doltlite's content-addressed tables
        // then carry no diff for it — nothing here decides "unchanged".
        store
            .put_document(&md)
            .with_context(|| format!("store document {}", md.markdown_uuid))?;
        emitted.insert(md.markdown_uuid);
        if let Some(render_unit) = md.render_unit_key {
            emitted_under.insert(render_unit);
        }
        docs += 1;
        progress.metric("documents_rendered_total", &[], docs as i64);
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
            // version moved and there is nothing to report.
            if let Some(hash) = sealed {
                progress.checkpoint_rows(&store.tree_of(&hash)?, rows);
            }
            // A seal is the one moment inside a run when everything
            // written so far is committed and nothing is half-written,
            // so it is where the whole-store count can be taken without
            // reading a torn batch. This is what keeps the Manage
            // screen's Documents column moving while a render runs;
            // between seals it stands still, which is honest.
            report_holdings(&progress, store.holdings(storage_uuid.as_deref())?);
            store.begin_batch()?;
        }
        writing_micros.fetch_add(
            write_started.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        Ok(())
    };
    // How each render unit the run looked at ended, decided at the seal. The
    // inputs of a built render unit land in the open batch beside its
    // documents; a render unit declared with none keeps the ones it had until
    // the seal decides it is gone, because the evidence that it is gone
    // is in them.
    let mut ends: BTreeMap<String, Ended> = BTreeMap::new();
    // Render units this run built that held no inputs before it: where a
    // re-keyed render unit's rows go.
    let mut put_this_run: HashSet<String> = HashSet::new();
    let mut new_render_units: HashSet<String> = HashSet::new();
    let running_version: Mutex<Option<u32>> = Mutex::new(None);
    let mut on_declare = |render_unit: &str, end: RenderUnitEnd<'_>| -> Result<()> {
        if let RenderUnitEnd::Read(inputs) = end {
            if !inputs.is_empty() {
                let had_inputs = store
                    .put_inputs(render_unit, inputs)
                    .with_context(|| format!("record inputs of render unit {render_unit}"))?;
                if put_this_run.insert(render_unit.to_string()) && !had_inputs {
                    new_render_units.insert(render_unit.to_string());
                }
            }
        }
        let ended = Ended::of(end, *running_version.lock().unwrap());
        // A failure is the render unit's last word: what else the run said
        // about it cannot make a failed build good.
        if !ends.get(render_unit).is_some_and(Ended::failed) {
            ends.insert(render_unit.to_string(), ended);
        }
        Ok(())
    };
    // Entity-scoped problems land in the open batch beside the
    // documents, so a checkpoint carries them and a failed processor's
    // rollback takes them with it.
    let mut on_problems = |scope: &ReadScope, rows: &[ProblemRow]| -> Result<()> {
        match scope {
            ReadScope::Whole(tables) => store
                .put_entity_problems(tables, rows)
                .context("record the parse's entity problems"),
            ReadScope::Partial => store
                .put_entity_problems(&[], rows)
                .context("record the parse's entity problems"),
            ReadScope::Document(markdown_uuid) => store
                .put_document_problems(markdown_uuid, rows)
                .with_context(|| format!("record problems of document {markdown_uuid}")),
        }
    };
    // The raw commit each processor rendered from, or `None` for one
    // that read no store.
    let mut consumed: Vec<Option<String>> = Vec::with_capacity(processors.len());
    let ran = (|| -> Result<()> {
        for proc in processors {
            *running_version.lock().unwrap() = proc.render_version();
            let ctx = RenderCtx::new(
                &name,
                &data_root,
                &now,
                &progress,
                raw_cursor.as_deref(),
                raw_pin.as_deref(),
                stale_render_units.as_ref(),
                &mut on_doc,
                &mut on_declare,
                &mut on_problems,
            )
            .with_page_size(page_size);
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
    // The records the download could not fetch, at the commit this run
    // rendered from, whose grid rows are looked up after the sweep.
    let fetch_entities = raw_db
        .as_deref()
        .map(|raw_db| fetch_problem_entities(raw_db, raw_commit.as_deref()))
        .transpose()
        .with_context(|| format!("read the download's problems for {name}"))?;

    // A full render in which every processor read its store walked
    // everything, so whatever it did not produce is gone. A processor
    // that read nothing (no raw store on disk, nothing committed) says
    // nothing about what should exist, and nothing is swept.
    let full_walk =
        render_everything && !consumed.is_empty() && consumed.iter().all(Option::is_some);
    let rendered_any = !emitted.is_empty();
    let mut keep = emitted;
    // The storage report's id goes in `keep`: the provider's processors
    // know nothing about it, and the sweep would otherwise drop it on
    // any run where no number moved.
    if let Some(m) = storage.as_ref() {
        keep.insert(m.doc.markdown_uuid.clone());
    }
    // Read before the seal clears any of them: what a render unit declared
    // with nothing was last built from.
    let silent: Vec<&str> = ends
        .iter()
        .filter(|(render_unit, e)| {
            e.end == End::Read { empty: true } && !emitted_under.contains(*render_unit)
        })
        .map(|(render_unit, _)| render_unit.as_str())
        .collect();
    let last_built_from = store.inputs_of(&silent)?;
    // Which of those rows a render unit new this run now reads: where they
    // went when the key a render unit is minted from changed. A render unit that
    // read them before is no evidence (a calendar series reads all of a
    // changed occurrence's rows).
    let built: BTreeSet<&str> = ends
        .iter()
        .filter(|(render_unit, e)| {
            e.end == End::Read { empty: false } && new_render_units.contains(render_unit.as_str())
        })
        .map(|(render_unit, _)| render_unit.as_str())
        .collect();
    let asked: Vec<Input> = last_built_from
        .values()
        .flatten()
        .filter(|i| i.id != WHOLE_TABLE)
        .cloned()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let moved: HashSet<Input> = store
        .readers_of(&asked)?
        .into_iter()
        .filter(|(_, render_unit)| built.contains(render_unit.as_str()))
        .map(|(input, _)| input)
        .collect();
    let removed_rows = removed_rows.unwrap_or_default();
    let endings: BTreeMap<String, Ending> = ends
        .into_iter()
        .map(|(render_unit, ended)| {
            let left = last_built_from
                .get(&render_unit)
                .is_some_and(|inputs| rows_left(inputs, &removed_rows, &moved));
            let fate = fate(&ended.end, emitted_under.contains(&render_unit), left);
            let ending = Ending {
                fate,
                inputs_unwritten: matches!(ended.end, End::Read { empty: true } | End::Excluded),
                render_version: ended.render_version,
            };
            (render_unit, ending)
        })
        .collect();

    // The sweep runs only on a run that got through every processor
    // — a render that failed partway named a fraction of what it
    // holds, and `?` above already returned.
    let sealed = seal_run(
        &store,
        RunEnd {
            source_id: &name,
            sweep: full_walk,
            keep: &keep,
            render_units: &endings,
            storage,
            // The cursor is rewritten only when it moves: its row carries
            // a per-run stamp, and rewriting it unchanged would give
            // every steady-state run a commit. A run that rendered nothing
            // over a range where only bookkeeping moved leaves it too:
            // the next diff from it reads the same, and moving it would
            // move this store's version and run everything downstream
            // for nothing.
            cursor: raw_commit
                .filter(|_| render_everything || !bookkeeping_only || rendered_any)
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
    removed += sealed.removed;
    if sealed.kept > 0 {
        tracing::warn!(
            source = %name,
            render_units = sealed.kept,
            "render units that produced no document kept the ones they had \
             (see `problems` in its indexed_markdown.doltlite_db)"
        );
    }
    // The download's problems are read in its own store; a copy an
    // older build carried in here goes. After the sweep, the grid row
    // each of them is about, where this store still holds it, so a
    // reader can link a problem the download could only name by its raw
    // record.
    store
        .replace_stage_problems(Stage::Fetch, &[])
        .with_context(|| format!("drop the download's problems from {name}'s store"))?;
    if let Some(entities) = fetch_entities {
        let items = entity_items(&store, processors, &name, &entities)
            .with_context(|| format!("find the rows {name}'s download problems are about"))?;
        store
            .replace_entity_items(&items)
            .with_context(|| format!("record the rows {name}'s download problems are about"))?;
    }
    if !endings.is_empty() {
        tracing::info!(
            source = %name,
            render_units = endings.len(),
            "render units declared with their inputs"
        );
    }

    // One commit for the whole render. Per-document commits would
    // put thousands of entries in `dolt_log` per run; committing
    // once is also what makes `dolt_diff` over this store answer
    // "what did this render change?".
    // The provider's documents, then the driver's own storage report
    // when its counts moved, so the number reads as what the provider
    // rendered.
    let mut msg = format!("render {name}: {docs} document(s)");
    if removed > 0 {
        msg.push_str(&format!(", {removed} removed upstream"));
    }
    if sealed.stored > 0 {
        msg.push_str(", storage report");
    }
    docs += sealed.stored;
    store
        .commit(&msg)
        .with_context(|| format!("commit render store for {}", name))?;
    // Read back from the store that just wrote them, before `close`
    // consumes it.
    let versions = store.render_versions()?;
    let problems = store.own_problem_counts()?;
    let holdings = store.holdings(storage_uuid.as_deref())?;
    let head = store.content_version()?;
    store.close();
    every_stored_version_must_be_declared(&name, &rendered_root, &versions, declared.as_ref())?;
    let total = render_started.elapsed();
    let written = std::time::Duration::from_micros(writing_micros.into_inner());
    tracing::info!(
        source = %name,
        docs,
        total_ms = total.as_millis() as u64,
        write_ms = written.as_millis() as u64,
        "the render took {:.1}s, {:.1}s of it writing documents to the store",
        total.as_secs_f64(),
        written.as_secs_f64(),
    );
    Ok(RenderReport {
        docs,
        removed,
        holdings,
        problems,
        head,
        // What the final commit sealed beyond the last checkpoint.
        unsealed: checkpointer.pending(),
    })
}

/// What a processor said about one render unit, as the driver records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum End {
    /// Built; `empty` when from no rows at all.
    Read {
        empty: bool,
    },
    Excluded,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ended {
    end: End,
    /// The version of the processor that said it.
    render_version: Option<u32>,
}

impl Ended {
    fn of(end: RenderUnitEnd<'_>, render_version: Option<u32>) -> Self {
        let end = match end {
            RenderUnitEnd::Read(inputs) => End::Read {
                empty: inputs.is_empty(),
            },
            RenderUnitEnd::Excluded => End::Excluded,
            RenderUnitEnd::Failed(why) => End::Failed(why.to_string()),
        };
        Ended {
            end,
            render_version,
        }
    }

    fn failed(&self) -> bool {
        matches!(self.end, End::Failed(_))
    }
}

/// What the seal does with the documents the store holds under a render unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fate {
    /// Whatever this run did not emit under it goes.
    Swept,
    /// They stay as they were, each carrying a problem saying why.
    Kept(Kept),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kept {
    /// The renderer said its build failed, and why.
    Failed(String),
    /// Declared with nothing, though no row it was built from left.
    Unexplained,
}

/// The rule the sweep rests on: a render unit is gone only when the diff says
/// its rows left. Built, excluded and failed are the renderer's word; a
/// render unit declared with no rows that emitted nothing is gone only on
/// that evidence, on a full walk as on a narrowed run.
pub(crate) fn fate(end: &End, emitted: bool, rows_left: bool) -> Fate {
    match end {
        End::Failed(why) => Fate::Kept(Kept::Failed(why.clone())),
        End::Excluded | End::Read { empty: false } => Fate::Swept,
        End::Read { empty: true } if emitted || rows_left => Fate::Swept,
        End::Read { empty: true } => Fate::Kept(Kept::Unexplained),
    }
}

/// Whether the rows a render unit was last built from left it: the diff
/// reports one of them removed, or every one of them is now read by a
/// render unit new this run (`moved`) — the render unit's key is minted from a
/// value that changed, and its rows build that render unit instead. A
/// whole-table input is never evidence: one row of a table leaving, or
/// another render unit reading it, says nothing about a render unit that read all
/// of it.
pub(crate) fn rows_left(
    last_built_from: &[Input],
    removed: &HashSet<Input>,
    moved: &HashSet<Input>,
) -> bool {
    let mut rows = last_built_from
        .iter()
        .filter(|i| i.id != WHOLE_TABLE)
        .peekable();
    if rows.peek().is_none() {
        return false;
    }
    let rows: Vec<&Input> = rows.collect();
    rows.iter().any(|i| removed.contains(*i)) || rows.iter().all(|i| moved.contains(*i))
}

/// One render unit the run looked at, as the seal acts on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ending {
    pub(crate) fate: Fate,
    /// It was declared with no inputs, which the driver has not written
    /// yet: the seal clears its old ones unless it is kept.
    pub(crate) inputs_unwritten: bool,
    /// The version of the processor that declared it, for its problem.
    pub(crate) render_version: Option<u32>,
}

/// What closes a run: the sweeps, the storage report, and the cursor to
/// record.
pub(crate) struct RunEnd<'a> {
    pub(crate) source_id: &'a str,
    /// Whether the run walked everything, so a document not in `keep`
    /// (or under a kept render unit) is one the source no longer produces.
    pub(crate) sweep: bool,
    /// Every document this run emitted or owns.
    pub(crate) keep: &'a BTreeSet<String>,
    /// Every render unit the run looked at, and what becomes of it.
    pub(crate) render_units: &'a BTreeMap<String, Ending>,
    pub(crate) storage: Option<crate::introspect::Measured>,
    pub(crate) cursor: Option<RenderCursorRow>,
}

pub(crate) struct Sealed {
    pub(crate) stored: usize,
    pub(crate) removed: usize,
    /// Render units whose documents stayed though the run produced none. A
    /// kept render unit that holds no document is not one.
    pub(crate) kept: usize,
}

/// The problem a document of a kept render unit carries. Scoped to the
/// document, so it reaches the document's banner, and it clears when
/// the render unit next renders it, or removes it.
fn kept_problem(source_id: &str, markdown_uuid: &str, ending: &Ending) -> Option<ProblemRow> {
    use datalib_schema::problems::{Outcome, Problem, Reason, Scope};
    let Fate::Kept(why) = &ending.fate else {
        return None;
    };
    Some(match why {
        Kept::Failed(why) => datalib_etl_render::processor::document_failed(
            source_id,
            markdown_uuid,
            why,
            ending.render_version,
        ),
        Kept::Unexplained => ProblemRow::new(
            source_id,
            Stage::Render,
            Scope::Markdown(markdown_uuid),
            None,
            Outcome::Ok,
            Problem::explained(
                Reason::NoDocument,
                None,
                "the last render produced no document here, though the rows this was \
                 built from are still upstream; this is what an earlier run rendered",
            )
            .severity(Severity::Warning),
            ending.render_version,
        ),
    })
}

/// The sweep, the storage report and the cursor land as one transaction,
/// so the cursor can never claim a range the store's rows do not reflect.
pub(crate) fn seal_run(store: &IndexedMarkdownStore, end: RunEnd<'_>) -> Result<Sealed> {
    store.transaction(|| {
        let mut sealed = Sealed {
            stored: 0,
            removed: 0,
            kept: 0,
        };
        let kept: Vec<&str> = end
            .render_units
            .iter()
            .filter(|(_, e)| matches!(e.fate, Fate::Kept(_)))
            .map(|(render_unit, _)| render_unit.as_str())
            .collect();
        let held_by_kept = store.documents_for_render_units(&kept)?;
        sealed.kept = held_by_kept
            .iter()
            .map(|(render_unit, _)| render_unit)
            .collect::<BTreeSet<_>>()
            .len();
        let mut problems = Vec::with_capacity(held_by_kept.len());
        for (render_unit, uuid) in &held_by_kept {
            problems.extend(kept_problem(
                end.source_id,
                uuid,
                &end.render_units[render_unit],
            ));
        }
        store
            .put_problems(&problems)
            .context("record why kept render units kept their documents")?;
        let kept_documents: BTreeSet<&str> =
            held_by_kept.iter().map(|(_, uuid)| uuid.as_str()).collect();
        if end.sweep {
            let gone: Vec<String> = store
                .all_document_uuids()?
                .into_iter()
                .filter(|uuid| !end.keep.contains(uuid) && !kept_documents.contains(uuid.as_str()))
                .collect();
            store
                .remove_documents(&gone)
                .context("remove the documents this source no longer produces")?;
            sealed.removed += gone.len();
            if !gone.is_empty() {
                tracing::info!(
                    documents = gone.len(),
                    "this source no longer produces these documents; dropped them",
                );
            }
        }
        // A swept render unit produces exactly what it emitted; a document the
        // store still holds for it is from a period that emptied or a
        // thread whose messages went. Positive evidence only: render units the
        // run never looked at are not here.
        let swept: Vec<&str> = end
            .render_units
            .iter()
            .filter(|(_, e)| e.fate == Fate::Swept)
            .map(|(render_unit, _)| render_unit.as_str())
            .collect();
        for render_unit in end
            .render_units
            .iter()
            .filter(|(_, e)| e.fate == Fate::Swept && e.inputs_unwritten)
            .map(|(render_unit, _)| render_unit)
        {
            store
                .put_inputs(render_unit, &[])
                .with_context(|| format!("clear inputs of render unit {render_unit}"))?;
        }
        let mut gone_by_render_unit: BTreeMap<String, usize> = BTreeMap::new();
        let mut gone: Vec<String> = Vec::new();
        for (render_unit, uuid) in store.documents_for_render_units(&swept)? {
            if !end.keep.contains(&uuid) {
                *gone_by_render_unit.entry(render_unit).or_default() += 1;
                gone.push(uuid);
            }
        }
        store
            .remove_documents(&gone)
            .context("remove the documents swept render units no longer produce")?;
        sealed.removed += gone.len();
        // One line for the sweep, not one per document: a source that
        // emptied drops every document it had.
        if !gone.is_empty() {
            tracing::info!(
                documents = gone.len(),
                render_units = gone_by_render_unit.len(),
                "swept render units no longer produce these documents; dropped them",
            );
        }
        for (render_unit, documents) in &gone_by_render_unit {
            tracing::debug!(
                render_unit,
                documents,
                "dropped this render unit's documents"
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
                tracing::debug!("storage unchanged since the last run");
            } else {
                store.put_document(&m.doc).context("store storage report")?;
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

/// The driver's half of the scan.
#[derive(Debug, Default)]
pub(crate) struct ReverseLookup {
    /// The raw store's HEAD, for the provider to pin.
    pub(crate) pin: Option<String>,
    /// The render units whose declared inputs changed between the cursor and
    /// the pin.
    pub(crate) stale: Option<HashSet<String>>,
    /// Declared inputs that left: every one the diff reports, and from
    /// a table emptied since the cursor, one per render unit that named its
    /// rows — enough for `rows_left`, which asks whether any did. The
    /// only evidence a render unit declared with nothing is gone.
    pub(crate) removed: Option<HashSet<Input>>,
    /// Between the cursor and the pin only bookkeeping moved: `sync_runs`,
    /// the sidecars, the other shared tables. Every download commits a
    /// `sync_runs` row, so this is a sync that fetched nothing new.
    pub(crate) bookkeeping_only: bool,
}

/// One table's changed keys between two commits: `changed_keys`, or a
/// test's wrapper around it that records which tables were diffed.
pub(crate) type DiffTable<'a> = dyn FnMut(&sqlx::SqlitePool, &str, &str, &str) -> Result<Vec<datalib_etl::doltlite_raw::ChangedKey>>
    + 'a;

/// The render units whose declared inputs changed between the cursor and the
/// raw store's HEAD, from `dolt_diff` over every table `render_inputs`
/// mentions and a reverse lookup, and which of those rows left. A table
/// emptied in that range is answered without its diff where it can be
/// (`emptied_table_evidence`). Nothing
/// to say — no cursor, no raw doltlite store, nothing declared yet, or a
/// range this store cannot resolve — leaves the provider's own scan to
/// decide alone, and no render unit gone.
fn reverse_lookup(
    store: &IndexedMarkdownStore,
    raw_db: Option<&Path>,
    raw_cursor: Option<&str>,
) -> Result<ReverseLookup> {
    reverse_lookup_with(store, raw_db, raw_cursor, &mut |pool, table, from, to| {
        blocking(datalib_etl::doltlite_raw::changed_keys(
            pool, table, from, to,
        ))
    })
}

pub(crate) fn reverse_lookup_with(
    store: &IndexedMarkdownStore,
    raw_db: Option<&Path>,
    raw_cursor: Option<&str>,
    diff_table: &mut DiffTable<'_>,
) -> Result<ReverseLookup> {
    let (Some(from), Some(raw_db)) = (raw_cursor, raw_db) else {
        return Ok(ReverseLookup::default());
    };
    if !raw_db.exists() {
        return Ok(ReverseLookup::default());
    }
    let tables = store.input_tables()?;
    if tables.is_empty() {
        return Ok(ReverseLookup::default());
    }
    let Some(reader) = blocking(datalib_etl::doltlite_raw::open_reader(raw_db, None))
        .with_context(|| format!("open {} for the reverse lookup", raw_db.display()))?
    else {
        return Ok(ReverseLookup::default());
    };
    let pool = reader.pool().clone();
    let result = (|| -> Result<_> {
        let to = reader.pin().commit().to_string();
        let mut changed: Vec<Input> = Vec::new();
        let mut removed: HashSet<Input> = HashSet::new();
        let mut stale: HashSet<String> = HashSet::new();
        let mut emptied: Vec<&str> = Vec::new();
        for table in &tables {
            if let Some(evidence) = emptied_table_evidence(store, &pool, table, from, reader.pin())?
            {
                stale.extend(evidence.stale);
                removed.extend(evidence.left);
                emptied.push(table);
                continue;
            }
            match diff_table(&pool, table, from, &to) {
                Ok(keys) => {
                    for k in keys {
                        let input = Input::new(table.clone(), k.key);
                        if k.change == datalib_etl::doltlite_raw::RowChange::Removed {
                            removed.insert(input.clone());
                        }
                        changed.push(input);
                    }
                }
                // A cursor this store cannot resolve — the file was
                // replaced by hand — or a table a schema change dropped.
                // Either way the range is gone; the provider cold-starts.
                Err(e) => {
                    tracing::warn!(
                        table,
                        from,
                        error = %format!("{e:#}"),
                        "reverse lookup could not diff this table; leaving the scan to the provider"
                    );
                    return Ok(ReverseLookup {
                        pin: Some(to),
                        ..ReverseLookup::default()
                    });
                }
            }
        }
        stale.extend(store.render_units_reading(&changed)?);
        let bookkeeping_only = blocking(datalib_etl::doltlite_raw::content_tables_changed(
            &pool, from, &to,
        ))?
        .is_empty();
        tracing::info!(
            changed_rows = changed.len(),
            removed_rows = removed.len(),
            emptied_tables = ?emptied,
            stale_render_units = stale.len(),
            "reverse lookup from the changed rows to the render units"
        );
        Ok(ReverseLookup {
            pin: Some(to),
            stale: Some(stale),
            removed: Some(removed),
            bookkeeping_only,
        })
    })();
    blocking(pool.close());
    result
}

/// What a table emptied since the cursor says without its diff.
struct EmptiedTable {
    /// Every render unit that declared an input from it.
    stale: HashSet<String>,
    /// For each render unit that named rows of it, one that held at the cursor.
    left: Vec<Input>,
}

/// The answer the diff would give for a table that held rows at the
/// cursor and holds none at the pin, as after a reset, at the cost of a
/// few key lookups rather than a read of every row that left. Each
/// render unit that named rows of the table needs one that held at the
/// cursor: that row left, so the render unit is stale and has lost a row. A
/// render unit that only ever looked up keys the table did not have read
/// nothing that changed, and a key that cannot be sought proves nothing,
/// so either sends the table to the diff (`None`), as does a table that
/// cannot be read at both commits.
fn emptied_table_evidence(
    store: &IndexedMarkdownStore,
    pool: &sqlx::SqlitePool,
    table: &str,
    from: &str,
    to: &datalib_etl::pin::Pin,
) -> Result<Option<EmptiedTable>> {
    use datalib_etl::doltlite_raw::{emptied_between, KeyLookup};
    let Ok(from) = datalib_etl::pin::Pin::at(from) else {
        return Ok(None);
    };
    let ready = blocking(async {
        if !emptied_between(pool, table, &from, to).await? {
            return Ok(None);
        }
        KeyLookup::new(pool, table, &from).await.map(Some)
    });
    let lookup = match ready {
        Ok(Some(lookup)) => lookup,
        Ok(None) => return Ok(None),
        Err(e) => {
            tracing::debug!(table, error = %format!("{e:#}"), "leaving this table to the diff");
            return Ok(None);
        }
    };
    let stale = store.render_units_reading_table(table)?;
    let mut left = Vec::new();
    for render_unit in &stale {
        let mut named_any = false;
        let mut witness = None;
        let mut after: Option<String> = None;
        while witness.is_none() {
            let ids = store.ids_read_from(render_unit, table, after.as_deref(), KEYS_PER_PAGE)?;
            let Some(last) = ids.last() else {
                break;
            };
            named_any = true;
            after = Some(last.clone());
            witness = match blocking(lookup.first_held(&ids)) {
                Ok(found) => found,
                Err(e) => {
                    tracing::debug!(table, error = %format!("{e:#}"), "leaving this table to the diff");
                    return Ok(None);
                }
            };
        }
        match witness {
            Some(id) => left.push(Input::new(table, id)),
            None if named_any => {
                tracing::debug!(
                    table,
                    render_unit,
                    "a render unit named no row this table held; leaving it to the diff"
                );
                return Ok(None);
            }
            None => {}
        }
    }
    Ok(Some(EmptiedTable { stale, left }))
}

/// `(entity_key, item_uuid)` for each of `entities` (a download's
/// problem scopes, `<table>:<id>`) whose grid row this store holds: the
/// uuid the first processor that knows the raw table mints for it
/// (`item_of_entity`), else the row whose upstream key the processor
/// names (`upstream_of_entity`).
fn entity_items(
    store: &IndexedMarkdownStore,
    processors: &[Box<dyn RenderProcessor>],
    source_id: &str,
    entities: &[String],
) -> Result<Vec<(String, String)>> {
    let raw: Vec<Option<(&str, &str)>> = entities.iter().map(|e| raw_entity(e)).collect();
    let mut items: Vec<Option<String>> = raw
        .iter()
        .map(|r| {
            let (table, id) = (*r)?;
            processors
                .iter()
                .find_map(|p| p.item_of_entity(source_id, table, id))
        })
        .collect();
    let upstream: Vec<Option<(&'static str, String)>> = raw
        .iter()
        .zip(&items)
        .map(|(r, item)| {
            let (table, id) = (*r)?;
            if item.is_some() {
                return None;
            }
            processors
                .iter()
                .find_map(|p| p.upstream_of_entity(table, id))
        })
        .collect();
    let keys: Vec<(&str, String)> = upstream.iter().flatten().cloned().collect();
    let found = store.grid_rows_by_upstream(&keys)?;
    for (item, key) in items.iter_mut().zip(upstream) {
        if let Some((kind, id)) = key {
            *item = found.get(&(kind.to_string(), id)).cloned();
        }
    }
    let wanted: Vec<String> = items.iter().flatten().cloned().collect();
    let held = store.grid_rows_among(&wanted)?;
    Ok(entities
        .iter()
        .zip(items)
        .filter_map(|(entity, item)| {
            item.filter(|uuid| held.contains(uuid))
                .map(|uuid| (entity.clone(), uuid))
        })
        .collect())
}

/// The raw `(table, id)` a download's problem names: `table:id` from a
/// fetch attempt, `record:table:id` from a record it could not reach.
fn raw_entity(scope_key: &str) -> Option<(&str, &str)> {
    scope_key
        .strip_prefix(datalib_etl::download_problems::RECORD_PREFIX)
        .unwrap_or(scope_key)
        .split_once(':')
}

/// The raw store a render reads is in this build's shape, or the render
/// is refused before any provider reads it, naming the download step that
/// brings it there: the launch's prepare pass migrates every raw store, so
/// a refusal here is one that pass could not migrate. A store this very
/// build opened is in its shape by construction; any other is compared
/// with a new store made in scratch. No store yet is nothing to read.
pub(crate) async fn require_raw_shape(raw_db: &Path, source_type: &str) -> Result<()> {
    if !raw_db.exists() {
        return Ok(());
    }
    let meta = datalib_store_meta::guard::read_at(raw_db)
        .await
        .with_context(|| format!("read {}'s _datalib_meta", raw_db.display()))?;
    if meta
        .as_ref()
        .is_some_and(crate::prepare::opened_by_this_build)
    {
        return Ok(());
    }
    let want = crate::prepare::raw_shape(source_type).await?;
    let step = datalib_store_meta::step_of(raw_db);
    datalib_store_meta::require(raw_db, step.as_deref(), meta.as_ref(), &want)
}

/// The entity scopes of the raw store's fetch problems at `commit` (HEAD
/// when the run consumed none). None when there is no store or nothing
/// committed.
fn fetch_problem_entities(raw_db: &Path, commit: Option<&str>) -> Result<Vec<String>> {
    if !raw_db.exists() {
        return Ok(Vec::new());
    }
    let Some(reader) = blocking(datalib_etl::doltlite_raw::open_reader(raw_db, commit))
        .with_context(|| format!("open {} for its problems", raw_db.display()))?
    else {
        return Ok(Vec::new());
    };
    let pool = reader.pool().clone();
    let result = blocking(async {
        sql!(
            "datalib_step.render.list_fetch_problem_entities",
            "SELECT DISTINCT scope_key FROM problems WHERE stage = ? AND scope_kind = ? \
             ORDER BY scope_key"
        )
        .query_scalar::<String>()
        .bind(Stage::Fetch.as_str())
        .bind(datalib_schema::problems::ScopeKind::Entity.as_str())
        .fetch_all(&pool)
        .await
        .context("read the raw store's problems")
    });
    blocking(pool.close());
    result
}

/// How many of a render unit's keys one lookup at the cursor tries: the first
/// usually held, and a page bounds what a render unit of a million rows loads.
const KEYS_PER_PAGE: usize = 64;

/// Whether this run diffs from the stored cursor or renders every render unit.
#[derive(Debug, PartialEq, Eq)]
enum RenderPlan {
    /// Diff from this raw-store commit — or from nothing, when there is
    /// no cursor yet: the provider then reads its whole store, which is
    /// also the steady state of a renderer that never records a cursor.
    FromCursor(Option<String>),
    /// Render everything, for the reason given. The stored cursor is
    /// kept: the sweep at the end of the run is what removes documents
    /// the new version or params no longer produce, and the range is
    /// still the one `unified_grid_and_search_index` diffs the store over.
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

/// The top-level params whose value differs between the stored cursor
/// and this build: a processor's id, or one of the underscored keys.
fn changed_params(stored: &str, declared: &serde_json::Value) -> Vec<String> {
    let stored: serde_json::Value = serde_json::from_str(stored).unwrap_or_default();
    let empty = serde_json::Map::new();
    let (old, new) = (
        stored.as_object().unwrap_or(&empty),
        declared.as_object().unwrap_or(&empty),
    );
    let mut keys: Vec<String> = old
        .keys()
        .chain(new.keys())
        .filter(|k| old.get(*k) != new.get(*k))
        .cloned()
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

/// The key the render store's own DDL hash sits under, beside the
/// processors' params. Underscored so it cannot collide with a
/// processor id, which is a group's function name.
const STORE_SCHEMA_PARAM: &str = "_store_schema";

/// The key `datalib_handle::RULES_VERSION` sits under. Every source
/// carries it, not only the ones that mint handles today: a provider
/// that starts writing them cannot forget to declare it, and a rules
/// change is rare enough that re-rendering the rest costs little.
const HANDLE_RULES_PARAM: &str = "_handle_rules";

/// The key `datalib_etl_render::SHARED_RULES_VERSION` sits under: what
/// the shared writers put on every source's pages changed.
const SHARED_RULES_PARAM: &str = "_shared_render_rules";

/// Every processor's params under its id, plus the render store's DDL
/// hash, the handle rules and the shared render rules, so one source's
/// cursor carries all of them and a change to any one — a processor's
/// knob, the shape of the store, what a handle normalizes to, what every
/// page's shared parts say — re-renders the source.
pub(crate) fn declared_render_params(processors: &[Box<dyn RenderProcessor>]) -> serde_json::Value {
    processors
        .iter()
        .map(|p| (p.id().to_string(), p.render_params()))
        .chain([
            (
                STORE_SCHEMA_PARAM.to_string(),
                serde_json::Value::String(datalib_etl_render::indexed_markdown::schema_hash()),
            ),
            (
                HANDLE_RULES_PARAM.to_string(),
                serde_json::Value::from(datalib_handle::RULES_VERSION),
            ),
            (
                SHARED_RULES_PARAM.to_string(),
                serde_json::Value::from(datalib_etl_render::SHARED_RULES_VERSION),
            ),
        ])
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
                "processors pinned different raw commits; the cursor takes the first"
            );
        }
    }
    Some(first)
}

pub(crate) fn tree_is_from_an_older_renderer(
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
        "rendered tree came from a different renderer version; \
         rendering every document again and sweeping what the walk does not produce"
    );
    true
}

pub(crate) fn every_stored_version_must_be_declared(
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

pub(crate) fn declared_render_versions(
    processors: &[Box<dyn RenderProcessor>],
) -> Option<BTreeSet<u32>> {
    let versions: BTreeSet<u32> = processors
        .iter()
        .map(|p| p.render_version())
        .collect::<Option<_>>()?;
    (!versions.is_empty()).then_some(versions)
}

#[cfg(test)]
mod render_pages_tests {
    use super::decode_render_pages;

    #[test]
    fn render_pages_read_either_side_or_neither() {
        assert_eq!(decode_render_pages(","), Some((None, None)));
        assert_eq!(decode_render_pages("200,"), Some((Some(200), None)));
        assert_eq!(decode_render_pages(",16"), Some((None, Some(16))));
        assert_eq!(decode_render_pages("1,1"), Some((Some(1), Some(1))));
    }

    /// A zero or a word is `None`, never a guess: the render falls back
    /// to its own page size and logs.
    #[test]
    fn unreadable_render_pages_are_none() {
        for bad in ["", "500", "0,64", "500,0", "-1,64", "five,64", "500,64,1"] {
            assert_eq!(decode_render_pages(bad), None, "{bad:?} must not parse");
        }
    }
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

    /// "render params changed" said nothing of which: a live root
    /// re-rendered every source and its log could not say why.
    #[test]
    fn the_changed_params_are_named() {
        let stored = json!({"render": {"a": 1}, "_doc_layout": 1, "_gone": true}).to_string();
        let declared = json!({"render": {"a": 1}, "_doc_layout": 2, "_new": 0});
        assert_eq!(
            changed_params(&stored, &declared),
            ["_doc_layout", "_gone", "_new"]
        );
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

    /// A param change re-renders everything and keeps the stored cursor —
    /// "render every render unit" and "drop the cursor" are different
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
    /// records one — perseus, reading files — sweeps through the render unit
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
        write_doc_in(root, uuid, None);
    }

    fn write_doc_in(root: &Path, uuid: &str, render_unit_key: Option<&str>) {
        let store = IndexedMarkdownStore::open(root).unwrap();
        let row = datalib_schema::grid_rows::GridRow::builder()
            .uuid(uuid)
            .provider(datalib_schema::providers::Provider::Test)
            .kind("Test")
            .source_label("Test")
            .conversation_uuid(uuid)
            .entire_chat(format!("/chat/{uuid}"))
            .body("body")
            .markdown_uuid(Some(uuid.to_string()))
            .is_document(true)
            .item_count(Some(1))
            .build()
            .unwrap();
        store
            .put_document(&RenderedMarkdown {
                searchable: true,
                content_hash: None,
                markdown_uuid: uuid.to_string(),
                source_id: "src".into(),
                upstream_cursor: None,
                render_unit_key: render_unit_key.map(String::from),
                render_version: 5,
                rows: vec![row],
                sections: Vec::new(),
                front: Default::default(),
                edges: Vec::new(),
                contacts: Vec::new(),
                data_nodes: Vec::new(),
                filings: Vec::new(),
                photos: Vec::new(),
                problems: Vec::new(),
            })
            .unwrap();
        store.commit("fixture").unwrap();
        store.close();
    }

    /// Knows the rows of one raw table, each keyed by its raw id.
    struct Resolves(&'static str);

    #[async_trait::async_trait]
    impl RenderProcessor for Resolves {
        fn id(&self) -> &str {
            "resolves"
        }
        async fn run(&self, _ctx: &RenderCtx<'_>) -> Result<String> {
            Ok(String::new())
        }
        fn item_of_entity(&self, _source_id: &str, table: &str, id: &str) -> Option<String> {
            (table == self.0).then(|| id.to_string())
        }
    }

    /// A download's problem is linked to the grid row its raw record
    /// belongs to only when the store holds that row after the sweep, so
    /// a link always opens something; a record no processor knows, a
    /// listing, and a row swept away are left unlinked.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_download_problem_is_linked_only_to_a_row_the_store_holds() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("src/render_markdown");
        write_doc(&root, "kept");
        write_doc(&root, "swept");

        let store = IndexedMarkdownStore::open(&root).unwrap();
        let keep: BTreeSet<String> = ["kept".to_string()].into_iter().collect();
        seal_run(
            &store,
            RunEnd {
                source_id: "src",
                sweep: true,
                keep: &keep,
                render_units: &BTreeMap::new(),
                storage: None,
                cursor: None,
            },
        )
        .unwrap();
        let entities: Vec<String> = [
            "things:kept",
            "record:things:kept",
            "things:swept",
            "things:never-rendered",
            "others:kept",
            "listing:things",
        ]
        .map(String::from)
        .to_vec();
        let processors: Vec<Box<dyn RenderProcessor>> =
            vec![Box::new(Resolves("users")), Box::new(Resolves("things"))];
        let items = entity_items(&store, &processors, "src", &entities).unwrap();
        store.close();
        assert_eq!(
            items,
            [
                ("things:kept".to_string(), "kept".to_string()),
                ("record:things:kept".to_string(), "kept".to_string()),
            ]
        );
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
            RunEnd {
                source_id: "src",
                sweep: true,
                keep: &keep,
                render_units: &BTreeMap::new(),
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

    /// Only a kept render unit that holds a document is counted as keeping
    /// one. Claude declares each id under both its conversation and its
    /// project uuid, and the shape that never had a page used to make the
    /// warning name every rendered render unit.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_kept_render_unit_with_no_document_is_not_counted_as_keeping_one() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("src/render_markdown");
        write_doc_in(&root, "doc", Some("held"));

        let store = IndexedMarkdownStore::open(&root).unwrap();
        let unexplained = || Ending {
            fate: Fate::Kept(Kept::Unexplained),
            inputs_unwritten: true,
            render_version: None,
        };
        let render_units: BTreeMap<String, Ending> = [
            ("held".to_string(), unexplained()),
            ("never-had-a-page".to_string(), unexplained()),
        ]
        .into_iter()
        .collect();
        let sealed = seal_run(
            &store,
            RunEnd {
                source_id: "src",
                sweep: false,
                keep: &BTreeSet::new(),
                render_units: &render_units,
                storage: None,
                cursor: None,
            },
        )
        .unwrap();
        assert_eq!(sealed.kept, 1);
        assert_eq!(store.all_document_uuids().unwrap(), vec!["doc".to_string()]);
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
            RunEnd {
                source_id: "src",
                sweep: false,
                keep: &BTreeSet::new(),
                render_units: &BTreeMap::new(),
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

    /// The rule, case by case: only the renderer's word or a removed row
    /// sweeps a render unit that produced nothing.
    #[test]
    fn a_render_unit_that_produced_nothing_is_gone_only_on_evidence() {
        let silent = End::Read { empty: true };
        let built = End::Read { empty: false };
        let failed = End::Failed("bad payload".into());
        let kept_failed = Fate::Kept(Kept::Failed("bad payload".into()));
        let unexplained = Fate::Kept(Kept::Unexplained);
        // (end, emitted, rows left) → fate
        for (end, emitted, left, want) in [
            (&silent, false, false, &unexplained),
            (&silent, false, true, &Fate::Swept),
            (&silent, true, false, &Fate::Swept),
            (&built, false, false, &Fate::Swept),
            (&End::Excluded, false, false, &Fate::Swept),
            (&failed, false, true, &kept_failed),
            (&failed, true, false, &kept_failed),
        ] {
            assert_eq!(
                &fate(end, emitted, left),
                want,
                "{end:?} emitted={emitted} rows_left={left}"
            );
        }
    }

    /// Evidence is a removed row the render unit named, or every row it named
    /// now read by a render unit the run built; a table it read whole says
    /// nothing either way.
    #[test]
    fn rows_left_only_when_a_named_row_was_removed_or_all_moved() {
        let thread = || Input::new("threads", "t1");
        let message = || Input::new("messages", "m2");
        let users = || Input::whole_table("users");
        let set = |inputs: Vec<Input>| inputs.into_iter().collect::<HashSet<Input>>();
        let none = HashSet::new();
        let built_from = [thread(), message(), users()];

        assert!(rows_left(&built_from, &set(vec![message()]), &none));
        assert!(!rows_left(&[thread()], &set(vec![message()]), &none));
        assert!(!rows_left(&[users()], &set(vec![users()]), &none));
        assert!(!rows_left(&[], &none, &none));

        assert!(rows_left(
            &built_from,
            &none,
            &set(vec![thread(), message()])
        ));
        assert!(
            !rows_left(&built_from, &none, &set(vec![message()])),
            "its thread row still builds nothing else: not moved"
        );
        assert!(!rows_left(&[users()], &none, &set(vec![users()])));
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
        declared_render_params, declared_render_versions, every_stored_version_must_be_declared,
        tree_is_from_an_older_renderer, RenderCursorRow, RenderPlan, HANDLE_RULES_PARAM,
        SHARED_RULES_PARAM, STORE_SCHEMA_PARAM,
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
            .body("body")
            .markdown_uuid(Some(chat_uuid.to_string()))
            .is_document(true)
            .item_count(Some(1))
            .build()
            .unwrap();
        store
            .put_document(&RenderedMarkdown {
                searchable: true,
                content_hash: None,
                markdown_uuid: chat_uuid.to_string(),
                source_id: "claude_web".into(),
                upstream_cursor: None,
                render_unit_key: None,
                render_version: version,
                rows: vec![row],
                sections: Vec::new(),
                front: Default::default(),
                edges: Vec::new(),
                contacts: Vec::new(),
                data_nodes: Vec::new(),
                filings: Vec::new(),
                photos: Vec::new(),
                problems: Vec::new(),
            })
            .unwrap();
        store.commit("fixture").unwrap();
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
    /// itself is kept — its history is what `unified_grid_and_search_index` diffs over, so
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

    /// The render store's own DDL hash rides in the params under a key
    /// no processor can claim, so a column added to `grid_rows` is a
    /// param change — every source re-renders, and nobody has to bump
    /// anything.
    #[test]
    fn the_store_schema_is_a_render_param() {
        let procs: Vec<Box<dyn RenderProcessor>> = vec![Box::new(Stub(Some(1)))];
        let params = declared_render_params(&procs);
        assert_eq!(
            params[STORE_SCHEMA_PARAM],
            serde_json::Value::String(datalib_etl_render::indexed_markdown::schema_hash())
        );
        assert!(params["stub"].is_object() || params["stub"].is_null());
        assert_eq!(params.as_object().unwrap().len(), 4);
    }

    /// A stored handle is only as current as the rules that minted it
    /// (#980 had to bump six renderers by hand). The rules version rides
    /// in every source's params, so moving it renders every source again.
    #[test]
    fn a_handle_rules_change_renders_everything() {
        let procs: Vec<Box<dyn RenderProcessor>> = vec![Box::new(Stub(Some(1)))];
        let params = declared_render_params(&procs);
        assert_eq!(
            params[HANDLE_RULES_PARAM],
            serde_json::Value::from(datalib_handle::RULES_VERSION)
        );
        let mut older = params.clone();
        older[HANDLE_RULES_PARAM] = serde_json::Value::from(datalib_handle::RULES_VERSION - 1);
        let stored = RenderCursorRow {
            source_id: "src".into(),
            raw_commit: "commit-a".into(),
            params: older.to_string(),
            rendered_at_utc: "2026-01-01T00:00:00.000000Z".into(),
            tz_offset: Some("+00:00".into()),
        };
        assert_eq!(
            RenderPlan::decide(Some(&stored), &params, false),
            RenderPlan::Everything("render params changed")
        );
    }

    /// What the shared writers put on every page — a title, a chip, a
    /// name without its bidi controls — changed for every source, and
    /// only the chat sources' layout version moved, so the rest kept
    /// their old pages. The shared rules ride in every source's params.
    #[test]
    fn a_shared_render_rules_change_renders_everything() {
        let procs: Vec<Box<dyn RenderProcessor>> = vec![Box::new(Stub(Some(1)))];
        let params = declared_render_params(&procs);
        assert_eq!(
            params[SHARED_RULES_PARAM],
            serde_json::Value::from(datalib_etl_render::SHARED_RULES_VERSION)
        );
        let mut older = params.clone();
        older.as_object_mut().unwrap().remove(SHARED_RULES_PARAM);
        let stored = RenderCursorRow {
            source_id: "src".into(),
            raw_commit: "commit-a".into(),
            params: older.to_string(),
            rendered_at_utc: "2026-01-01T00:00:00.000000Z".into(),
            tz_offset: Some("+00:00".into()),
        };
        assert_eq!(
            RenderPlan::decide(Some(&stored), &params, false),
            RenderPlan::Everything("render params changed")
        );
    }
}
