//! Render-side smoke test against the checked-in TNG fixture under
//! `tests/fixtures/chatgpt_api`. Bazel doesn't surface fixture dirs via
//! `CARGO_MANIFEST_DIR` in the sandbox, so this lives as an integration
//! test tagged `manual` and is run via `cargo test`.

use frankweiler_etl_chatgpt::render::parse::{parse_api_dir, shred};
use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CHATGPT_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/chatgpt_api")
}

#[test]
fn parses_tng_fixture() {
    let parsed = parse_api_dir(&fixture_dir()).expect("parse");

    assert_eq!(parsed.accounts.len(), 1);
    assert_eq!(parsed.accounts[0].name.as_deref(), Some("Lt. Cmdr. Data"));

    let titles: std::collections::HashSet<_> = parsed
        .conversations
        .iter()
        .filter_map(|c| c.conv.title.clone())
        .collect();
    assert!(titles.contains("Sonnet on a Cat Named Spot"));
    assert!(titles.contains("Polynomial Fit for Sensor Calibration"));
    assert!(
        titles
            .iter()
            .any(|t| t.starts_with("I have been reviewing") && t.len() > 512),
        "expected long auto-title to be preserved"
    );

    let shredded: Vec<_> = parsed.conversations.iter().map(shred).collect();
    let has_meta = shredded
        .iter()
        .flat_map(|s| s.messages.iter())
        .any(|m| m.content_type.as_deref() == Some("model_editable_context"));
    assert!(has_meta);

    let has_python_code = shredded
        .iter()
        .flat_map(|s| s.content_parts.iter())
        .any(|p| p.kind == "code" && p.language.as_deref() == Some("python"));
    assert!(has_python_code);
}
