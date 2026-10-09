//! Strip / rewrite the Unicode private-use-area sentinels ChatGPT
//! embeds in assistant message text. The OpenAI client wraps inline
//! links, file citations, web search results, etc. in
//! `U+E200 … U+E201` regions whose arguments are separated by
//! `U+E202`. The plain-text export drops the wrappers but leaves the
//! arguments concatenated — e.g. `urlOpenAIhttps://openai.com`
//! becomes `urlOpenAIhttps://openai.com` in the rendered markdown,
//! which is both ugly and breaks the link.

use datalib_etl_render::html::md_link_dest;

const START: char = '\u{e200}';
const END: char = '\u{e201}';
const SEP: char = '\u{e202}';

/// ChatGPT's private-use block. `U+E203`/`U+E204` bracket a cited span
/// and `U+E206` trails it; whatever the wrappers above did not consume
/// would show as a tofu box, so every character left in the block goes.
fn is_sentinel(c: char) -> bool {
    ('\u{e200}'..='\u{e2ff}').contains(&c)
}

pub fn clean_text(s: &str) -> String {
    if !s.chars().any(is_sentinel) {
        return s.to_string();
    }
    expand_wrappers(s)
        .chars()
        .filter(|&c| !is_sentinel(c))
        .collect()
}

fn expand_wrappers(s: &str) -> String {
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
            // The text is the model's, so its emphasis stays; only a
            // bracket, which would end the link early, is escaped.
            out.push('[');
            out.push_str(&text.replace('[', "\\[").replace(']', "\\]"));
            out.push_str("](");
            out.push_str(&md_link_dest(href));
            out.push(')');
        } else if parts.len() == 2 && parts[0] == "entity" {
            // `["movie","<name>","<disambiguation>"]`: the name is what
            // the sentence reads.
            out.push_str(&entity_name(parts[1]));
        }
        // Other sentinel kinds (filecite, cite, search, …): drop.
    }
    out
}

fn entity_name(args: &str) -> String {
    let parsed: Option<Vec<serde_json::Value>> = serde_json::from_str(args).ok();
    parsed
        .as_deref()
        .and_then(|a| a.get(1).or_else(|| a.first()))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| args.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_url_becomes_markdown_link() {
        let raw = "test for \u{e200}url\u{e202}OpenAI\u{e202}https://openai.com\u{e201}.";
        assert_eq!(clean_text(raw), "test for [OpenAI](https://openai.com).");
    }

    /// A link's text cannot end the link early, nor its target.
    #[test]
    fn a_link_in_brackets_stays_one_link() {
        let raw = "\u{e200}url\u{e202}[1] Ops\u{e202}https://e.invalid/a (b)\u{e201}";
        assert_eq!(clean_text(raw), "[\\[1\\] Ops](<https://e.invalid/a (b)>)");
    }

    #[test]
    fn filecite_is_stripped() {
        let raw = "Hello.\n\n\u{e200}filecite\u{e202}turn0file0\u{e202}L1-L2\u{e201}";
        assert_eq!(clean_text(raw), "Hello.\n\n");
    }

    /// A named entity is part of the sentence; dropping it left
    /// "mentioning ." where the model named a film.
    #[test]
    fn an_entity_reads_as_its_name() {
        let raw =
            "about \u{e200}entity\u{e202}[\"starship\",\"USS Enterprise\",\"NCC-1701-D\"]\u{e201}.";
        assert_eq!(clean_text(raw), "about USS Enterprise.");
        let odd = "\u{e200}entity\u{e202}not json\u{e201}";
        assert_eq!(clean_text(odd), "not json");
    }

    #[test]
    fn passthrough_when_no_sentinels() {
        assert_eq!(clean_text("plain text"), "plain text");
    }

    #[test]
    fn unterminated_sentinel_keeps_inner_text() {
        let raw = "trailing \u{e200}url\u{e202}stuff";
        assert_eq!(clean_text(raw), "trailing urlstuff");
    }

    #[test]
    fn multiple_sentinels_in_one_string() {
        let raw = "\u{e200}url\u{e202}A\u{e202}https://a\u{e201} and \u{e200}url\u{e202}B\u{e202}https://b\u{e201}.";
        assert_eq!(clean_text(raw), "[A](https://a) and [B](https://b).");
    }

    /// A cited span leaves `U+E203`/`U+E204`/`U+E206` behind, which
    /// showed as tofu boxes after every sentence.
    #[test]
    fn stray_span_markers_are_removed() {
        let raw = "\u{e200}i\u{e202}turn0image0\u{e202}turn0image1\u{e201}Hi.\u{e203}\u{e204} Next.\u{e203} \u{e204}\u{e206}";
        assert_eq!(clean_text(raw), "Hi. Next. ");
    }
}
