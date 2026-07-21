//! Render-side smoke test against the checked-in TNG fixture under
//! `tests/fixtures/anthropic_api`. Bazel doesn't surface fixture dirs
//! via `CARGO_MANIFEST_DIR` in the sandbox, so this lives as an
//! integration test tagged `manual` and is run via `cargo test`.

use frankweiler_etl_anthropic::render::parse::{parse_export, shred};
use std::collections::HashSet;
use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    if let Ok(d) = std::env::var("ANTHROPIC_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anthropic_api")
}

#[test]
fn parses_tng_api_fixture() {
    let parsed = parse_export(&fixture_dir()).expect("parse");

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
