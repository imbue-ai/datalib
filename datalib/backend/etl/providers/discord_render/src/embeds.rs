//! What a message shows besides its text and its files: link previews
//! and bots' cards (embeds), a forwarded message, stickers, a poll, a
//! bot's laid-out text. Drawn as markdown, with no remote picture.

use serde_json::Value;

use datalib_etl_render::html::{escape_md_inline, md_link_dest, md_quote, safe_url};

use crate::markdown::{to_commonmark, Labels};

/// A link preview's description longer than this is left out: the page it
/// describes is a click away.
const PREVIEW_TEXT_MAX: usize = 300;

/// A component that is text, and one that holds others.
const TEXT_DISPLAY: i64 = 10;

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn list<'a>(v: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    v.get(key).and_then(Value::as_array).into_iter().flatten()
}

/// Each part as a block of markdown, in order.
pub fn draw(message: &Value, labels: Labels<'_>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for snapshot in list(message, "message_snapshots") {
        if let Some(forwarded) = snapshot.get("message") {
            out.push(forward(forwarded, labels));
        }
    }
    let said: Vec<String> = list(message, "components")
        .flat_map(|c| laid_out_text(c, labels))
        .collect();
    if !said.is_empty() {
        out.push(said.join("\n\n"));
    }
    let content = text(message, "content").unwrap_or("");
    out.extend(list(message, "embeds").filter_map(|e| embed(e, content, labels)));
    for sticker in list(message, "sticker_items") {
        let name = text(sticker, "name").unwrap_or("sticker");
        out.push(format!("Sticker: {}", escape_md_inline(name)));
    }
    if let Some(poll) = message.get("poll").filter(|p| p.is_object()) {
        out.push(poll_of(poll));
    }
    out
}

/// The text of a bot's laid-out message, wherever it sits in the layout.
fn laid_out_text(component: &Value, labels: Labels<'_>) -> Vec<String> {
    let mut out = Vec::new();
    if component.get("type").and_then(Value::as_i64) == Some(TEXT_DISPLAY) {
        out.extend(text(component, "content").map(|t| to_commonmark(t, labels)));
    }
    for key in ["components", "accessory"] {
        match component.get(key) {
            Some(Value::Array(inner)) => {
                out.extend(inner.iter().flat_map(|c| laid_out_text(c, labels)));
            }
            Some(inner @ Value::Object(_)) => out.extend(laid_out_text(inner, labels)),
            _ => {}
        }
    }
    out
}

fn forward(message: &Value, labels: Labels<'_>) -> String {
    let mut parts = vec!["Forwarded:".to_string()];
    parts.extend(text(message, "content").map(|t| to_commonmark(t, labels)));
    parts.extend(draw(message, labels));
    md_quote(&parts.join("\n\n"))
}

/// One embed, set off as a quote. `None` for one that says no more than
/// the link in the message's text does.
fn embed(e: &Value, content: &str, labels: Labels<'_>) -> Option<String> {
    let url = text(e, "url").and_then(safe_url);
    let rich = text(e, "type") == Some("rich");
    let mut head: Vec<String> = Vec::new();
    head.extend(
        e.get("provider")
            .and_then(|p| text(p, "name"))
            .map(escape_md_inline),
    );
    head.extend(
        e.get("author")
            .and_then(|a| text(a, "name"))
            .map(escape_md_inline),
    );
    if let Some(title) = text(e, "title") {
        let title = escape_md_inline(title);
        head.push(match url {
            Some(url) => format!("**[{title}]({})**", md_link_dest(url)),
            None => format!("**{title}**"),
        });
    } else if let Some(url) = url.filter(|u| !content.contains(u)) {
        head.push(format!("<{}>", url.replace('<', "%3C").replace('>', "%3E")));
    }
    let mut parts: Vec<String> = Vec::new();
    if !head.is_empty() {
        parts.push(head.join(" · "));
    }
    let description =
        text(e, "description").filter(|d| rich || d.chars().count() <= PREVIEW_TEXT_MAX);
    parts.extend(description.map(|d| to_commonmark(d, labels)));
    let fields: Vec<String> = list(e, "fields")
        .map(|f| {
            let name = escape_md_inline(text(f, "name").unwrap_or(""));
            let value = text(f, "value").map(|v| to_commonmark(v, labels));
            let value = value.unwrap_or_default().replace('\n', "\n  ");
            format!("- **{name}** {value}").trim_end().to_string()
        })
        .collect();
    if !fields.is_empty() {
        parts.push(fields.join("\n"));
    }
    parts.extend(
        e.get("footer")
            .and_then(|f| text(f, "text"))
            .map(escape_md_inline),
    );
    (!parts.is_empty()).then(|| md_quote(&parts.join("\n\n")))
}

