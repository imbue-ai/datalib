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

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use datalib_etl::grid_index::RenderedMarkdown;
use datalib_etl::processor::{CheckpointSink, DataProcessor, RunCtx};
use serde::Deserialize;

use crate::dispatch::PlannedSource;
use crate::events::{Emitter, OutputClaim};

pub async fn run(
    planned: PlannedSource,
    data_root: &Path,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let progress = emitter.progress();
    let rendered_root = data_root.join(&planned.name).join("rendered_md");
    // Skip state and renderer versions come from the store now, not from
    // walking the tree and parsing every sidecar header. The walk still
    // happens below *as a cross-check* while both writers are live; it
    // goes away with the sidecars themselves.
    let scan = scan_sidecars(&rendered_root)?;
    // A tree an older renderer wrote can't be updated in place, only
    // replaced — see [`discard_tree_from_an_older_renderer`]. When it is
    // discarded there is nothing left to skip against, so every document
    // renders fresh.
    let declared = declared_render_versions(&planned.processors);
    let discarded = discard_tree_from_an_older_renderer(&rendered_root, &scan, declared.as_ref())?;
    let mut versions_on_disk = scan.versions;
    let prior = if discarded {
        progress.set_message("renderer version changed; re-rendering this source from scratch");
        versions_on_disk.clear();
        HashMap::new()
    } else {
        scan.fingerprints
    };
    tracing::info!(
        source = %planned.name,
        prior = prior.len(),
        "render: prior fingerprints from sidecar tree"
    );

    let docs = Arc::new(AtomicUsize::new(0));
    let out_rel = format!("{}/rendered_md", planned.name);
    // `planned` moves into the render task below; the post-render check
    // still needs the source's name for its message.
    let source_name = planned.name.clone();
    let data_root = data_root.to_path_buf();
    let docs_in = docs.clone();
    // The store lives inside the tree, so the render task needs its own
    // copy of the path; the post-render checks below still use the
    // original.
    let store_root = rendered_root.clone();
    // Render is synchronous work driven by `futures`' executor (NOT
    // tokio's — providers block_on their own internal futures); run it
    // on a blocking thread.
    tokio::task::spawn_blocking(move || -> Result<()> {
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
        let store = datalib_etl::indexed_markdown::IndexedMarkdownStore::open(&store_root)
            .with_context(|| format!("open render store for {}", planned.name))?;
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
        store.close();
        Ok(())
    })
    .await
    .context("render task panicked")??;

    let docs = docs.load(Ordering::SeqCst);
    tracing::info!(docs, "render: docs (re)rendered");
    // Nothing rendered ⇒ nothing on disk moved, so the pre-render scan
    // still describes the tree and a second walk would only re-read it.
    if docs > 0 {
        versions_on_disk = scan_sidecars(&rendered_root)?.versions;
    }
    every_sidecar_version_must_be_declared(
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

/// What one walk of the sidecar tree yields.
struct SidecarScan {
    /// `markdown_uuid → source_fingerprint`, the skip state.
    fingerprints: HashMap<String, String>,
    /// Every distinct `render_version` stamped in a header under the
    /// tree. A healthy tree holds exactly the versions the current
    /// processors produce; anything else was written by a different
    /// build. See [`discard_tree_from_an_older_renderer`].
    versions: BTreeSet<u32>,
}

/// Read every sidecar header under the tree once. Row payloads are
/// skipped — this parses the header and nothing else.
fn scan_sidecars(rendered_root: &Path) -> Result<SidecarScan> {
    #[derive(Deserialize)]
    struct HeaderOnly {
        header: Header,
    }
    #[derive(Deserialize)]
    struct Header {
        markdown_uuid: String,
        source_fingerprint: String,
        render_version: u32,
    }

    let mut scan = SidecarScan {
        fingerprints: HashMap::new(),
        versions: BTreeSet::new(),
    };
    if !rendered_root.is_dir() {
        return Ok(scan);
    }
    for e in walkdir::WalkDir::new(rendered_root) {
        let e = e?;
        if !e.file_type().is_file() {
            continue;
        }
        let Some(name) = e.file_name().to_str() else {
            continue;
        };
        if !name.ends_with(".grid_rows.json") {
            continue;
        }
        let raw = std::fs::read_to_string(e.path())
            .with_context(|| format!("read {}", e.path().display()))?;
        match serde_json::from_str::<HeaderOnly>(&raw) {
            Ok(h) => {
                scan.versions.insert(h.header.render_version);
                scan.fingerprints
                    .insert(h.header.markdown_uuid, h.header.source_fingerprint);
            }
            // A malformed sidecar just loses its skip — the doc
            // re-renders and the sidecar gets rewritten.
            Err(e2) => tracing::warn!("skip malformed sidecar {}: {e2}", e.path().display()),
        }
    }
    Ok(scan)
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
fn discard_tree_from_an_older_renderer(
    rendered_root: &Path,
    scan: &SidecarScan,
    current: Option<&BTreeSet<u32>>,
) -> Result<bool> {
    let Some(current) = current else {
        return Ok(false);
    };
    if scan.versions.is_empty() || scan.versions.is_subset(current) {
        return Ok(false);
    }

    tracing::warn!(
        on_disk = ?scan.versions,
        current = ?current,
        path = %rendered_root.display(),
        "render: rendered tree came from a different renderer version; \
         removing it and re-rendering from the raw store"
    );
    std::fs::remove_dir_all(rendered_root)
        .with_context(|| format!("remove stale rendered tree {}", rendered_root.display()))?;
    Ok(true)
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
fn every_sidecar_version_must_be_declared(
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
                "source `{source}` wrote .grid_rows.json sidecars (render_version {on_disk:?}) ",
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

    #[test]
    fn scan_sidecars_reads_headers_and_survives_junk() {
        let td = tempfile::tempdir().unwrap();
        let tree = td.path().join("rendered_md/2026/05");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(
            tree.join("a.grid_rows.json"),
            r#"{"header":{"markdown_uuid":"u1","source_fingerprint":"f1","render_version":3},"rows":[{"ignored":"payload"}]}"#,
        )
        .unwrap();
        std::fs::write(tree.join("b.grid_rows.json"), "not json").unwrap();
        std::fs::write(tree.join("a.md"), "# doc").unwrap();

        let scan = scan_sidecars(&td.path().join("rendered_md")).unwrap();
        assert_eq!(scan.fingerprints.len(), 1);
        assert_eq!(scan.fingerprints["u1"], "f1");
        // The header's version comes back too — the staleness check has
        // nothing else to go on, and a scan that dropped it would leave
        // every tree looking current.
        assert_eq!(scan.versions, [3].into_iter().collect());

        // Missing tree → empty scan, no error (first run).
        let none = scan_sidecars(&td.path().join("nope")).unwrap();
        assert!(none.fingerprints.is_empty());
        assert!(none.versions.is_empty());
    }
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

    use std::collections::BTreeSet;
    use std::path::Path;

    use anyhow::Result;
    use datalib_etl::processor::{DataProcessor, RunCtx};

    use super::{
        declared_render_versions, discard_tree_from_an_older_renderer,
        every_sidecar_version_must_be_declared, scan_sidecars,
    };

    /// Write one document's sidecar at `version` under
    /// `<root>/<chat_uuid>/all.grid_rows.json` — the shape chat-common
    /// produces, where the directory name is the minted id.
    fn write_doc(root: &Path, chat_uuid: &str, version: u32) {
        let dir = root.join(chat_uuid);
        std::fs::create_dir_all(&dir).unwrap();
        let sidecar = serde_json::json!({
            "header": {
                "markdown_uuid": chat_uuid,
                "source_fingerprint": format!("fp-{chat_uuid}"),
                "render_version": version,
            },
            "rows": [],
        });
        std::fs::write(
            dir.join("all.grid_rows.json"),
            serde_json::to_string(&sidecar).unwrap(),
        )
        .unwrap();
    }

    fn versions(vs: &[u32]) -> BTreeSet<u32> {
        vs.iter().copied().collect()
    }

    /// A tree at an older version is deleted, and the render that
    /// follows has no fingerprints left to skip against.
    #[test]
    fn a_tree_from_an_older_renderer_is_discarded() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("claude_web/rendered_md");
        write_doc(&root, "old-uuid", 4);

        let scan = scan_sidecars(&root).unwrap();
        assert_eq!(scan.fingerprints.len(), 1, "the fixture must be readable");

        let discarded =
            discard_tree_from_an_older_renderer(&root, &scan, Some(&versions(&[5]))).unwrap();

        assert!(discarded, "a v4 tree must not survive a v5 renderer");
        assert!(
            !root.exists(),
            "the tree is replaced, not merged: leaving the old directory \
             behind is what puts every document in the index twice"
        );
    }

    /// The render cursor goes with the tree.
    ///
    /// Keeping it would be worse than doing nothing: the dolt-diff
    /// renderers ask the cursor what changed since the last raw commit,
    /// and against an emptied tree the honest answer — "nothing" —
    /// leaves the source with no rendered documents at all until
    /// something upstream moves.
    #[test]
    fn discarding_the_tree_takes_the_render_cursor_with_it() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("claude_web/rendered_md");
        write_doc(&root, "old-uuid", 4);
        datalib_etl::render_cursor::write(
            &root.join("_render_cursor.json"),
            "commit-a",
            None,
            &serde_json::json!({}),
        )
        .unwrap();

        let scan = scan_sidecars(&root).unwrap();
        discard_tree_from_an_older_renderer(&root, &scan, Some(&versions(&[5]))).unwrap();

        assert!(
            !root.join("_render_cursor.json").exists(),
            "a cursor surviving the wipe would report nothing to re-render"
        );
    }

    /// A tree at the current version is left alone. Without this, every
    /// run would delete and re-render the whole source — correct output,
    /// and the incrementality silently gone.
    #[test]
    fn a_current_tree_is_kept() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("claude_web/rendered_md");
        write_doc(&root, "uuid-a", 5);
        write_doc(&root, "uuid-b", 5);

        let scan = scan_sidecars(&root).unwrap();
        let discarded =
            discard_tree_from_an_older_renderer(&root, &scan, Some(&versions(&[5]))).unwrap();

        assert!(!discarded);
        assert_eq!(scan_sidecars(&root).unwrap().fingerprints.len(), 2);
    }

    /// An empty tree — a first run — is not "stale".
    #[test]
    fn a_first_run_has_nothing_to_discard() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("claude_web/rendered_md");

        let scan = scan_sidecars(&root).unwrap();
        assert!(!discard_tree_from_an_older_renderer(&root, &scan, Some(&versions(&[5]))).unwrap());
    }

    /// A source whose processors don't all declare a version deletes
    /// nothing, whatever is in its tree — acting on a partial
    /// declaration is how you delete a live document. The run still
    /// fails, at the post-render check rather than here.
    #[test]
    fn an_undeclared_version_deletes_nothing() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("claude_web/rendered_md");
        write_doc(&root, "old-uuid", 4);

        let scan = scan_sidecars(&root).unwrap();
        let discarded = discard_tree_from_an_older_renderer(&root, &scan, None).unwrap();

        assert!(!discarded);
        assert!(root.exists());
    }

    /// A renderer that writes sidecars and declares nothing fails the
    /// step rather than silently opting its source out of the staleness
    /// check. This is the assertion that makes the trait method
    /// mandatory in practice — without it, "every provider declares one"
    /// is a convention that a new provider breaks by doing nothing.
    #[test]
    fn writing_sidecars_without_declaring_a_version_is_an_error() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("claude_web/rendered_md");
        write_doc(&root, "uuid-a", 5);

        let scan = scan_sidecars(&root).unwrap();
        let err = every_sidecar_version_must_be_declared("claude_web", &root, &scan.versions, None)
            .expect_err("a source with sidecars and no declaration must fail");
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
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("claude_web/rendered_md");
        write_doc(&root, "uuid-a", 4);

        let scan = scan_sidecars(&root).unwrap();
        let err = every_sidecar_version_must_be_declared(
            "claude_web",
            &root,
            &scan.versions,
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
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("claude_web/rendered_md");
        write_doc(&root, "uuid-a", 5);

        let scan = scan_sidecars(&root).unwrap();
        every_sidecar_version_must_be_declared(
            "claude_web",
            &root,
            &scan.versions,
            Some(&versions(&[5])),
        )
        .expect("declared 5, wrote 5");

        // No sidecars — a download-only source, or a first run that
        // found nothing — must not trip the "declare something" rule.
        every_sidecar_version_must_be_declared("empty", &root, &BTreeSet::new(), None)
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
    /// processor's own sidecars look foreign, so the tree — including
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

        // A source with no processors at all has nothing to compare.
        assert_eq!(declared_render_versions(&[]), None);
    }
}
