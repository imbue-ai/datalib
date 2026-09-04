//! Render-side smoke test against the checked-in TNG fixture under
//! `tests/fixtures/claude_export`. Bazel doesn't surface fixture dirs
//! via `CARGO_MANIFEST_DIR` in the sandbox, so this lives as an
//! integration test tagged `manual` and is run via `cargo test`.

use datalib_etl_claude::download::export::{ingest, IngestOptions};
use datalib_etl_claude::render::parse::{parse, shred};
use std::collections::HashSet;
use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CLAUDE_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude_export")
}

#[tokio::test(flavor = "multi_thread")]
async fn parses_tng_api_fixture() {
    // Same path the pipeline takes: the export directory is ingested
    // into a raw store, and render reads that store.
    let raw = tempfile::tempdir().expect("raw");
    ingest(IngestOptions {
        db_path: raw.path().to_path_buf(),
        db: None,
        input_path: fixture_dir(),
        now: "2026-09-04T00:00:00-07:00".to_string(),
        progress: Default::default(),
        control: Default::default(),
    })
    .await
    .expect("ingest");
    let parsed = parse(raw.path(), None).expect("parse");

    assert!(!parsed.accounts.is_empty(), "expected accounts");
    assert!(!parsed.conversations.is_empty(), "expected conversations");

    let shredded: Vec<_> = parsed.conversations.iter().map(shred).collect();
    assert!(
        shredded.iter().any(|s| !s.messages.is_empty()),
        "expected messages"
    );

    let block_types: HashSet<_> = shredded
        .iter()
        .flat_map(|s| s.content_blocks.iter())
        .filter_map(|b| b.r#type.clone())
        .collect();
    for t in ["text", "thinking", "tool_use", "tool_result"] {
        assert!(
            block_types.contains(t),
            "expected block type {t:?} in {block_types:?}"
        );
    }

    let kinds: HashSet<_> = shredded
        .iter()
        .flat_map(|s| s.attachments.iter())
        .map(|a| a.kind.clone())
        .collect();
    assert!(kinds.contains("file"), "expected a 'file' kind attachment");
}
