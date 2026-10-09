//! An email's @-mentions: what Gmail and Outlook write when a person is
//! picked after `@` (Gmail once used `+`) in the composer, a `mailto:` link
//! whose text begins with the sign. Read from the markdown the body
//! becomes, where such a link is `[@Will Riker](mailto:riker@…)`.

use std::sync::LazyLock;

use datalib_etl_render::message::chip_link;
use datalib_handle::Handle;
use regex::{Captures, Regex};

static MENTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\[([@+][^\]\n]+)\]\(<?mailto:([^)>\s?"]+)[^)\n]*\)"#).unwrap());

/// `markdown` with each mention drawn as a person chip, and the people it
/// mentions, each once, in the order first mentioned. A link whose
/// address makes no handle is left as it was.
pub fn mention_chips(markdown: &str) -> (String, Vec<Handle>) {
    let mut mentioned: Vec<Handle> = Vec::new();
    let drawn = MENTION
        .replace_all(markdown, |caps: &Captures<'_>| {
            let Some(handle) = Handle::email(&caps[2]) else {
                return caps[0].to_string();
            };
            let chip = chip_link(&caps[1], &handle);
            if !mentioned.contains(&handle) {
                mentioned.push(handle);
            }
            chip
        })
        .into_owned();
    (drawn, mentioned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(markdown: &str) -> Vec<String> {
        mention_chips(markdown)
            .1
            .iter()
            .map(|h| h.as_str().to_string())
            .collect()
    }

    /// The links Outlook and Gmail write for a mention, as the body's
    /// HTML becomes markdown, are the people mentioned; a mailto link a
    /// person typed (no `@`) is not a mention.
    #[test]
    fn a_mailto_link_that_begins_with_at_is_a_mention() {
        let html = "<p>Hi <a href=\"mailto:Riker@Enterprise.example\" id=\"OWAAM1\">@Will Riker</a>, \
                    <a class=\"gmail_plusreply\" href=\"mailto:troi@enterprise.example\">+Deanna</a> and \
                    <a href=\"mailto:riker@enterprise.example\">@Will</a>; write to \
                    <a href=\"mailto:crusher@enterprise.example\">sickbay</a>.</p>";
        let markdown = crate::render::render::html_markdown(html);
        assert_eq!(
            named(&markdown),
            [
                "email:riker@enterprise.example",
                "email:troi@enterprise.example"
            ],
            "{markdown}"
        );
        let (drawn, _) = mention_chips(&markdown);
        assert_eq!(
            drawn.trim(),
            "Hi [@Will Riker](mailto:riker@enterprise.example \"@Will Riker <riker@enterprise.example>\"), \
             [+Deanna](mailto:troi@enterprise.example \"+Deanna <troi@enterprise.example>\") and \
             [@Will](mailto:riker@enterprise.example \"@Will <riker@enterprise.example>\"); write to \
             [sickbay](mailto:crusher@enterprise.example)."
        );
    }

    #[test]
    fn an_address_that_makes_no_handle_stays_a_link() {
        let (drawn, mentioned) = mention_chips("[@nobody](mailto:not-an-address)");
        assert!(mentioned.is_empty());
        assert_eq!(drawn, "[@nobody](mailto:not-an-address)");
    }
}
