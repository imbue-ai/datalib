//! The diff group's render: the source's own render processors run
//! twice — the buckets that moved between two raw commits, at the later
//! one and then at the earlier — into a collecting sink, the two sides
//! subtracted (`datalib_etl_render::diff`), and the result written as
//! one ordinary render tree. `docs/dev/plans/diff_renderer.md`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_dag::events::Event;
use datalib_etl::progress::Progress;
use datalib_etl_render::diff::{diff_document, Counts};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;
use datalib_etl_render::processor::{Input, RenderCtx, RenderProcessor};
use datalib_etl_render::section::{join, Section};
use datalib_id::{entity_id_str, IdNamespace, Scope};
use datalib_schema::edges::EdgeRow;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::render_cursor::RenderCursorRow;

use crate::dispatch::{PlannedSource, Wave};
use crate::events::{Emitter, OutputClaim};
use crate::render::{
    declared_render_params, declared_render_versions, every_stored_version_must_be_declared,
    seal_run, RenderReport, RenderSource, RunEnd,
};
use crate::source::StepEnv;

/// The two raw commits a diff group compares, from the step's
/// `params.diff`. Both required: a diff is asked for, never standing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffPair {
    pub from: String,
    pub to: String,
    /// The most documents either side may render before the step
    /// fails instead — for a pair a person expected to be small and is
    /// not. `params.diff.max_documents`, else [`DEFAULT_MAX_DOCUMENTS`].
    pub max_documents: usize,
}

/// What a diff renders at most, per side, when the config does not say:
/// enough for a week of a busy source, and far short of "everything".
pub const DEFAULT_MAX_DOCUMENTS: usize = 1000;

/// A side rendered past `max_documents`. Its own type so the step can
/// tell this failure from any other and say what to do about it as a
/// hint, the way an auth failure names its fix.
#[derive(Debug)]
pub struct DiffTooLarge {
    pub pin: String,
    pub max_documents: usize,
}

impl std::fmt::Display for DiffTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the diff at {} touches more than {} document(s). A diff this large was \
             probably not the one meant: choose two closer commits, or raise \
             `params.diff.max_documents` to say it was",
            self.pin, self.max_documents
        )
    }
}

impl std::error::Error for DiffTooLarge {}

fn too_large(e: &anyhow::Error) -> Option<&DiffTooLarge> {
    e.chain().find_map(|c| c.downcast_ref::<DiffTooLarge>())
}

