//! The render step driver: one source's render wave, un-fused from
//! Load.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use datalib_etl::grid_index::RenderedMarkdown;
use datalib_etl::processor::{CheckpointSink, DataProcessor, RunCtx};

use crate::dispatch::PlannedSource;
use crate::events::{Emitter, OutputClaim};
use datalib_etl::indexed_markdown::IndexedMarkdownStore;

pub async fn run(
    planned: PlannedSource,
    data_root: &Path,
    now: &str,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let progress = emitter.progress();
    let rendered_root = data_root.join(&planned.name).join("rendered_md");
    // Skip state and renderer versions come from the store: two indexed
    // reads, where this used to walk the whole tree and parse every
    // document's header to rebuild the same two answers.
    let declared = declared_render_versions(&planned.processors);
    let mut store = IndexedMarkdownStore::open(&rendered_root)
        .map(|s| s.with_now(now))
        .with_context(|| format!("open render store for {}", planned.name))?;
    // A tree an older renderer wrote can't be updated in place, only
    // replaced — see [`discard_tree_from_an_older_renderer`]. When it is
    // discarded there is nothing left to skip against, so every document
    // renders fresh.
    let on_disk = store.render_versions()?;
    if tree_is_from_an_older_renderer(&on_disk, declared.as_ref()) {
        progress.set_message("renderer version changed; re-rendering this source from scratch");
        // The store lives inside the tree being removed, so its pool has
        // to let go of the file first.
        store.close();
        discard_tree(&rendered_root)?;
        store = IndexedMarkdownStore::open(&rendered_root)
            .map(|s| s.with_now(now))
            .with_context(|| format!("reopen render store for {}", planned.name))?;
    }
    let prior = store.prior_fingerprints()?;
    tracing::info!(
        source = %planned.name,
        prior = prior.len(),
        "render: prior fingerprints from the store"
    );

    // What the source's mirror weighs. Measured out here because the
    // scan is async and `blocking()` cannot drive a future from inside
    // the `spawn_blocking` thread below.
    let measured = crate::introspect::scan(data_root, &planned.name)
        .await
        .with_context(|| format!("measure {}", planned.name))?;

    let docs = Arc::new(AtomicUsize::new(0));
    let removed = Arc::new(AtomicUsize::new(0));
    let out_rel = format!("{}/rendered_md", planned.name);
    // `planned` moves into the render task below; the post-render check
    // still needs the source's name for its message.
    let source_name = planned.name.clone();
    let data_root = data_root.to_path_buf();
    let docs_in = docs.clone();
    let removed_in = removed.clone();
    // `Progress` is a cheap clone; the render task takes one and this
    // one stays behind to report the problem counts afterwards.
    let progress_after = progress.clone();
    // The run-pinned "now" (`--now` / `$DATALIB_DAG_NOW`), so every
    // problem row this render writes carries one timestamp rather than
    // each renderer sampling its own clock. This used to be `""`, which
    // was harmless only for as long as nothing read `ctx.now` on the
    // render side; the problem sink does.
    let now = now.to_string();
    // Render is synchronous work driven by `futures`' executor (NOT
    // tokio's — providers block_on their own internal futures); run it
    // on a blocking thread.
    let versions_after =
        tokio::task::spawn_blocking(move || -> Result<(BTreeSet<u32>, HashMap<String, i64>)> {
            let checkpoints = CheckpointSink::new();
            let control = datalib_etl::control::DownloadControl::default();
            // Every finished document goes into the per-source store. The
            // providers already hand us a `RenderedMarkdown` carrying its
            // rows, edges, fingerprint, version and problems through
            // `ctx.emit_doc` — the same value `grid_index::apply_one`
            // consumes — so nothing provider-side had to change to start
            // writing a database.
            let mut on_doc = |md: RenderedMarkdown| -> Result<()> {
                store
                    .put_document(&data_root, &md)
                    .with_context(|| format!("store document {}", md.markdown_uuid))?;
                docs_in.fetch_add(1, Ordering::SeqCst);
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
                    removed_in.fetch_add(gone.len(), Ordering::SeqCst);
                    tracing::info!(
                        conversation = conversation_uuid,
                        documents = gone.len(),
                        "render: conversation is gone from the raw store; dropped its documents",
                    );
                }
                Ok(gone.len())
            };
            // A whole-store renderer declares the complete document set
            // instead of naming vanished ids: `retained` accumulates across
            // this source's processors and the sweep runs once, below.
            let mut retained: Option<BTreeSet<String>> = None;
            let mut on_retain = |seen: &std::collections::HashSet<String>| {
                retained
                    .get_or_insert_with(BTreeSet::new)
                    .extend(seen.iter().cloned());
            };
            for proc in &planned.processors {
                let ctx = RunCtx::for_render(
                    &planned.name,
                    &data_root,
                    &now,
                    &progress,
                    &control,
                    &prior,
                    &checkpoints,
                    &mut on_doc,
                    &mut on_remove,
                    &mut on_retain,
                );
                futures::executor::block_on(proc.run(&ctx))
                    .with_context(|| format!("processor {}", proc.id()))?;
            }

            // Every source gets a storage report, including the ones
            // that render no documents of their own — for `fsindex` and
            // `media` it is the only thing they put in the grid.
            //
            // Planned before the retain sweep below so its id can be
            // added to `keep`. That exemption is load-bearing:
            // `retained` is what the *provider's* processors declared
            // they hold, and they know nothing about this document — so
            // the sweep would delete it, and the fingerprint skip would
            // then decline to write it back on any run where no number
            // moved. The report would vanish from the grid and stay
            // gone.
            let storage = crate::introspect::plan(&data_root, &planned.name, measured, &now)?;
            if let (Some(m), Some(keep)) = (storage.as_ref(), retained.as_mut()) {
                keep.insert(m.doc.markdown_uuid.clone());
            }

            // The retain sweep, after every processor has had its say and
            // only on a run that got through them all: a render that failed
            // partway named a fraction of what it holds, and sweeping on
            // that would delete the rest. `?` above already returned.
            if let Some(keep) = retained {
                for uuid in store.all_document_uuids()? {
                    if keep.contains(&uuid) {
                        continue;
                    }
                    store
                        .remove_document(&data_root, &uuid)
                        .with_context(|| format!("remove document {uuid}"))?;
                    removed_in.fetch_add(1, Ordering::SeqCst);
                    tracing::info!(
                        document = %uuid,
                        "render: this source no longer produces this document; dropped it",
                    );
                }
            }

            // Written after the sweep, so a report the sweep could not
            // see (this source has none yet) is still created.
            //
            // Skipped whole when no number moved: the report would be
            // byte-identical, and appending a sample saying "still the
            // same" would grow the store on a run where nothing
            // happened.
            if let Some(m) = storage {
                if prior.get(&m.doc.markdown_uuid) == Some(&m.doc.source_fingerprint) {
                    tracing::debug!(
                        source = %planned.name,
                        "render: storage unchanged since the last run"
                    );
                } else {
                    m.write_report().with_context(|| {
                        format!("write the storage report for {}", planned.name)
                    })?;
                    store
                        .put_document(&data_root, &m.doc)
                        .with_context(|| format!("store storage report for {}", planned.name))?;
                    store
                        .put_measurements(&m.samples)
                        .with_context(|| format!("append measurements for {}", planned.name))?;
                    docs_in.fetch_add(1, Ordering::SeqCst);
                }
            }

            // One commit for the whole render. Per-document commits would
            // put thousands of entries in `dolt_log` per run; committing
            // once is also what makes `dolt_diff` over this store answer
            // "what did this render change?".
            let stored = docs_in.load(Ordering::SeqCst);
            let dropped = removed_in.load(Ordering::SeqCst);
            let msg = if dropped == 0 {
                format!("render {}: {stored} document(s)", planned.name)
            } else {
                format!(
                    "render {}: {stored} document(s), {dropped} removed upstream",
                    planned.name
                )
            };
            store
                .commit(&msg)
                .with_context(|| format!("commit render store for {}", planned.name))?;
            // The versions the tree now carries, read back from the store
            // that just wrote them — the post-render check needs them, and
            // the store is consumed by `close` here. Problem counts come
            // back the same way, so the step can say what it dropped.
            let after = store.render_versions()?;
            let problems = store.problem_counts()?;
            store.close();
            Ok((after, problems))
        })
        .await
        .context("render task panicked")??;

    let (versions_on_disk, problem_counts) = versions_after;
    let docs = docs.load(Ordering::SeqCst);
    let removed = removed.load(Ordering::SeqCst);
    tracing::info!(docs, removed, "render: docs (re)rendered");
    if removed > 0 {
        progress_after.set_message(&format!(
            "{removed} document(s) dropped — their source is gone upstream"
        ));
    }
    // Say out loud what the sink holds. A problem store nothing ever
    // reads is indistinguishable from one that is empty because
    // everything is fine — and the more dangerous of those two reads as
    // success. These are whole-store counts, not this-run counts: a
    // problem on a document this run skipped is still current, which is
    // the point of the per-document sweep.
    if !problem_counts.is_empty() {
        let total: i64 = problem_counts.values().sum();
        let dropped = problem_counts.get("dropped").copied().unwrap_or(0);
        let nulled = problem_counts.get("nulled").copied().unwrap_or(0);
        tracing::warn!(
            source = %source_name,
            total,
            dropped,
            nulled,
            "render: rows this source could not fully project \
             (see render_problems in its indexed_markdown.doltlite_db)"
        );
        progress_after.set_message(&format!(
            "{total} row(s) with render problems ({dropped} dropped, {nulled} degraded)"
        ));
    }
    every_stored_version_must_be_declared(
        &source_name,
        &rendered_root,
        &versions_on_disk,
        declared.as_ref(),
    )?;
    // The whole tree re-renders from raw/, so cache-aware backups
    // (`restic --exclude-caches` etc.) may skip it. No-op until the
    // first render materializes the dir.
    datalib_core::layout::mark_derived_cache(&rendered_root);
    match rendered_tree_version(&rendered_root) {
        // rendered_md always lives at the canonical path (only
        // raw_path is overridable).
        Some(version) => Ok(vec![OutputClaim {
            path: out_rel,
            version,
        }]),
        // No cursor: a provider that hasn't been ported to the
        // dolt-diff render path, so we have nothing content-derived to
        // vouch for. The runner hashes the tree instead.
        None => Ok(vec![]),
    }
}

