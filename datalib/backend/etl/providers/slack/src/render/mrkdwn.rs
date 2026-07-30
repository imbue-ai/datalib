//! Slack mrkdwn → CommonMark converter. Port of `src/ingest/providers/
//! slack/mrkdwn.py`.
//!
//! Slack's "mrkdwn" overlaps with but diverges from CommonMark in
//! several places:
//!
//!   - `*bold*` (single asterisk) instead of `**bold**`
//!   - `~strike~` (single tilde) instead of `~~strike~~`
//!   - `<https://url|label>` instead of `[label](https://url)`
//!   - `<@U…>` / `<#C…|name>` / `<!here>` / `<!subteam^S…|name>` mentions
//!   - `&amp;` / `&lt;` / `&gt;` HTML-escapes in the message body
//!
//! Rendered `.md` is served verbatim to the UI's CommonMark renderer, so
//! translation happens at render time. The regex pipeline is exercised
//! by snapshot tests sitting alongside this module.

use std::collections::BTreeMap;

use once_cell::sync::Lazy;
use regex::{Captures, Regex};

static USER_REF: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<@([UW][A-Z0-9_]+)(?:\|([^>]+))?>").unwrap());
static CHANNEL_REF: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<#([CG][A-Z0-9_]+)(?:\|([^>]+))?>").unwrap());
static SUBTEAM_REF: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<!subteam\^[A-Z0-9_]+(?:\|([^>]+))?>").unwrap());
static SPECIAL_REF: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<!(here|channel|everyone)(?:\|[^>]+)?>").unwrap());
static URL_REF: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<((?:https?|mailto):[^|>\s]+)(?:\|([^>]*))?>").unwrap());
// Bold/strike use look-around-style boundary checks but Rust's `regex`
// crate has no look-around. We compensate by including the boundary
// char in the match and re-emitting it. See [`reflow_bold`] / [`reflow_strike`].
static BOLD: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(^|[^\w*])\*([^\s*][^*\n]*?[^\s*]|[^\s*])\*([^\w*]|$)").unwrap());
static STRIKE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(^|[^\w~])~([^\s~][^~\n]*?[^\s~]|[^\s~])~([^\w~]|$)").unwrap());
static SHORTCODE: Lazy<Regex> = Lazy::new(|| Regex::new(r":([a-zA-Z0-9_+\-]+):").unwrap());

/// Replace `:shortcode:` with the matching Unicode emoji. Unknown
/// shortcodes pass through untouched, matching the Python `emoji` lib's
/// `language="alias"` behavior.
pub fn emojize_shortcodes(text: &str) -> String {
    SHORTCODE
        .replace_all(text, |caps: &Captures<'_>| {
            match emojis::get_by_shortcode(&caps[1]) {
                Some(e) => e.as_str().to_string(),
                None => caps[0].to_string(),
            }
        })
        .into_owned()
}

/// Replace `<@U…>` / `<@U…|label>` with `@<label>` only. Plain-text
/// path — see `to_commonmark` for the full pipeline.
pub fn resolve_user_mentions(text: &str, user_labels: &BTreeMap<String, String>) -> String {
    let replaced = USER_REF
        .replace_all(text, |caps: &Captures<'_>| {
            user_replacement(caps, user_labels)
        })
        .into_owned();
    emojize_shortcodes(&replaced)
}

fn user_replacement(caps: &Captures<'_>, user_labels: &BTreeMap<String, String>) -> String {
    let uid = &caps[1];
    let label = caps.get(2).map(|m| m.as_str().to_string());
    let resolved = label
        .or_else(|| user_labels.get(uid).cloned())
        .unwrap_or_else(|| uid.to_string());
    format!("@{resolved}")
}