fn poll_of(poll: &Value) -> String {
    let question = poll.get("question").and_then(|q| text(q, "text"));
    let mut out = format!("Poll: {}", escape_md_inline(question.unwrap_or("")));
    let counts: Vec<(i64, i64)> = poll
        .get("results")
        .into_iter()
        .flat_map(|r| list(r, "answer_counts"))
        .filter_map(|c| Some((c.get("id")?.as_i64()?, c.get("count")?.as_i64()?)))
        .collect();
    for answer in list(poll, "answers") {
        let id = answer.get("answer_id").and_then(Value::as_i64);
        let said = answer.get("poll_media").and_then(|m| text(m, "text"));
        let count = counts.iter().find(|(a, _)| Some(*a) == id).map(|(_, n)| *n);
        let count = count.map(|n| format!(" ({n})")).unwrap_or_default();
        out.push_str(&format!(
            "\n- {}{count}",
            escape_md_inline(said.unwrap_or(""))
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_render::inputs::Inputs;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn drawn(message: Value) -> Vec<String> {
        let inputs = Inputs::default();
        let none = BTreeMap::new();
        let lookup = inputs.lookup("users", &none);
        let labels = Labels {
            users: lookup,
            channels: lookup,
            roles: lookup,
        };
        draw(&message, labels)
    }

    #[test]
    fn a_link_preview_names_its_page_and_draws_no_picture() {
        let got = drawn(json!({"content": "https://a.test/warp", "embeds": [{
            "type": "article", "url": "https://a.test/warp", "title": "Warp *theory*",
            "description": "Faster.", "provider": {"name": "Starfleet"},
            "thumbnail": {"url": "https://evil.test/track.png"}}]}));
        assert_eq!(
            got,
            ["> Starfleet · **[Warp \\*theory\\*](https://a.test/warp)**\n>\n> Faster."]
        );
    }

    /// A preview with nothing but the link already in the text says
    /// nothing more.
    #[test]
    fn a_bare_preview_is_left_out() {
        let got = drawn(json!({"content": "see https://a.test/x.png", "embeds": [
            {"type": "image", "url": "https://a.test/x.png"}]}));
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn a_bots_card_keeps_its_fields_and_footer() {
        let got = drawn(json!({"content": "", "embeds": [{
            "type": "rich", "title": "Red alert", "url": "javascript:alert(1)",
            "description": "x".repeat(400),
            "fields": [{"name": "Deck", "value": "**7**\nport side"}],
            "footer": {"text": "Ship's computer"}}]}));
        assert!(got[0].starts_with("> **Red alert**\n"), "{got:?}");
        assert!(got[0].contains("> - **Deck** **7**"), "{got:?}");
        assert!(got[0].ends_with("> Ship's computer"), "{got:?}");
    }

    #[test]
    fn a_forward_is_quoted_whole() {
        let got = drawn(json!({"content": "", "message_snapshots": [{"message": {
            "content": "Tea. Earl Grey.\nHot.", "embeds": [], "attachments": []}}]}));
        assert_eq!(got, ["> Forwarded:\n>\n> Tea. Earl Grey.  \n> Hot."]);
    }

    #[test]
    fn a_poll_a_sticker_and_laid_out_text_are_words() {
        let got = drawn(json!({"content": "",
            "sticker_items": [{"id": "1", "name": "Salute", "format_type": 1}],
            "components": [{"type": 17, "components": [{"type": 10, "content": "All hands."}]}],
            "poll": {"question": {"text": "Poker?"},
                "answers": [{"answer_id": 1, "poll_media": {"text": "Yes"}},
                            {"answer_id": 2, "poll_media": {"text": "No"}}],
                "results": {"answer_counts": [{"id": 1, "count": 4}]}}}));
        assert_eq!(
            got,
            [
                "All hands.",
                "Sticker: Salute",
                "Poll: Poker?\n- Yes (4)\n- No"
            ]
        );
    }
}