fn rendered_tree_version(rendered_root: &Path) -> Option<String> {
    let path = rendered_root.join("_render_cursor.json");
    let cursor = match datalib_etl::render_cursor::read(&path) {
        Ok(c) => c?,
        // The cursor is written without an atomic rename, so a crash
        // mid-write leaves truncated JSON. Falling back to the hash is
        // correct, but doing it silently looks identical to "provider
        // not ported yet" and would stay that way forever.
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %format!("{e:#}"),
                "render: unreadable render cursor; reporting no version,                  so the runner will content-hash the tree"
            );
            return None;
        }
    };
    let params = cursor
        .params
        .as_ref()
        .map(|p| p.to_string())
        .unwrap_or_default();
    Some(format!(
        "raw:{} params:{}",
        cursor.last_rendered_hash,
        blake3::hash(params.as_bytes()).to_hex()
    ))
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
         removing it and re-rendering from the raw store"
    );
    true
}

fn discard_tree(rendered_root: &Path) -> Result<()> {
    if !rendered_root.exists() {
        return Ok(());
    }
    std::fs::remove_dir_all(rendered_root)
        .with_context(|| format!("remove stale rendered tree {}", rendered_root.display()))
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
                "which none of its processors declare (declared: {declared:?}). A processor ",
                "that reports one version and writes another marks every tree stale — ",
                "including the one it just wrote — and re-renders this source from scratch on ",
                "every run."
            ),
            source = source,
            root = rendered_root.display(),
            undeclared = undeclared,
            declared = declared,
        );
    }
    Ok(())
}

