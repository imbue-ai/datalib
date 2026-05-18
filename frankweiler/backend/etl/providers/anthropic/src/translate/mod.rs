//! Anthropic Translate: raw API capture → parsed rows → markdown +
//! grid_rows sidecars. Stages 3-4 fill in render + sidecar emit;
//! `parse` is in place.

pub mod parse;

#[cfg(test)]
mod tests {
    use super::parse::parse_export;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anthropic_api")
    }

    #[test]
    fn parses_tng_api_fixture() {
        let parsed = parse_export(&fixture_dir()).expect("parse");

        assert!(!parsed.accounts.is_empty(), "expected accounts");
        assert!(!parsed.conversations.is_empty(), "expected conversations");
        assert!(!parsed.messages.is_empty(), "expected messages");

        // The API-shape fixture exercises non-text content blocks.
        let block_types: HashSet<_> = parsed
            .content_blocks
            .iter()
            .filter_map(|b| b.r#type.clone())
            .collect();
        for t in ["text", "thinking", "tool_use", "tool_result"] {
            assert!(
                block_types.contains(t),
                "expected block type {t:?} in {block_types:?}"
            );
        }

        // The image-file-only message produces a "file" attachment,
        // not "attachment".
        let kinds: HashSet<_> = parsed.attachments.iter().map(|a| a.kind.clone()).collect();
        assert!(kinds.contains("file"), "expected a 'file' kind attachment");
    }
}
