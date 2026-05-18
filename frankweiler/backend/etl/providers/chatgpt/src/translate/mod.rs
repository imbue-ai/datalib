//! ChatGPT Translate: raw API capture → parsed rows → markdown +
//! grid_rows sidecars. Stages 3-4 of the porting plan fill in the
//! render + sidecar emit; `parse` is in place.

pub mod grid_rows;
pub mod parse;
pub mod render;

#[cfg(test)]
mod tests {
    use super::parse::parse_api_dir;
    use std::path::PathBuf;

    fn fixture_dir() -> PathBuf {
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
            .filter_map(|c| c.title.clone())
            .collect();
        assert!(titles.contains("Sonnet on a Cat Named Spot"));
        assert!(titles.contains("Polynomial Fit for Sensor Calibration"));
        // Third conversation: the auto-titler leaves the full first user
        // message as the title — must survive parse without truncation.
        assert!(
            titles
                .iter()
                .any(|t| t.starts_with("I have been reviewing") && t.len() > 512),
            "expected long auto-title to be preserved"
        );

        // The sonnet thread has model_editable_context + code parts in
        // the polyfit thread.
        let has_meta = parsed
            .messages
            .iter()
            .any(|m| m.content_type.as_deref() == Some("model_editable_context"));
        assert!(has_meta);

        let has_python_code = parsed
            .content_parts
            .iter()
            .any(|p| p.kind == "code" && p.language.as_deref() == Some("python"));
        assert!(has_python_code);
    }
}