fn declared_render_versions(processors: &[Box<dyn DataProcessor>]) -> Option<BTreeSet<u32>> {
    let versions: BTreeSet<u32> = processors
        .iter()
        .map(|p| p.render_version())
        .collect::<Option<_>>()?;
    (!versions.is_empty()).then_some(versions)
}

#[cfg(test)]
mod tests {

    /// The reported version must be stable for an unchanged tree and
    /// move when either half of what determines the tree moves. Both
    /// failure modes are silent: a version that drifts re-indexes
    /// forever, one that sticks skips real work.
    #[test]
    fn rendered_tree_version_is_stable_and_moves_with_source_or_params() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("slack/rendered_md");
        let cursor = root.join("_render_cursor.json");
        let params = |p: &str| serde_json::json!({ "period": p });

        datalib_etl::render_cursor::write(&cursor, "commit-a", None, &params("month")).unwrap();
        let v1 = rendered_tree_version(&root).expect("cursor present");
        // A second render that found nothing new rewrites the same
        // cursor; the version must not budge.
        datalib_etl::render_cursor::write(&cursor, "commit-a", None, &params("month")).unwrap();
        assert_eq!(rendered_tree_version(&root).as_deref(), Some(v1.as_str()));

        // New upstream data.
        datalib_etl::render_cursor::write(&cursor, "commit-b", None, &params("month")).unwrap();
        let v2 = rendered_tree_version(&root).unwrap();
        assert_ne!(v1, v2, "a new source commit must move the version");