/// Take `diff` out of the step's params; what is left is the source
/// type's own render config, parsed as strictly as on the source's step.
pub fn split_params(mut params: serde_json::Value) -> Result<(DiffPair, serde_json::Value)> {
    let diff = params
        .as_object_mut()
        .and_then(|o| o.remove("diff"))
        .context(
            "a diff group's render step needs `params.diff = { from = <raw commit>, to = \
             <raw commit> }`: the two commits of the source's raw store to compare",
        )?;
    let table = diff
        .as_object()
        .context("parse params.diff: expected a table with `from` and `to`")?;
    if let Some(stray) = table
        .keys()
        .find(|k| !matches!(k.as_str(), "from" | "to" | "max_documents"))
    {
        anyhow::bail!(
            "parse params.diff: unknown field {stray:?}; only `from`, `to` and \
             `max_documents` are read"
        );
    }
    let max_documents = match table.get("max_documents") {
        None => DEFAULT_MAX_DOCUMENTS,
        Some(v) => v
            .as_u64()
            .filter(|n| *n > 0)
            .and_then(|n| usize::try_from(n).ok())
            .context("parse params.diff: `max_documents` is a positive whole number")?,
    };
    let commit = |key: &str| -> Result<String> {
        let value = table
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .with_context(|| {
                format!("parse params.diff: `{key}` is a raw commit and is required")
            })?;
        Ok(value.to_string())
    };
    let pair = DiffPair {
        from: commit("from")?,
        to: commit("to")?,
        max_documents,
    };
    anyhow::ensure!(
        pair.from != pair.to,
        "params.diff: `from` and `to` are the same commit {:?}; nothing changed between a \
         commit and itself",
        pair.from
    );
    Ok((pair, params))
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    planned: PlannedSource,
    env: &StepEnv,
    data_root: &Path,
    now: &str,
    emitter: &Emitter,
    control: &datalib_etl::control::DownloadControl,
    pair: &DiffPair,
) -> Result<Vec<OutputClaim>> {
    emitter.declare_streams_output(true);
    let PlannedSource {
        name,
        processors,
        raw_path,
        source_type,
        ..
    } = planned;
    let Wave::Render(processors) = processors else {
        anyhow::bail!("the diff driver was handed source {name:?}'s ingest wave");
    };
    anyhow::ensure!(
        !processors.is_empty(),
        "diff group {name:?}: its source's type `{source_type}` renders nothing, so there \
         is nothing to compare"
    );
    let progress = emitter.progress();
    let rendered_root = data_root.join(&env.step);
    let source = RenderSource {
        name: name.clone(),
        data_root: data_root.to_path_buf(),
        rendered_root: rendered_root.clone(),
        now: now.to_string(),
        cadence: control.checkpoint_cadence.unwrap_or_default(),
        // A diff group has no store of its own to measure.
        storage: None,
        raw_db: Some(datalib_etl::doltlite_raw::db_path_for(&raw_path)),
        progress: progress.clone(),
    };
    let pair = pair.clone();
    let report =
        tokio::task::spawn_blocking(move || render_diff_source(&processors, source, &pair))
            .await
            .context("diff render task panicked")?;
    let report = match report {
        Ok(r) => r,
        Err(e) => {
            // The cap is advice as much as an error: say what to do
            // where a person reads the run, like an auth failure does.
            if let Some(cap) = too_large(&e) {
                emitter.event(&Event::Hint {
                    step: String::new(), // re-tagged by the runner
                    msg: cap.to_string(),
                });
            }
            return Err(e);
        }
    };
    tracing::info!(
        docs = report.docs,
        removed = report.removed,
        "diff: documents written"
    );
    progress.metric("documents_removed", &[], report.removed as i64);
    datalib_core::layout::mark_derived_cache(&rendered_root);
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

/// One side of the comparison: what the processors emitted at one pin.
#[derive(Default)]
struct Side {
    docs: BTreeMap<String, Collected>,
    buckets: BTreeMap<String, Vec<Input>>,
    /// Set once the sink refused a document past the cap. The refusal
    /// is an error the processor sees too, but a processor is entitled
    /// to log a document's failure and carry on — so the side fails on
    /// this after the processors return, whatever they did with it.
    capped: bool,
}

/// One emitted document with its markdown as sections, read off the
/// file the renderer just wrote when it declared none.
struct Collected {
    md: RenderedMarkdown,
    sections: Vec<Section>,
}

pub fn render_diff_source(
    processors: &[Box<dyn RenderProcessor>],
    source: RenderSource,
    pair: &DiffPair,
) -> Result<RenderReport> {
    let RenderSource {
        name,
        data_root,
        rendered_root,
        now,
        progress,
        ..
    } = source;
    let declared = declared_render_versions(processors);
    let declared_params = declared_render_params(processors);
    let store = IndexedMarkdownStore::open(&rendered_root)
        .map(|s| s.with_now(&now))
        .with_context(|| format!("open diff render store for {}", name))?;

    tracing::info!(source = %name, from = %pair.from, to = %pair.to, "diff: starting");
    // One pass per side, each scanning from the other: the raw diff
    // names the same rows either way, and a side renders the buckets of
    // the rows that exist at its own pin. A row deleted between the two
    // has no bucket at `to` — nothing there to load it into — and is
    // named only by the pass at `from`; an added one only by the pass at
    // `to`. The stale set is empty rather than absent so the scan
    // decides alone: `None` would mean "render everything".
    let none_stale: HashSet<String> = HashSet::new();
    let to_side = collect(
        processors,
        &name,
        &data_root,
        &now,
        &progress,
        Some(&pair.from),
        &pair.to,
        &none_stale,
        pair.max_documents,
    )
    .context("render the `to` side")?;
    let from_side = collect(
        processors,
        &name,
        &data_root,
        &now,
        &progress,
        Some(&pair.to),
        &pair.from,
        &none_stale,
        pair.max_documents,
    )
    .context("render the `from` side")?;
    tracing::info!(
        source = %name,
        buckets = to_side.buckets.len(),
        to_docs = to_side.docs.len(),
        from_docs = from_side.docs.len(),
        "diff: both sides rendered"
    );

    let mut docs = 0usize;
    let mut unchanged_docs = 0usize;
    let mut totals = Counts::default();
    let mut emitted: BTreeSet<String> = BTreeSet::new();
    let uuids: BTreeSet<&String> = to_side.docs.keys().chain(from_side.docs.keys()).collect();
    store.begin_batch()?;
    let written = (|| -> Result<()> {
        for uuid in uuids {
            let to = to_side.docs.get(uuid);
            let from = from_side.docs.get(uuid);
            let diff = diff_document(
                from.map(|c| (c.md.rows.as_slice(), c.sections.as_slice())),
                to.map(|c| (c.md.rows.as_slice(), c.sections.as_slice())),
            );
            let base = to.or(from).expect("a uuid comes from one side");
            let sections_same = to.is_some_and(|t| from.is_some_and(|f| t.sections == f.sections));
            if diff.counts.changed() == 0 && sections_same {
                // Named by the scan, rendered the same: a raw change the
                // renderer does not show. Not a document of this diff.
                unchanged_docs += 1;
                let _ = fs::remove_file(&base.md.md_path);
                continue;
            }
            totals.added += diff.counts.added;
            totals.removed += diff.counts.removed;
            totals.modified += diff.counts.modified;
            totals.unchanged += diff.counts.unchanged;
            let doc = rekeyed(&name, uuid, &base.md, diff.rows, diff.sections);
            fs::write(&doc.md_path, join(&doc.sections))
                .with_context(|| format!("write {}", doc.md_path.display()))?;
            store
                .put_document(&data_root, &doc)
                .with_context(|| format!("store diff document {}", doc.markdown_uuid))?;
            emitted.insert(doc.markdown_uuid.clone());
            docs += 1;
            progress.metric("documents_rendered", &[], docs as i64);
        }
        // Every bucket either side rendered, with what the `to` side
        // read — the declaration a later run's reverse lookup would use,
        // and what the sweep below keys on.
        for (bucket, inputs) in to_side.buckets.iter().chain(
            from_side
                .buckets
                .iter()
                .filter(|(b, _)| !to_side.buckets.contains_key(*b)),
        ) {
            store
                .put_inputs(bucket, inputs)
                .with_context(|| format!("record inputs of bucket {bucket}"))?;
        }
        Ok(())
    })();
    if let Err(e) = written {
        let _ = store.rollback_batch();
        return Err(e);
    }
    store.commit_batch()?;

    let buckets: BTreeSet<String> = to_side
        .buckets
        .keys()
        .chain(from_side.buckets.keys())
        .cloned()
        .collect();
    let stored_cursor = store.cursor()?;
    let declared_params_text = declared_params.to_string();
    // Every run is a full walk of a fixed pair, so whatever an earlier
    // pair produced and this one did not is swept.
    let sealed = seal_run(
        &store,
        &data_root,
        RunEnd {
            sweep: true,
            keep: &emitted,
            declared: &buckets,
            storage: None,
            cursor: Some(&pair.to)
                .filter(|to| {
                    stored_cursor
                        .as_ref()
                        .is_none_or(|c| c.raw_commit != **to || c.params != declared_params_text)
                })
                .map(|to| {
                    let stamp = datalib_time::split_stamp(&now);
                    RenderCursorRow {
                        source_id: name.clone(),
                        raw_commit: to.clone(),
                        params: declared_params_text.clone(),
                        rendered_at_utc: stamp.utc,
                        tz_offset: stamp.tz_offset,
                    }
                }),
        },
    )?;
    tracing::info!(
        source = %name,
        docs,
        unchanged_docs,
        added = totals.added,
        removed = totals.removed,
        modified = totals.modified,
        unchanged = totals.unchanged,
        swept = sealed.removed,
        "diff: rows by fate"
    );
    let msg = format!(
        "diff {name}: {docs} document(s) between {} and {} ({} added, {} removed, {} modified)",
        pair.from, pair.to, totals.added, totals.removed, totals.modified
    );
    store
        .commit(&msg)
        .with_context(|| format!("commit diff render store for {}", name))?;
    let versions = store.render_versions()?;
    let problems = store.problem_counts()?;
    let head = store.head()?;
    store.close();
    every_stored_version_must_be_declared(&name, &rendered_root, &versions, declared.as_ref())?;
    Ok(RenderReport {
        docs,
        removed: sealed.removed,
        problems,
        head,
        // No checkpoints: the one commit seals every document.
        unsealed: docs as u64,
    })
}

/// The diff's own ids. A diff row is about the source's entity but is
/// not it — the source's own row keeps that uuid, and the unified index
/// refuses two sources claiming one id — so every uuid the document
/// carries is minted again under the diff group, by the one recipe
/// (`docs/dev/entity_ids.md`), and the anchors in the markdown follow
/// so a row still scrolls to its section. Only the places an id is an
/// id are rewritten — the anchor attributes and the frontmatter keys —
/// never a path: the file and its blobs are where the renderer put
/// them. `upstream_id` stays too: it is the backpointer to the real
/// thing, which is what a person copying it wants.
fn rekeyed(
    group: &str,
    markdown_uuid: &str,
    base: &RenderedMarkdown,
    rows: Vec<GridRow>,
    sections: Vec<Section>,
) -> RenderedMarkdown {
    let mint = |old: &str| {
        entity_id_str(
            IdNamespace::Datalib,
            Scope::SourceInstance(group),
            "diff",
            old,
        )
    };
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    map.insert(markdown_uuid.to_string(), mint(markdown_uuid));
    for row in &rows {
        map.entry(row.uuid.clone())
            .or_insert_with(|| mint(&row.uuid));
        let c = &row.conversation_uuid;
        map.entry(c.clone()).or_insert_with(|| mint(c));
    }
    let swap = |s: &str| -> String { map.get(s).cloned().unwrap_or_else(|| s.to_string()) };
    let swap_in = |s: &str, prefix: &str| -> String {
        let mut out = s.to_string();
        for (old, new) in &map {
            let needle = format!("{prefix}{old}");
            if out.contains(&needle) {
                out = out.replace(&needle, &format!("{prefix}{new}"));
            }
        }
        out
    };
    // The places an id is an id in a document: `id="m-…"`, every
    // `…-uuid="…"` attribute (`data-section-uuid`, `data-page-title-uuid`),
    // and the frontmatter's `markdown_uuid:` / `chat_uuid:` lines.
    let swap_md = |md: &str| -> String {
        let md = swap_in(md, "id=\"m-");
        let md = swap_in(&md, "-uuid=\"");
        let md = swap_in(&md, "markdown_uuid: ");
        swap_in(&md, "chat_uuid: ")
    };
    let rows = rows
        .into_iter()
        .map(|row| GridRow {
            uuid: swap(&row.uuid),
            markdown_uuid: row.markdown_uuid.as_deref().map(swap),
            conversation_uuid: swap(&row.conversation_uuid),
            entire_chat: swap_in(&row.entire_chat, "/chat/"),
            ..row
        })
        .collect();
    let sections = sections
        .into_iter()
        .map(|s| Section {
            uuid: s.uuid.as_deref().map(swap),
            md: swap_md(&s.md),
        })
        .collect();
    let edges = base
        .edges
        .iter()
        .map(|e| {
            let src_markdown_uuid = swap(&e.src_markdown_uuid);
            let src_anchor_uuid = e.src_anchor_uuid.as_deref().map(swap);
            EdgeRow {
                edge_uuid: datalib_id::edge_id(
                    &src_markdown_uuid,
                    src_anchor_uuid.as_deref(),
                    &e.dst_markdown_uuid,
                    e.dst_anchor_uuid.as_deref(),
                    e.label.as_deref(),
                ),
                src_markdown_uuid,
                src_anchor_uuid,
                ..e.clone()
            }
        })
        .collect();
    RenderedMarkdown {
        markdown_uuid: swap(markdown_uuid),
        source_id: group.to_string(),
        upstream_cursor: base.upstream_cursor.clone(),
        bucket_key: base.bucket_key.clone(),
        md_path: base.md_path.clone(),
        render_version: base.render_version,
        rows,
        sections,
        edges,
        problems: base.problems.clone(),
    }
}

/// Run every processor once at `pin`, scanning from `cursor`, and keep
/// what it emitted rather than storing it. The renderer still writes
/// its `.md` and blobs to their real paths; the file is read back at
/// once when the renderer declared no sections, and overwritten by the
/// subtraction's result later. The side stops at `max_documents`: a
/// diff that big was not the one asked for, and failing on the
/// document after the cap costs that many renders and no more.
#[allow(clippy::too_many_arguments)]
fn collect(
    processors: &[Box<dyn RenderProcessor>],
    name: &str,
    data_root: &Path,
    now: &str,
    progress: &Progress,
    cursor: Option<&str>,
    pin: &str,
    stale: &HashSet<String>,
    max_documents: usize,
) -> Result<Side> {
    let mut side = Side::default();
    let mut sectionless_sources: BTreeSet<String> = BTreeSet::new();
    let mut on_doc = |md: RenderedMarkdown| -> Result<()> {
        if side.capped
            || (side.docs.len() >= max_documents && !side.docs.contains_key(&md.markdown_uuid))
        {
            side.capped = true;
            return Err(DiffTooLarge {
                pin: pin.to_string(),
                max_documents,
            }
            .into());
        }
        let sections = if md.sections.is_empty() {
            sectionless_sources.insert(md.source_id.clone());
            whole_document_sections(&md)?
        } else {
            md.sections.clone()
        };
        side.docs
            .insert(md.markdown_uuid.clone(), Collected { md, sections });
        Ok(())
    };
    let mut on_declare = |bucket: &str, inputs: &[Input]| -> Result<()> {
        side.buckets.insert(bucket.to_string(), inputs.to_vec());
        Ok(())
    };
    // A diff compares documents; what a side could not parse is not
    // part of the comparison and has its own home in the source's
    // render store.
    let mut on_problems =
        |_: &datalib_etl_render::processor::ReadScope,
         _: &[datalib_schema::problems::ProblemRow]| Ok(());
    for proc in processors {
        let ctx = RenderCtx::new(
            name,
            data_root,
            now,
            progress,
            cursor,
            Some(pin),
            Some(stale),
            &mut on_doc,
            &mut on_declare,
            &mut on_problems,
        );
        futures::executor::block_on(proc.run(&ctx))
            .with_context(|| format!("processor {} at {pin}", proc.id()))?;
    }
    for source in sectionless_sources {
        tracing::warn!(
            source,
            "diff: this renderer declares no sections; its documents diff as one block, \
             with no added or removed section bands"
        );
    }
    if side.capped {
        return Err(DiffTooLarge {
            pin: pin.to_string(),
            max_documents,
        }
        .into());
    }
    Ok(side)
}

/// A renderer that declares no sections: its document is the frontmatter,
/// unkeyed, and the rest keyed by the document itself.
fn whole_document_sections(md: &RenderedMarkdown) -> Result<Vec<Section>> {
    let text = fs::read_to_string(&md.md_path)
        .with_context(|| format!("read back {}", md.md_path.display()))?;
    let body_at = text
        .strip_prefix("---\n")
        .and_then(|rest| rest.find("\n---\n").map(|n| 4 + n + 5))
        .unwrap_or(0);
    let (front, body) = text.split_at(body_at);
    let mut out = Vec::with_capacity(2);
    if !front.is_empty() {
        out.push(Section::unkeyed(front.to_string()));
    }
    out.push(Section::keyed(&md.markdown_uuid, body.to_string()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_is_split_off_and_the_rest_is_the_render_config() {
        let (pair, rest) = split_params(serde_json::json!({
            "diff": {"from": "a", "to": "b"},
            "common": {"x": 1}
        }))
        .unwrap();
        assert_eq!(
            pair,
            DiffPair {
                from: "a".into(),
                to: "b".into(),
                max_documents: DEFAULT_MAX_DOCUMENTS,
            }
        );
        assert_eq!(rest, serde_json::json!({"common": {"x": 1}}));
        let (pair, _) = split_params(serde_json::json!({
            "diff": {"from": "a", "to": "b", "max_documents": 7}
        }))
        .unwrap();
        assert_eq!(pair.max_documents, 7);
        for bad in [
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!("many"),
        ] {
            let err = split_params(
                serde_json::json!({"diff": {"from": "a", "to": "b", "max_documents": bad}}),
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("positive whole number"), "{err}");
        }
    }

    /// One processor, `n` documents; the sink refuses the one past the
    /// cap — and the side fails even when the processor, as contact-common
    /// does, logs a document's failure and carries on.
    #[test]
    fn a_side_past_the_cap_fails_the_step() {
        struct Emits(usize, bool);
        #[async_trait::async_trait]
        impl RenderProcessor for Emits {
            fn id(&self) -> &str {
                "emits"
            }
            async fn run(&self, ctx: &RenderCtx<'_>) -> Result<String> {
                for i in 0..self.0 {
                    let emitted = ctx.emit_doc(RenderedMarkdown {
                        markdown_uuid: format!("d{i}"),
                        source_id: "s".into(),
                        upstream_cursor: None,
                        bucket_key: None,
                        md_path: std::path::PathBuf::from(format!("/nonexistent/d{i}.md")),
                        render_version: 1,
                        rows: vec![],
                        sections: vec![Section::keyed(&format!("d{i}"), "x\n".into())],
                        edges: vec![],
                        problems: vec![],
                    });
                    // `true`: swallow the sink's answer, as a provider that
                    // treats every failed document as that document's own.
                    if !self.1 {
                        emitted?;
                    }
                }
                Ok("ok".into())
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let progress = Progress::default();
        let none: HashSet<String> = HashSet::new();
        let run = |n: usize, cap: usize, swallows: bool| {
            let procs: Vec<Box<dyn RenderProcessor>> = vec![Box::new(Emits(n, swallows))];
            collect(
                &procs,
                "s",
                dir.path(),
                "now",
                &progress,
                None,
                "c",
                &none,
                cap,
            )
        };
        assert_eq!(
            run(3, 3, false).unwrap().docs.len(),
            3,
            "at the cap is fine"
        );
        for swallows in [false, true] {
            let err = match run(4, 3, swallows) {
                Ok(_) => panic!("the fourth document must fail the side (swallows={swallows})"),
                Err(e) => e,
            };
            assert!(too_large(&err).is_some(), "{err:#}");
            let text = format!("{err:#}");
            assert!(text.contains("more than 3 document(s)"), "{text}");
            assert!(text.contains("max_documents"), "{text}");
        }
    }

    #[test]
    fn a_missing_or_degenerate_pair_is_refused() {
        let err = split_params(serde_json::json!({})).unwrap_err().to_string();
        assert!(err.contains("params.diff"), "{err}");
        let err = split_params(serde_json::json!({"diff": {"from": "a", "to": "a"}}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("same commit"), "{err}");
        let err = split_params(serde_json::json!({"diff": {"from": "a"}}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`to` is a raw commit"), "{err}");
        let err = split_params(serde_json::json!({"diff": {"from": "a", "to": "b", "x": 1}}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn rekeying_moves_every_id_together_and_leaves_paths_alone() {
        use datalib_schema::providers::Provider;
        let row = GridRow::builder()
            .uuid("m-1")
            .provider(Provider::Test)
            .kind("Message")
            .source_label("Test")
            .conversation_uuid("chat-1")
            .markdown_uuid(Some("chat-1".to_string()))
            .entire_chat("/chat/chat-1")
            .qmd_path(Some("src/render_markdown/chat-1/all.md".to_string()))
            .text("hi")
            .upstream_id(Some("ts-1".to_string()))
            .build()
            .unwrap();
        let base = RenderedMarkdown {
            markdown_uuid: "chat-1".into(),
            source_id: "src".into(),
            upstream_cursor: None,
            bucket_key: Some("chat-1".into()),
            md_path: "/root/src/render_markdown/chat-1/all.md".into(),
            render_version: 1,
            rows: vec![],
            sections: vec![],
            edges: vec![EdgeRow {
                edge_uuid: "old".into(),
                src_markdown_uuid: "chat-1".into(),
                src_anchor_uuid: Some("m-1".into()),
                dst_markdown_uuid: "other-doc".into(),
                dst_anchor_uuid: None,
                label: None,
            }],
            problems: vec![],
        };
        let sections = vec![Section::keyed(
            "m-1",
            "<div id=\"m-m-1\" data-section-uuid=\"m-1\">x ![p](blobs/m-1.png)</div>\n".into(),
        )];
        let doc = rekeyed("src-diff", "chat-1", &base, vec![row], sections);
        let new_doc = entity_id_str(
            IdNamespace::Datalib,
            Scope::SourceInstance("src-diff"),
            "diff",
            "chat-1",
        );
        let new_row = entity_id_str(
            IdNamespace::Datalib,
            Scope::SourceInstance("src-diff"),
            "diff",
            "m-1",
        );
        assert_eq!(doc.markdown_uuid, new_doc);
        assert_eq!(doc.source_id, "src-diff");
        assert_eq!(doc.rows[0].uuid, new_row);
        assert_eq!(doc.rows[0].markdown_uuid.as_deref(), Some(new_doc.as_str()));
        assert_eq!(doc.rows[0].conversation_uuid, new_doc);
        assert_eq!(doc.rows[0].entire_chat, format!("/chat/{new_doc}"));
        assert_eq!(
            doc.rows[0].qmd_path.as_deref(),
            Some("src/render_markdown/chat-1/all.md"),
            "the file is where the renderer put it"
        );
        assert_eq!(doc.rows[0].upstream_id.as_deref(), Some("ts-1"));
        assert_eq!(doc.sections[0].uuid.as_deref(), Some(new_row.as_str()));
        assert_eq!(
            doc.sections[0].md,
            format!(
                "<div id=\"m-{new_row}\" data-section-uuid=\"{new_row}\">x ![p](blobs/m-1.png)</div>\n"
            ),
            "anchors move, the blob path does not"
        );
        assert_eq!(doc.edges[0].src_markdown_uuid, new_doc);
        assert_eq!(
            doc.edges[0].src_anchor_uuid.as_deref(),
            Some(new_row.as_str())
        );
        assert_eq!(doc.edges[0].dst_markdown_uuid, "other-doc");
        assert_ne!(doc.edges[0].edge_uuid, "old");
        assert_eq!(doc.md_path, base.md_path);
    }

    #[test]
    fn a_sectionless_document_splits_at_its_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.md");
        fs::write(&path, "---\ntitle: t\n---\n\n# T\n\nbody\n").unwrap();
        let md = RenderedMarkdown {
            markdown_uuid: "d".into(),
            source_id: "s".into(),
            upstream_cursor: None,
            bucket_key: None,
            md_path: path,
            render_version: 1,
            rows: vec![],
            sections: vec![],
            edges: vec![],
            problems: vec![],
        };
        let sections = whole_document_sections(&md).unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].md, "---\ntitle: t\n---\n");
        assert_eq!(sections[1].uuid.as_deref(), Some("d"));
        assert_eq!(sections[1].md, "\n# T\n\nbody\n");
        assert_eq!(join(&sections), "---\ntitle: t\n---\n\n# T\n\nbody\n");
    }
}
