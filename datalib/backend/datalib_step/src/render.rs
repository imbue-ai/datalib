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

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use datalib_etl::grid_index::RenderedMarkdown;
use datalib_etl::processor::{CheckpointSink, RunCtx};
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
    let prior = sidecar_fingerprints(&rendered_root)?;
    tracing::info!(
        source = %planned.name,
        prior = prior.len(),
        "render: prior fingerprints from sidecar tree"
    );

    let docs = Arc::new(AtomicUsize::new(0));
    let out_rel = format!("{}/rendered_md", planned.name);
    let data_root = data_root.to_path_buf();
    let docs_in = docs.clone();
    // Render is synchronous work driven by `futures`' executor (NOT
    // tokio's — providers block_on their own internal futures); run it
    // on a blocking thread.
    tokio::task::spawn_blocking(move || -> Result<()> {
        let checkpoints = CheckpointSink::new();
        let control = datalib_etl::control::DownloadControl::default();
        let now = String::new();
        let mut on_doc = |_md: RenderedMarkdown| -> Result<()> {
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
        Ok(())
    })
    .await
    .context("render task panicked")??;

    let docs = docs.load(Ordering::SeqCst);
    tracing::info!(docs, "render: docs (re)rendered");
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

/// `markdown_uuid → source_fingerprint` for every sidecar under the
/// tree. Parses only the header; row payloads are skipped.
fn sidecar_fingerprints(rendered_root: &Path) -> Result<HashMap<String, String>> {
    #[derive(Deserialize)]
    struct HeaderOnly {
        header: Header,
    }
    #[derive(Deserialize)]
    struct Header {
        markdown_uuid: String,
        source_fingerprint: String,
    }

    let mut out = HashMap::new();
    if !rendered_root.is_dir() {
        return Ok(out);
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
                out.insert(h.header.markdown_uuid, h.header.source_fingerprint);
            }
            // A malformed sidecar just loses its skip — the doc
            // re-renders and the sidecar gets rewritten.
            Err(e2) => tracing::warn!("skip malformed sidecar {}: {e2}", e.path().display()),
        }
    }
    Ok(out)
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
    fn sidecar_fingerprints_scans_headers_and_survives_junk() {
        let td = tempfile::tempdir().unwrap();
        let tree = td.path().join("rendered_md/2026/05");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(
            tree.join("a.grid_rows.json"),
            r#"{"header":{"markdown_uuid":"u1","source_fingerprint":"f1"},"rows":[{"ignored":"payload"}]}"#,
        )
        .unwrap();
        std::fs::write(tree.join("b.grid_rows.json"), "not json").unwrap();
        std::fs::write(tree.join("a.md"), "# doc").unwrap();

        let fps = sidecar_fingerprints(&td.path().join("rendered_md")).unwrap();
        assert_eq!(fps.len(), 1);
        assert_eq!(fps["u1"], "f1");

        // Missing tree → empty map, no error (first run).
        assert!(sidecar_fingerprints(&td.path().join("nope"))
            .unwrap()
            .is_empty());
    }
}
