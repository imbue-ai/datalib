//! Shared "section anchor" helpers for provider renderers, and the
//! [`Section`] a renderer hands over so a document can be read piece by
//! piece without parsing its markdown.

use datalib_schema::providers::Provider;

/// One piece of a rendered document, in document order. `uuid` is the
/// piece's `data-section-uuid` — a message, a contact — so a row of the
/// grid names the piece it came from; `None` for a piece that belongs
/// to no row: the frontmatter and title, a `<details>` opener around a
/// run of asides. Concatenated in order, a document's sections are its
/// `.md` byte for byte ([`join`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub uuid: Option<String>,
    pub md: String,
}

impl Section {
    pub fn keyed(uuid: &str, md: String) -> Self {
        Self {
            uuid: Some(uuid.to_string()),
            md,
        }
    }

    pub fn unkeyed(md: String) -> Self {
        Self { uuid: None, md }
    }
}

/// The `.md` a list of sections spells.
pub fn join(sections: &[Section]) -> String {
    let mut out = String::with_capacity(sections.iter().map(|s| s.md.len()).sum());
    for s in sections {
        out.push_str(&s.md);
    }
    out
}

/// HTML attribute fragment that anchors a navigable section.
/// Identical shape across providers: `id="m-{uuid}"` for in-page
/// `#anchor` links and `data-section-uuid="{uuid}"` for the frontend's
/// row→preview lookup.
pub fn section_attrs(uuid: &str) -> String {
    format!(r#"id="m-{uuid}" data-section-uuid="{uuid}""#)
}

/// Opening tag for a per-message wrapper div. Pair with
/// [`MSG_DIV_CLOSE`]. `provider` tags the element so per-provider CSS
/// (avatar, accent color, etc.) can apply without each renderer
/// inventing its own class scheme.
pub fn msg_div_open(msg_uuid: &str, provider: Provider) -> String {
    format!(
        r#"<div {attrs} class="msg msg--{provider}">"#,
        attrs = section_attrs(msg_uuid),
    )
}

/// Closes a div opened with [`msg_div_open`]. A constant rather than
/// a function so renderers can hard-code it in `const`-evaluable
/// contexts; the close tag has no per-message state to thread in.
pub const MSG_DIV_CLOSE: &str = "</div>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joined_sections_are_the_document() {
        let sections = vec![
            Section::unkeyed("---\n---\n\n".into()),
            Section::keyed("a", "<div>a</div>\n".into()),
            Section::keyed("b", "<div>b</div>\n".into()),
        ];
        assert_eq!(join(&sections), "---\n---\n\n<div>a</div>\n<div>b</div>\n");
        assert_eq!(sections[0].uuid, None);
        assert_eq!(sections[1].uuid.as_deref(), Some("a"));
    }

    #[test]
    fn attrs_shape() {
        assert_eq!(
            section_attrs("abc-123"),
            r#"id="m-abc-123" data-section-uuid="abc-123""#
        );
    }

    #[test]
    fn div_open_shape() {
        assert_eq!(
            msg_div_open("abc", Provider::Slack),
            r#"<div id="m-abc" data-section-uuid="abc" class="msg msg--slack">"#
        );
    }
}