        // Same data, different render knob: the tree differs, so the
        // version must too, or the index keeps the old rendering.
        datalib_etl::render_cursor::write(&cursor, "commit-b", None, &params("week")).unwrap();
        assert_ne!(
            rendered_tree_version(&root).unwrap(),
            v2,
            "a render param change must move the version"
        );
    }

    /// No cursor (a provider not on the dolt-diff render path) means no
    /// version, and the runner content-hashes instead.
    #[test]
    fn rendered_tree_version_is_none_without_a_cursor() {
        let td = tempfile::tempdir().unwrap();
        assert!(rendered_tree_version(&td.path().join("nope")).is_none());
    }

    /// A truncated cursor — the file is written without an atomic
    /// rename — must not be mistaken for "no cursor" silently.
    #[test]
    fn rendered_tree_version_is_none_for_an_unreadable_cursor() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("slack/rendered_md");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("_render_cursor.json"), "{ truncated").unwrap();
        assert!(rendered_tree_version(&root).is_none());
    }
    use super::*;
}

#[cfg(test)]
mod stale_tree_tests {
    //! A rendered tree written by a different renderer version is
    //! replaced, not updated.

    use std::collections::BTreeSet;
    use std::path::Path;

    use anyhow::Result;
    use datalib_etl::grid_index::RenderedMarkdown;
    use datalib_etl::indexed_markdown::IndexedMarkdownStore;
    use datalib_etl::processor::{DataProcessor, RunCtx};
    use datalib_schema::grid_rows::GridRow;

    use super::{
        declared_render_versions, discard_tree, every_stored_version_must_be_declared,
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
                    source_name: "claude_web".into(),
                    source_fingerprint: format!("fp-{chat_uuid}"),
                    upstream_cursor: None,
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

    fn fingerprint_count(root: &Path) -> usize {
        let store = IndexedMarkdownStore::open(root).unwrap();
        let n = store.prior_fingerprints().unwrap().len();
        store.close();
        n
    }

    fn versions(vs: &[u32]) -> BTreeSet<u32> {
        vs.iter().copied().collect()
    }

    /// A tree at an older version is deleted, and the render that
    /// follows has no fingerprints left to skip against.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tree_from_an_older_renderer_is_discarded() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("claude_web/rendered_md");
        write_doc(&root, "old-uuid", 4);
        assert_eq!(fingerprint_count(&root), 1, "the fixture must be readable");

        let on_disk = stored_versions(&root);
        assert!(tree_is_from_an_older_renderer(
            &on_disk,
            Some(&versions(&[5]))
        ));
        discard_tree(&root).unwrap();

        assert!(
            !root.exists(),
            "the tree is replaced, not merged: leaving the old directory \
             behind is what puts every document in the index twice"
        );
    }

    /// A tree at the current version is left alone. Without this, every
    /// run would delete and re-render the whole source — correct output,
    /// and the incrementality silently gone.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_current_tree_is_kept() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("claude_web/rendered_md");
        write_doc(&root, "uuid-a", 5);
        write_doc(&root, "uuid-b", 5);

        let on_disk = stored_versions(&root);
        assert!(!tree_is_from_an_older_renderer(
            &on_disk,
            Some(&versions(&[5]))
        ));
        assert_eq!(fingerprint_count(&root), 2);
    }

    /// An empty tree — a first run — is not "stale".
    #[tokio::test(flavor = "multi_thread")]
    async fn a_first_run_has_nothing_to_discard() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("claude_web/rendered_md");
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
        let root = td.path().join("claude_web/rendered_md");
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
            Path::new("/tmp/claude_web/rendered_md"),
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
            Path::new("/tmp/claude_web/rendered_md"),
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
            Path::new("/tmp/claude_web/rendered_md"),
            &versions(&[5]),
            Some(&versions(&[5])),
        )
        .expect("declared 5, wrote 5");
        every_stored_version_must_be_declared(
            "empty",
            Path::new("/tmp/empty/rendered_md"),
            &BTreeSet::new(),
            None,
        )
        .expect("nothing written, nothing to declare");
    }

    struct Stub(Option<u32>);

    #[async_trait::async_trait]
    impl DataProcessor for Stub {
        fn id(&self) -> &str {
            "stub"
        }
        async fn run(&self, _ctx: &RunCtx<'_>) -> Result<String> {
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
        let mixed: Vec<Box<dyn DataProcessor>> =
            vec![Box::new(Stub(Some(5))), Box::new(Stub(None))];
        assert_eq!(declared_render_versions(&mixed), None);

        let all_declared: Vec<Box<dyn DataProcessor>> =
            vec![Box::new(Stub(Some(5))), Box::new(Stub(Some(5)))];
        assert_eq!(
            declared_render_versions(&all_declared),
            Some(versions(&[5]))
        );

        assert_eq!(declared_render_versions(&[]), None);
    }
}
