//! The render step driver: one source's render wave, un-fused from
//! Load.
//!
//! The provider's translate `DataProcessor`s (planned per-provider by
//! [`crate::dispatch`]) write the `.md` files and `.grid_rows.json`
//! sidecars themselves; the fused-Load callback sync installs is
//! replaced by a counter, so nothing here touches the index DB.
//! Incrementality comes from the same `prior_fingerprints` gate the
//! processors already consult — except the fingerprints are read back
//! from the sidecar tree on disk (the render step's own output)
//! rather than from the index DB. The sidecar tree is thus both the
//! artifact and the resume state, which is exactly the "mechanics
//! private to the node" contract.

use std::collections::BTreeSet;
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
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let progress = emitter.progress();
    let rendered_root = data_root.join(&planned.name).join("rendered_md");
    // Skip state and renderer versions come from the store: two indexed
    // reads, where this used to walk the whole tree and parse every
    // sidecar header to rebuild the same two answers.
    let declared = declared_render_versions(&planned.processors);
    let mut store = IndexedMarkdownStore::open(&rendered_root)
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
            .with_context(|| format!("reopen render store for {}", planned.name))?;
    }
    let prior = store.prior_fingerprints()?;
    tracing::info!(
        source = %planned.name,
        prior = prior.len(),
        "render: prior fingerprints from the store"
    );

    let docs = Arc::new(AtomicUsize::new(0));
    let out_rel = format!("{}/rendered_md", planned.name);
    // `planned` moves into the render task below; the post-render check
    // still needs the source's name for its message.
    let source_name = planned.name.clone();
    let data_root = data_root.to_path_buf();
    let docs_in = docs.clone();
    // Render is synchronous work driven by `futures`' executor (NOT
    // tokio's — providers block_on their own internal futures); run it
    // on a blocking thread.
    let versions_after = tokio::task::spawn_blocking(move || -> Result<BTreeSet<u32>> {
        let checkpoints = CheckpointSink::new();
        let control = datalib_etl::control::DownloadControl::default();
        let now = String::new();
        // Every finished document goes into the per-source store. The
        // providers already hand us a `RenderedMarkdown` carrying its
        // rows, edges, fingerprint and version through `ctx.emit_doc` —
        // the same value `grid_index::apply_one` consumes — so nothing
        // provider-side had to change to start writing a database.
        //
        // `problems` is empty here: the sink exists and this is the
        // path that will carry it, but no renderer reports into it yet.
        let mut on_doc = |md: RenderedMarkdown| -> Result<()> {
            store
                .put_document(&data_root, &md, &[])
                .with_context(|| format!("store document {}", md.markdown_uuid))?;
            docs_in.fetch_add(1, Ordering::SeqCst);
            Ok(())
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
            );
            futures::executor::block_on(proc.run(&ctx))
                .with_context(|| format!("processor {}", proc.id()))?;
        }
        // One commit for the whole render. Per-document commits would
        // put thousands of entries in `dolt_log` per run; committing
        // once is also what makes `dolt_diff` over this store answer
        // "what did this render change?".
        let stored = docs_in.load(Ordering::SeqCst);
        store
            .commit(&format!("render {}: {stored} document(s)", planned.name))
            .with_context(|| format!("commit render store for {}", planned.name))?;
        // The versions the tree now carries, read back from the store
        // that just wrote them — the post-render check needs them, and
        // the store is consumed by `close` here.
        let after = store.render_versions()?;
        store.close();
        Ok(after)
    })
    .await
    .context("render task panicked")??;

    let versions_on_disk = versions_after;
    let docs = docs.load(Ordering::SeqCst);
    tracing::info!(docs, "render: docs (re)rendered");
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

/// A content version for a rendered tree, read back from the cursor the
/// render just wrote.
///
/// The cursor records the raw store commit the tree was rendered from
/// and the render params it was rendered under. Together those
/// determine the tree's contents, so a render that found nothing new
/// leaves the same version behind — no need to walk and hash the whole
/// `rendered_md` tree to discover that.
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

