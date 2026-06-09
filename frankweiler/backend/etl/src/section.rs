//! Shared "section anchor" helpers for provider renderers.
//!
//! Every grid row this codebase produces points at a `data-section-uuid`
//! in the rendered markdown. The frontend keys selection,
//! scroll-to-on-row-click, and copy-uuid off those attributes; if the
//! attribute is missing the row looks like data loss (the chat-preview
//! pane has no anchor to navigate to). Caught in production once for
//! signal — see commit `d828394` and
//! `row-click-scroll-position.spec.ts`.
//!
//! Two granularities ship here:
//!
//! * [`section_attrs`] — the bare attribute fragment
//!   `id="m-{uuid}" data-section-uuid="{uuid}"`. Drop into any element
//!   the renderer wants. Use this when the section needs to ride inside
//!   a markdown bullet or other inline shape (signal's per-item
//!   `<span>`, beeper's per-reaction `<span>`).
//! * [`msg_div_open`] / [`MSG_DIV_CLOSE`] — full `<div>` wrapper with
//!   the standard `class="msg msg--{provider}"` tag. Use this when each
//!   message renders as its own "card" block (chatgpt, anthropic, slack,
//!   beeper messages).
//!
//! The `data-section-uuid` MUST be byte-equal to the matching
//! `grid_row.uuid` so the row→preview navigation can resolve. Both
//! providers and the grid_rows builder compute the same UUID from the
//! same upstream identifiers — see each provider's `*_message_uuid()`
//! helper.

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
pub fn msg_div_open(msg_uuid: &str, provider: &str) -> String {
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
            msg_div_open("abc", "slack"),
            r#"<div id="m-abc" data-section-uuid="abc" class="msg msg--slack">"#
        );
    }
}
