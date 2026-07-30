//! Strip / rewrite the Unicode private-use-area sentinels ChatGPT
//! embeds in assistant message text. The OpenAI client wraps inline
//! links, file citations, web search results, etc. in
//! `U+E200 … U+E201` regions whose arguments are separated by
//! `U+E202`. The plain-text export drops the wrappers but leaves the
//! arguments concatenated — e.g. `urlOpenAIhttps://openai.com`
//! becomes `urlOpenAIhttps://openai.com` in the rendered markdown,
//! which is both ugly and breaks the link.
//!
//! We can't recover every sentinel kind faithfully (their schemas are
//! undocumented and shift over time), so the rule here is:
//!
//! * `url<text><href>` → `[text](href)` (the one
//!   shape we see consistently and can render correctly).
//! * Everything else (`filecite`, `cite`, `search`, `nav`, …) is
//!   stripped silently. Better an absence than a garbled glob of
//!   identifiers leaking into the prose.

const START: char = '\u{e200}';
const END: char = '\u{e201}';
const SEP: char = '\u{e202}';

pub fn clean_text(s: &str) -> String {
    if !s.contains(START) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != START {
            out.push(c);
            continue;
        }
        let mut inner = String::new();
        let mut closed = false;
        for c2 in chars.by_ref() {
            if c2 == END {
                closed = true;
                break;
            }
            inner.push(c2);
        }
        if !closed {
            // Unterminated region — drop the opener but keep the inner
            // text so partial data isn't silently lost.
            out.push_str(&inner);
            continue;
        }
        let parts: Vec<&str> = inner.split(SEP).collect();
        if parts.len() == 3 && parts[0] == "url" {
            let text = parts[1];
            let href = parts[2];
            out.push('[');
            out.push_str(text);
            out.push_str("](");
            out.push_str(href);
            out.push(')');
        }
        // Other sentinel kinds (filecite, cite, search, …): drop.
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_url_becomes_markdown_link() {
        let raw = "test for \u{e200}url\u{e202}OpenAI\u{e202}https://openai.com\u{e201}.";
        assert_eq!(clean_text(raw), "test for [OpenAI](https://openai.com).");
    }

    #[test]
    fn filecite_is_stripped() {
        let raw = "Hello.\n\n\u{e200}filecite\u{e202}turn0file0\u{e202}L1-L2\u{e201}";
        assert_eq!(clean_text(raw), "Hello.\n\n");
    }

    #[test]
    fn passthrough_when_no_sentinels() {
        assert_eq!(clean_text("plain text"), "plain text");
    }

    #[test]
    fn unterminated_sentinel_keeps_inner_text() {
        let raw = "trailing \u{e200}url\u{e202}stuff";
        assert_eq!(clean_text(raw), "trailing url\u{e202}stuff");
    }

    #[test]
    fn multiple_sentinels_in_one_string() {
        let raw = "\u{e200}url\u{e202}A\u{e202}https://a\u{e201} and \u{e200}url\u{e202}B\u{e202}https://b\u{e201}.";
        assert_eq!(clean_text(raw), "[A](https://a) and [B](https://b).");
    }
}