/// Delete the whole rendered tree when it was written by a renderer
/// other than the one about to run. Returns whether it did.
///
/// A bumped `RENDER_VERSION` already re-cuts every `source_fingerprint`,
/// so the documents re-render on their own. That is enough only while a
/// document's output *path* is stable. It is not: chat-common writes to
/// `rendered_md/<chat_uuid>/<period>.md`, and `chat_uuid` is an id the
/// renderer mints — so a change to the id recipe (#216 put claude,
/// chatgpt and slack through `datalib_id`) writes every document to a
/// new directory and leaves the old one sitting beside it. The index
/// walks whatever it finds, so both copies load and every conversation
/// appears twice, under two different uuids.
///
/// Replacing the tree wholesale is the only cheap way out: the step
/// cannot tell an orphaned directory from a legitimately-untouched one,
/// because "untouched" is exactly what the fingerprint skip produces on
/// every healthy run. Everything under here is derived from the raw
/// store, so the cost is one re-render — no re-download.
///
/// `_render_cursor.json` goes with it, deliberately. It records the raw
/// commit the tree was rendered from, and the dolt-diff renderers ask
/// it what changed since; leaving it behind would answer "nothing" and
/// the emptied tree would stay empty until the next upstream write.
///
/// `current` is what [`declared_render_versions`] returned; `None`
/// disables the check.
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

/// Remove the whole rendered tree, store included.
fn discard_tree(rendered_root: &Path) -> Result<()> {
    if !rendered_root.exists() {
        return Ok(());
    }
    std::fs::remove_dir_all(rendered_root)
        .with_context(|| format!("remove stale rendered tree {}", rendered_root.display()))
}

/// Fail the step unless every `render_version` now on disk is one the
/// source's processors declared.
///
/// This is what keeps [`DataProcessor::render_version`] from being
/// advisory. The declaration decides whether a tree gets discarded, and
/// there are two ways for it to be wrong, both of which are silent
/// without this:
///
///   * **Absent.** A renderer that declares nothing opts its whole
///     source out of the staleness check, so a future re-key writes each
///     document into a new directory beside the old one and the index
///     loads both. Nothing errors and every document appears twice. A
///     provider added later inherits that by simply not overriding the
///     default, which is exactly the failure mode "add one line to each
///     provider" is bad at preventing — so a source that wrote sidecars
///     and declared nothing is an error here.
///   * **Wrong.** A processor that reports one version and writes
///     another marks every tree stale, *including the one it just
///     wrote*, and re-renders the source from scratch on every run.
///     Correct output, unbounded cost, no symptom but a slow pipeline.
///
/// Checked against the sidecars on disk rather than against what the
/// processors emitted through the doc callback, because the tree is what
/// the next run reads. A declaration that agrees with the callback and
/// disagrees with the file would still be wrong, and only this direction
/// notices.
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
                "to `emit_sidecar`."
            ),
            source = source,
            on_disk = on_disk,
        );
    };
    let undeclared: Vec<u32> = on_disk.difference(declared).copied().collect();
    if !undeclared.is_empty() {
        anyhow::bail!(
            concat!(
                "source `{source}`: sidecars under {root} carry render_version {undeclared:?}, ",
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

/// The set of `render_version`s this wave will stamp into sidecars, or
/// `None` when the declaration is incomplete.
///
/// `None` when **any** processor declines to declare one, because a
/// partial set is worse than no set: that processor's own sidecars would
/// look foreign against the versions its siblings reported, and the tree
/// would be deleted and rebuilt on every run. So an incomplete
/// declaration deletes nothing — and
/// [`every_sidecar_version_must_be_declared`] then fails the step at the
/// end of the wave, so "incomplete" is loud rather than silently
/// unchecked.
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
    //!
    //! The failure this guards is quiet: a re-keyed provider writes each
    //! document to a directory named for its *new* uuid, leaving the old
    //! directory in place. Nothing errors — the index just loads both
    //! and every conversation shows up twice under two different ids.
    //!
    //! These used to build the fixture by writing `.grid_rows.json`
    //! files. The versions now come from the store, so they write real
    //! documents through it.

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

    /// Write one document at `version` through the store — the shape
    /// chat-common produces, where the directory name is the minted id.
    fn write_doc(root: &Path, chat_uuid: &str, version: u32) {
        let store = IndexedMarkdownStore::open(root).unwrap();
        let row = GridRow::builder()
            .uuid(chat_uuid)
            .provider("test")
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
                &[],
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
