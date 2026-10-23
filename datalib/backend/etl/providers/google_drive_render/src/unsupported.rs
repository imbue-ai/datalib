//! A piece of a native file this renderer does not know. It is shown
//! where it was, as the JSON Google sent, and reported once per kind per
//! document, so a new element Google adds reaches the reader and the
//! Manage row rather than vanishing.

use datalib_etl_render::html::{escape_text, md_code_block};
use datalib_schema::problems::{Problem, Reason, Severity};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    /// The key that names what it is (`holodeckProgram`), or the field
    /// whose shape was not one we know.
    pub key: String,
    /// Where in the native JSON, as a JSON pointer.
    pub pointer: String,
    pub json: String,
}

impl Unsupported {
    pub fn new(key: &str, pointer: &str, value: &Value) -> Self {
        Self {
            key: key.to_string(),
            pointer: pointer.to_string(),
            json: serde_json::to_string_pretty(value).unwrap_or_default(),
        }
    }

    /// Collapsed, so a page full of them still reads as the document.
    pub fn details(&self) -> String {
        format!(
            "<details class=\"unsupported-json\">\n<summary>Not rendered: <code>{}</code></summary>\n\n{}\n\n</details>",
            escape_text(&self.key),
            md_code_block("json", &self.json),
        )
    }
}

/// One problem per kind of thing not rendered, at the first place it
/// was seen.
pub fn problems(found: &[Unsupported]) -> Vec<Problem> {
    let mut seen = std::collections::BTreeSet::new();
    found
        .iter()
        .filter(|u| seen.insert(u.key.as_str()))
        .map(|u| {
            Problem::field(&u.key, Reason::UnsupportedJson, &u.json)
                .at(&u.pointer)
                .severity(Severity::Warning)
        })
        .collect()
}
