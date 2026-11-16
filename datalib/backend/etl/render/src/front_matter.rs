//! The YAML front matter that opens every rendered document. A renderer
//! hands it as facts ([`Front`]), which the render store keeps and this
//! module draws; a value from upstream goes through [`yaml_scalar`], so
//! nothing in it can end its line or the block, and a title through
//! [`yaml_name`].

use serde::{Deserialize, Serialize};

/// A document's front matter: its keys in order, each with its value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Front(Vec<Fact>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    pub key: String,
    pub value: FrontValue,
}

/// One value, and how it is written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "as", content = "value", rename_all = "snake_case")]
pub enum FrontValue {
    /// A word or a number we wrote: an id, a provider tag, a count, a
    /// stamp we formatted. Drawn as it is.
    Plain(String),
    /// Text from upstream, drawn by [`yaml_scalar`].
    Text(String),
    /// A title or a name, drawn by [`yaml_name`].
    Name(String),
    /// Nothing known, drawn as `null`.
    Null,
}

impl Front {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn facts(&self) -> &[Fact] {
        &self.0
    }

    pub fn with(mut self, key: impl Into<String>, value: FrontValue) -> Self {
        self.0.push(Fact {
            key: key.into(),
            value,
        });
        self
    }

    pub fn plain(self, key: impl Into<String>, value: impl std::fmt::Display) -> Self {
        self.with(key, FrontValue::Plain(value.to_string()))
    }

    pub fn text(self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.with(key, FrontValue::Text(value.into()))
    }

    pub fn name(self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.with(key, FrontValue::Name(value.into()))
    }

    /// The key only when there is a value.
    pub fn text_if(self, key: impl Into<String>, value: Option<&str>) -> Self {
        match value {
            Some(v) => self.text(key, v),
            None => self,
        }
    }

    /// The key always, `null` when there is no value.
    pub fn text_or_null(self, key: impl Into<String>, value: Option<&str>) -> Self {
        match value {
            Some(v) => self.text(key, v),
            None => self.with(key, FrontValue::Null),
        }
    }

    /// The block as a document opens with it, the blank line after it
    /// included.
    pub fn draw(&self) -> String {
        let mut out = String::from("---\n");
        for fact in &self.0 {
            let value = match &fact.value {
                FrontValue::Plain(v) => v.clone(),
                FrontValue::Text(v) => yaml_scalar(v),
                FrontValue::Name(v) => yaml_name(v),
                FrontValue::Null => "null".to_string(),
            };
            out.push_str(&format!("{}: {value}\n", fact.key));
        }
        out.push_str("---\n\n");
        out
    }

    /// As `markdowns.front` stores it.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("front matter always serializes")
    }

    /// `None` for a column this build cannot read.
    pub fn from_json(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }
}

/// `s` as a YAML scalar, JSON-quoted: YAML reads a JSON string as a
/// double-quoted scalar, and in one a line break, a quote or a `---` is
/// an escape or plain text rather than structure.
pub fn yaml_scalar(s: &str) -> String {
    serde_json::to_string(s).expect("a string serializes to JSON")
}

/// A title or a name as a YAML scalar: [`yaml_scalar`] without its bidi
/// controls (`crate::html::without_bidi_controls`).
pub fn yaml_name(s: &str) -> String {
    yaml_scalar(&crate::html::without_bidi_controls(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_draws_each_kind_of_value_and_round_trips() {
        let front = Front::new()
            .name("title", "Ten \u{202e}Forward")
            .plain("item_count", 3)
            .text("account", "a: b")
            .text_if("project", None)
            .text_or_null("merged_at", None);
        assert_eq!(
            front.draw(),
            "---\ntitle: \"Ten Forward\"\nitem_count: 3\naccount: \"a: b\"\nmerged_at: null\n---\n\n"
        );
        assert_eq!(Front::from_json(&front.to_json()), Some(front));
        assert_eq!(Front::new().draw(), "---\n---\n\n");
    }

    #[test]
    fn a_value_cannot_leave_its_line() {
        assert_eq!(yaml_scalar("Bridge Crew"), "\"Bridge Crew\"");
        assert_eq!(
            yaml_scalar("a: b\n---\ntitle: \"x\" \\ #c"),
            "\"a: b\\n---\\ntitle: \\\"x\\\" \\\\ #c\""
        );
        assert_eq!(yaml_scalar(""), "\"\"");
        assert!(!yaml_scalar("\r\u{0}\t").contains(['\r', '\n', '\0']));
    }
}