/// Render Slack mrkdwn `text` into CommonMark. `user_labels` maps
/// `user_id` → display label (real name); missing ids fall back to
/// `@U…`. Channel labels come from the source string itself
/// (`<#C…|name>`), so no lookup needed.
pub fn to_commonmark(text: &str, user_labels: &BTreeMap<String, String>) -> String {
    let mut out = text.to_string();

    out = USER_REF
        .replace_all(&out, |caps: &Captures<'_>| {
            user_replacement(caps, user_labels)
        })
        .into_owned();

    out = CHANNEL_REF
        .replace_all(&out, |caps: &Captures<'_>| {
            let cid = &caps[1];
            let name = caps.get(2).map(|m| m.as_str()).unwrap_or(cid);
            format!("#{name}")
        })
        .into_owned();

    out = SUBTEAM_REF
        .replace_all(&out, |caps: &Captures<'_>| {
            let name = caps.get(1).map(|m| m.as_str()).unwrap_or("group");
            format!("@{name}")
        })
        .into_owned();

    out = SPECIAL_REF
        .replace_all(&out, |caps: &Captures<'_>| format!("@{}", &caps[1]))
        .into_owned();

    out = URL_REF
        .replace_all(&out, |caps: &Captures<'_>| {
            let url = &caps[1];
            match caps.get(2) {
                Some(label) if !label.as_str().is_empty() && label.as_str() != url => {
                    format!("[{}]({})", label.as_str(), url)
                }
                _ => format!("<{url}>"),
            }
        })
        .into_owned();

    out = BOLD
        .replace_all(&out, |caps: &Captures<'_>| {
            format!("{}**{}**{}", &caps[1], &caps[2], &caps[3])
        })
        .into_owned();

    out = STRIKE
        .replace_all(&out, |caps: &Captures<'_>| {
            format!("{}~~{}~~{}", &caps[1], &caps[2], &caps[3])
        })
        .into_owned();

    // Slack escapes only these three entities in message text (per the
    // Formatting reference). Decode after the angle-bracket constructs
    // are consumed so we never accidentally synthesise a `<@U…>` from
    // text the user actually typed.
    out = out
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");

    out = terminate_blockquotes(&out);
    emojize_shortcodes(&out)
}

/// Slack `>` quotes only the prefixed line(s); CommonMark would lazily
/// absorb the following non-blank line into the same blockquote.
/// Inject a blank line after each `>`-block so the quote ends where
/// Slack ends it.
fn terminate_blockquotes(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        out.push((*line).to_string());
        if !line.starts_with('>') {
            continue;
        }
        let nxt = lines.get(i + 1).copied().unwrap_or("");
        if !nxt.is_empty() && !nxt.starts_with('>') {
            out.push(String::new());
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("U_PICARD".to_string(), "Jean-Luc Picard".to_string());
        m.insert("U_DATA".to_string(), "Lt. Cmdr. Data".to_string());
        m
    }

    #[test]
    fn bold_strike_user_url() {
        let lbl = labels();
        assert_eq!(to_commonmark("hello *world*", &lbl), "hello **world**");
        assert_eq!(to_commonmark("~old~ news", &lbl), "~~old~~ news");
        assert_eq!(
            to_commonmark("hi <@U_PICARD>!", &lbl),
            "hi @Jean-Luc Picard!"
        );
        assert_eq!(
            to_commonmark("<https://slack.com|Slack>", &lbl),
            "[Slack](https://slack.com)"
        );
        assert_eq!(
            to_commonmark("<https://slack.com>", &lbl),
            "<https://slack.com>"
        );
    }

    #[test]
    fn channel_subteam_special() {
        let lbl = BTreeMap::new();
        assert_eq!(to_commonmark("see <#C_BRIDGE|bridge>", &lbl), "see #bridge");
        assert_eq!(
            to_commonmark("<!subteam^S_OPS|ops-team> deploy", &lbl),
            "@ops-team deploy"
        );
        assert_eq!(to_commonmark("<!here> heads up", &lbl), "@here heads up");
    }

    #[test]
    fn html_entities_and_emoji() {
        let lbl = BTreeMap::new();
        assert_eq!(
            to_commonmark("a &amp; b &lt;3 &gt;_&lt;", &lbl),
            "a & b <3 >_<"
        );
        assert!(to_commonmark(":thumbsup:", &lbl).contains('👍'));
        // Unknown shortcode passes through.
        assert_eq!(to_commonmark(":notarealemoji:", &lbl), ":notarealemoji:");
    }

    #[test]
    fn blockquote_terminator() {
        let lbl = BTreeMap::new();
        let out = to_commonmark("> quoted\nplain follow", &lbl);
        assert_eq!(out, "> quoted\n\nplain follow");
    }
}
