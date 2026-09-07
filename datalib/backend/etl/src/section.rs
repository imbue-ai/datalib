//! Shared "section anchor" helpers for provider renderers.

use datalib_schema::providers::Provider;

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
