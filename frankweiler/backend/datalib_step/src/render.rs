//! The render step driver: one source's translate wave, un-fused from
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
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::processor::{CheckpointSink, RunCtx};
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
    // Translate is synchronous render work driven by `futures`'
    // executor (NOT tokio's — see sync's render_processor_translate
    // for the double-runtime story); run it on a blocking thread.
    tokio::task::spawn_blocking(move || -> Result<()> {
        let checkpoints = CheckpointSink::new();
        let control = frankweiler_etl::control::ExtractControl::default();
        let now = String::new();
        let mut on_doc = |_md: RenderedMarkdown| -> Result<()> {
            docs_in.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        for proc in &planned.processors {
            let ctx = RunCtx::for_translate(
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
    Ok(vec![OutputClaim {
        // rendered_md always lives at the canonical path (only
        // raw_path is overridable).
        path: out_rel,
        changed: Some(docs > 0),
        version: None,
    }])
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
    use super::*;

    #[test]
    fn sidecar_fingerprints_scans_headers_and_survives_junk() {
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

        let fps = sidecar_fingerprints(&td.path().join("rendered_md")).unwrap();
        assert_eq!(fps.len(), 1);
        assert_eq!(fps["u1"], "f1");

        // Missing tree → empty map, no error (first run).
        assert!(sidecar_fingerprints(&td.path().join("nope"))
            .unwrap()
            .is_empty());
    }
}
