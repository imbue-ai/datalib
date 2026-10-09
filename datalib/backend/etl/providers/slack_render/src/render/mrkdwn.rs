//! Slack mrkdwn → CommonMark converter. Port of `src/ingest/providers/
//! slack/mrkdwn.py`.

use std::collections::BTreeMap;

use datalib_etl_render::html::{code_span_parts, escape_md_inline, escape_md_syntax, md_link_dest};
use datalib_etl_render::inputs::Lookup;
use datalib_etl_render::message::chip_link;
use datalib_handle::Handle;
use once_cell::sync::Lazy;
use regex::{Captures, Regex};

static USER_REF: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<@([UW][A-Z0-9_]+)(?:\|([^>]+))?>").unwrap());
// The label may be empty (`<#C123|>`): Slack emits that for a channel
// the message's author can see but the reader may not.
static CHANNEL_REF: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<#([CG][A-Z0-9_]+)(?:\|([^>]*))?>").unwrap());
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

/// What a mention resolves to: `user_id` → real name, `channel_id` →
/// channel name. An id with no entry falls back to the id itself
/// (`@U…`, `#C…`), so a mention is never silently dropped. Both are
/// recording lookups: every id asked for is declared as an input of
/// the thread being rendered.
#[derive(Clone, Copy)]
pub struct Labels<'a> {
    pub users: Lookup<'a, BTreeMap<String, String>>,
    pub channels: Lookup<'a, BTreeMap<String, String>>,
    /// The workspace the text is from: a user mention's handle is
    /// scoped to it.
    pub team_id: &'a str,
}

/// Mentions and emoji only, as plain text — what a thread title needs,
/// without the rest of the CommonMark conversion.
pub fn resolve_mentions(text: &str, labels: Labels<'_>) -> String {
    let replaced = USER_REF
        .replace_all(text, |caps: &Captures<'_>| {
            format!("@{}", user_label(caps, labels, slack_encode))
        })
        .into_owned();
    let replaced = CHANNEL_REF
        .replace_all(&replaced, |caps: &Captures<'_>| {
            format!("#{}", channel_label(caps, labels, slack_encode))
        })
        .into_owned();
    decode_entities(&emojize_shortcodes(&replaced))
}

/// The people a message's text mentions (`<@U…>`), each once, in the
/// order first mentioned, as handles in `team_id`'s workspace.
pub fn mentioned_users(text: &str, team_id: &str) -> Vec<Handle> {
    let mut out: Vec<Handle> = Vec::new();
    for caps in USER_REF.captures_iter(text) {
        if let Some(handle) = Handle::slack(team_id, &caps[1]) {
            if !out.contains(&handle) {
                out.push(handle);
            }
        }
    }
    out
}

/// The three entities Slack escapes in message text (per its
/// Formatting reference), back to the characters typed.
fn decode_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// Plain text as Slack writes it in a message: [`decode_entities`]'s
/// inverse.
fn slack_encode(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Who a `<@U…>` names, through `name`: a label inside the mention is
/// message text, entities and all, and is decoded before `name` sees it;
/// a name from the users table is plain already.
fn user_label(caps: &Captures<'_>, labels: Labels<'_>, name: fn(&str) -> String) -> String {
    let uid = &caps[1];
    match caps.get(2) {
        Some(label) => name(&decode_entities(label.as_str())),
        None => labels
            .users
            .get(uid)
            .map_or_else(|| uid.to_string(), |n| name(n)),
    }
}

/// A `<@U…>` in a body: a chip link naming the user (`chip_link`), so
/// the viewer draws who they are; `@Name` as plain text where the id
/// makes no handle. The name is what the message showed, as
/// [`user_label`] reads it.
fn user_mention(caps: &Captures<'_>, labels: Labels<'_>) -> String {
    let shown = format!(
        "@{}",
        emojize_shortcodes(&user_label(caps, labels, str::to_string))
    );
    match Handle::slack(labels.team_id, &caps[1]) {
        Some(handle) => chip_link(&shown, &handle),
        None => escape_md_inline(&shown),
    }
}

fn channel_label(caps: &Captures<'_>, labels: Labels<'_>, name: fn(&str) -> String) -> String {
    let cid = &caps[1];
    match caps.get(2).map(|m| m.as_str()).filter(|l| !l.is_empty()) {
        Some(label) => name(&decode_entities(label)),
        None => labels
            .channels
            .get(cid)
            .map_or_else(|| cid.to_string(), |n| name(n)),
    }
}

/// A person's or a channel's name on a markdown line, emoji shortcodes
/// and all.
fn md_name(name: &str) -> String {
    escape_md_inline(&emojize_shortcodes(name))
}

/// Render Slack mrkdwn `text` into CommonMark. A `<#C…|name>` carries
/// its own label; a bare `<#C…>` or `<#C…|>` is looked up in
/// `labels.channels`.
///
/// Markdown's own syntax in what someone typed — a `[x](url)`, a `# `,
/// a `|` — is escaped first, since Slack shows it as typed. A name
/// stands in as a placeholder until the end, so none of the passes that
/// read mrkdwn can read into it.
pub fn to_commonmark(text: &str, labels: Labels<'_>) -> String {
    let mut names: Vec<String> = Vec::new();
    let mut hold = |md: String| {
        names.push(md);
        format!("{NAME_OPEN}{}{NAME_CLOSE}", names.len() - 1)
    };

    let mut out = escape_typed_markdown(text);

    // A mention is a chip link in text, and `@Name` in code, which shows
    // what it holds literally.
    out = code_parts_whole_constructs(&out)
        .into_iter()
        .map(|(part, is_code)| {
            USER_REF
                .replace_all(&part, |caps: &Captures<'_>| {
                    if is_code {
                        format!("@{}", user_label(caps, labels, slack_encode))
                    } else {
                        hold(user_mention(caps, labels))
                    }
                })
                .into_owned()
        })
        .collect();

    out = CHANNEL_REF
        .replace_all(&out, |caps: &Captures<'_>| {
            hold(format!("#{}", channel_label(caps, labels, md_name)))
        })
        .into_owned();

    out = SUBTEAM_REF
        .replace_all(&out, |caps: &Captures<'_>| {
            let name = caps.get(1).map(|m| m.as_str()).unwrap_or("group");
            hold(format!("@{}", md_name(&decode_entities(name))))
        })
        .into_owned();

    out = SPECIAL_REF
        .replace_all(&out, |caps: &Captures<'_>| format!("@{}", &caps[1]))
        .into_owned();

    out = URL_REF
        .replace_all(&out, |caps: &Captures<'_>| {
            let url = decode_entities(&caps[1]);
            match caps.get(2).map(|m| m.as_str()) {
                Some(label) if !label.is_empty() && label != &caps[1] => {
                    format!(
                        "[{}]({})",
                        escape_md_syntax(&label.replace(['\r', '\n'], " "), false),
                        md_link_dest(&url)
                    )
                }
                _ => format!("<{}>", url.replace('<', "%3C").replace('>', "%3E")),
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

    // After the angle-bracket constructs are consumed, so we never
    // synthesise a `<@U…>` from text the user actually typed.
    out = decode_entities_where_literal(&out);

    out = terminate_blockquotes(&out);
    out = emojize_shortcodes(&out);
    NAME_HELD
        .replace_all(&out, |caps: &Captures<'_>| {
            caps[1]
                .parse::<usize>()
                .ok()
                .and_then(|n| names.get(n).cloned())
                .unwrap_or_default()
        })
        .into_owned()
}

/// Brackets a held name's index: private-use characters, which Slack
/// text has no business carrying.
const NAME_OPEN: char = '\u{E000}';
const NAME_CLOSE: char = '\u{E001}';
static NAME_HELD: Lazy<Regex> = Lazy::new(|| Regex::new("\u{E000}([0-9]+)\u{E001}").unwrap());

/// [`escape_md_syntax`] over what someone typed: not inside a `<…>`,
/// which is Slack's own markup (a typed `<` arrives as `&lt;`), and not
/// inside code, which shows literally anyway. The `<…>`s are held aside
/// while the code is found, so a backtick in a link's label cannot pair
/// with one outside it.
fn escape_typed_markdown(text: &str) -> String {
    let mut constructs: Vec<String> = Vec::new();
    let masked = SLACK_CONSTRUCT.replace_all(text, |caps: &Captures<'_>| {
        constructs.push(caps[0].to_string());
        format!("{CONSTRUCT_OPEN}{}{CONSTRUCT_CLOSE}", constructs.len() - 1)
    });
    let mut out = String::with_capacity(text.len() + 8);
    for (part, is_code) in code_span_parts(&masked) {
        if is_code {
            out.push_str(part);
        } else {
            let starts_line = out.is_empty() || out.ends_with('\n');
            // A `!` typed straight before a construct that becomes a
            // link would make that link an image.
            let escaped = escape_md_syntax(part, starts_line).replace(
                &format!("!{CONSTRUCT_OPEN}"),
                &format!("\\!{CONSTRUCT_OPEN}"),
            );
            out.push_str(&escaped);
        }
    }
    CONSTRUCT_HELD
        .replace_all(&out, |caps: &Captures<'_>| {
            caps[1]
                .parse::<usize>()
                .ok()
                .and_then(|n| constructs.get(n).cloned())
                .unwrap_or_default()
        })
        .into_owned()
}

/// [`code_span_parts`] over Slack text, with each `<…>` construct kept
/// whole inside one part: a mention's label may hold a backtick, which
/// must not open a code span that cuts the mention in two.
fn code_parts_whole_constructs(text: &str) -> Vec<(String, bool)> {
    let mut constructs: Vec<String> = Vec::new();
    let masked = SLACK_CONSTRUCT.replace_all(text, |caps: &Captures<'_>| {
        constructs.push(caps[0].to_string());
        format!("{CONSTRUCT_OPEN}{}{CONSTRUCT_CLOSE}", constructs.len() - 1)
    });
    code_span_parts(&masked)
        .into_iter()
        .map(|(part, is_code)| {
            let whole = CONSTRUCT_HELD.replace_all(part, |caps: &Captures<'_>| {
                caps[1]
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| constructs.get(n).cloned())
                    .unwrap_or_default()
            });
            (whole.into_owned(), is_code)
        })
        .collect()
}

static SLACK_CONSTRUCT: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^<>]*>").unwrap());
const CONSTRUCT_OPEN: char = '\u{E002}';
const CONSTRUCT_CLOSE: char = '\u{E003}';
static CONSTRUCT_HELD: Lazy<Regex> = Lazy::new(|| Regex::new("\u{E002}([0-9]+)\u{E003}").unwrap());

/// Slack's entities stay entities in running text, so a `<b>` someone
/// typed shows as typed rather than being read as HTML. They are decoded
/// where markdown would show them literally — inside a code span or
/// block — and where Slack's own `&gt;` quote mark opens a line.
fn decode_entities_where_literal(text: &str) -> String {
    let quoted: Vec<String> = text
        .split('\n')
        .map(|line| match line.strip_prefix("&gt;") {
            Some(rest) => format!(">{rest}"),
            None => line.to_string(),
        })
        .collect();
    code_span_parts(&quoted.join("\n"))
        .into_iter()
        .map(|(part, is_code)| {
            if is_code {
                decode_entities(part)
            } else {
                part.to_string()
            }
        })
        .collect()
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

    use datalib_etl_render::inputs::Inputs;
    use once_cell::sync::Lazy;

    static USERS: Lazy<BTreeMap<String, String>> = Lazy::new(|| {
        let mut m = BTreeMap::new();
        m.insert("U_PICARD".to_string(), "Jean-Luc Picard".to_string());
        m.insert("U_DATA".to_string(), "Lt. Cmdr. Data".to_string());
        m
    });
    static CHANNELS: Lazy<BTreeMap<String, String>> = Lazy::new(|| {
        let mut m = BTreeMap::new();
        m.insert("C_BRIDGE".to_string(), "bridge".to_string());
        m
    });
    static NONE: Lazy<BTreeMap<String, String>> = Lazy::new(BTreeMap::new);
    static INPUTS: Lazy<Inputs> = Lazy::new(Inputs::default);

    fn labels() -> Labels<'static> {
        Labels {
            users: INPUTS.lookup("users", &USERS),
            channels: INPUTS.lookup("channels", &CHANNELS),
            team_id: "T01",
        }
    }

    fn no_labels() -> Labels<'static> {
        Labels {
            users: INPUTS.lookup("users", &NONE),
            channels: INPUTS.lookup("channels", &NONE),
            team_id: "T01",
        }
    }

    #[test]
    fn bold_strike_user_url() {
        let lbl = labels();
        assert_eq!(to_commonmark("hello *world*", lbl), "hello **world**");
        assert_eq!(to_commonmark("~old~ news", lbl), "~~old~~ news");
        assert_eq!(
            to_commonmark("hi <@U_PICARD>!", lbl),
            "hi [@Jean-Luc Picard](slack://user?team=T01&id=U_PICARD \
             \"@Jean-Luc Picard (slack:T01/U_PICARD)\")!"
        );
        assert_eq!(
            to_commonmark("<https://slack.com|Slack>", lbl),
            "[Slack](https://slack.com)"
        );
        assert_eq!(
            to_commonmark("<https://slack.com>", lbl),
            "<https://slack.com>"
        );
    }

    #[test]
    fn channel_subteam_special() {
        let lbl = no_labels();
        assert_eq!(to_commonmark("see <#C_BRIDGE|bridge>", lbl), "see #bridge");
        assert_eq!(
            to_commonmark("<!subteam^S_OPS|ops-team> deploy", lbl),
            "@ops-team deploy"
        );
        assert_eq!(to_commonmark("<!here> heads up", lbl), "@here heads up");
    }

    /// Slack emits `<#C…|>` — an empty label — for a channel the reader
    /// may not see, and a bare `<#C…>` from older clients. Both used to
    /// reach the page as-is: the regex required a non-empty label, so the
    /// first was left verbatim in the body and HTML-escaped into the
    /// thread title.
    #[test]
    fn unlabelled_channel_mentions_resolve_through_the_channel_table() {
        let lbl = labels();
        assert_eq!(
            to_commonmark("lunch in <#C_BRIDGE|>", lbl),
            "lunch in #bridge"
        );
        assert_eq!(
            to_commonmark("lunch in <#C_BRIDGE>", lbl),
            "lunch in #bridge"
        );
        assert_eq!(resolve_mentions("<#C_BRIDGE|> now", lbl), "#bridge now");
        // An id the table does not have still shows as a channel, not as
        // raw mrkdwn.
        assert_eq!(to_commonmark("<#C_UNKNOWN|>", lbl), "#C_UNKNOWN");
    }

    #[test]
    fn html_entities_and_emoji() {
        let lbl = no_labels();
        // Entities stay entities in running text: markdown-it shows them
        // as the characters, and never reads them as HTML.
        assert_eq!(
            to_commonmark("a &amp; b &lt;3 &gt;_&lt;", lbl),
            "a &amp; b &lt;3 &gt;_&lt;"
        );
        assert!(to_commonmark(":thumbsup:", lbl).contains('👍'));
        // Unknown shortcode passes through.
        assert_eq!(to_commonmark(":notarealemoji:", lbl), ":notarealemoji:");
    }

    /// Someone typing markup into Slack sees it as they typed it, in
    /// running text and in code alike, and Slack's own quote still quotes.
    #[test]
    fn markup_typed_into_slack_renders_as_typed() {
        let lbl = no_labels();
        assert_eq!(
            to_commonmark("&lt;script&gt;x&lt;/script&gt; &amp; co", lbl),
            "&lt;script&gt;x&lt;/script&gt; &amp; co"
        );
        assert_eq!(
            to_commonmark("run `a &lt; b &amp;&amp; c`\n```\n&lt;div&gt;\n```", lbl),
            "run `a < b && c`\n```\n<div>\n```"
        );
        assert_eq!(
            to_commonmark("&gt; quoted &lt;b&gt;\nplain", lbl),
            "> quoted &lt;b&gt;\n\nplain"
        );
        assert_eq!(
            to_commonmark("<https://e.invalid/?a=1&amp;b=2|see [this]>", lbl),
            "[see \\[this\\]](https://e.invalid/?a=1&b=2)"
        );
        assert_eq!(
            resolve_mentions("&lt;b&gt; &amp; co", lbl),
            "<b> & co",
            "a thread title is plain text; Title escapes it"
        );
    }

    /// Slack shows markdown's syntax as typed; markdown-it would have
    /// made a link, an image, a heading and a table of it (#992).
    #[test]
    fn markdown_typed_into_slack_renders_as_typed() {
        let lbl = no_labels();
        assert_eq!(
            to_commonmark("[x](https://e.test) ![](https://t.test/i.png) a|b", lbl),
            "\\[x\\](https://e.test) !\\[\\](https://t.test/i.png) a\\|b"
        );
        assert_eq!(
            to_commonmark("# not a heading\n---\n- item *bold*", lbl),
            "\\# not a heading\n\\---\n\\- item **bold**"
        );
        assert_eq!(
            to_commonmark("`[x](y)` and <https://x.test/[a]|[b]>", lbl),
            "`[x](y)` and [\\[b\\]](https://x.test/[a])"
        );
    }

    /// A name from the users or channels table, or a mention's own label,
    /// is text on a markdown line (#992).
    #[test]
    fn a_name_in_markdown_renders_as_typed() {
        static ODD: Lazy<BTreeMap<String, String>> = Lazy::new(|| {
            BTreeMap::from([("U_Q".to_string(), "[Q](https://e.test) *".to_string())])
        });
        let lbl = Labels {
            users: INPUTS.lookup("users", &ODD),
            channels: INPUTS.lookup("channels", &ODD),
            team_id: "T01",
        };
        assert_eq!(
            to_commonmark("hi <@U_Q> and <@U_X|`b`&lt;i&gt;>", lbl),
            "hi [@\\[Q\\](https://e.test) \\*](slack://user?team=T01&id=U_Q \
             \"@[Q](https://e.test) * (slack:T01/U_Q)\") \
             and [@\\`b\\`&lt;i&gt;](slack://user?team=T01&id=U_X \
             \"@`b`<i> (slack:T01/U_X)\")",
            "a name in markdown is text in the link, and the title is the name as shown"
        );
        assert_eq!(
            resolve_mentions("hi <@U_Q> and <@U_X|a&amp;b>", lbl),
            "hi @[Q](https://e.test) * and @a&b",
            "a thread title is plain text; Title escapes it"
        );
    }

    /// The people a message mentions are searched as `mention` terms:
    /// users only, never a channel or `@here`, each once.
    #[test]
    fn a_messages_mentions_are_its_users_each_once() {
        let named: Vec<String> = mentioned_users(
            "<@U_DATA|Data> and <@W_WORF>, then <@U_DATA> again; not <#C1|bridge> or <!here>",
            "T01",
        )
        .iter()
        .map(|h| h.as_str().to_string())
        .collect();
        assert_eq!(named, ["slack:T01/U_DATA", "slack:T01/W_WORF"]);
        assert!(
            mentioned_users("<@U_DATA>", "").is_empty(),
            "no workspace, no handle"
        );
    }

    /// A mention is the one link a Slack body carries that the viewer
    /// resolves: a chip with the user's handle. The thread title, plain
    /// text, keeps `@Name`.
    #[test]
    fn a_mention_is_a_chip_link_and_a_title_stays_plain() {
        let lbl = labels();
        assert_eq!(
            to_commonmark("<@U_DATA|Data>, report", lbl),
            "[@Data](slack://user?team=T01&id=U_DATA \"@Data (slack:T01/U_DATA)\"), report"
        );
        assert_eq!(
            to_commonmark("<@U_NOBODY> there?", lbl),
            "[@U_NOBODY](slack://user?team=T01&id=U_NOBODY \"@U_NOBODY (slack:T01/U_NOBODY)\") there?",
            "an id with no profile is still the person"
        );
        assert_eq!(
            resolve_mentions("<@U_DATA>, report", lbl),
            "@Lt. Cmdr. Data, report"
        );
        let no_team = Labels {
            users: INPUTS.lookup("users", &USERS),
            channels: INPUTS.lookup("channels", &CHANNELS),
            team_id: "",
        };
        assert_eq!(
            to_commonmark("<@U_DATA> there?", no_team),
            "@Lt. Cmdr. Data there?",
            "no workspace, no handle: plain text"
        );
    }

    /// Code shows what it holds, so a mention there is `@Name`, not the
    /// markdown of a link; outside the code beside it, it is still a chip.
    #[test]
    fn a_mention_in_code_is_its_name_as_text() {
        let lbl = labels();
        assert_eq!(
            to_commonmark("ask `<@U_DATA>` or <@U_DATA>", lbl),
            "ask `@Lt. Cmdr. Data` or [@Lt. Cmdr. Data](slack://user?team=T01&id=U_DATA \
             \"@Lt. Cmdr. Data (slack:T01/U_DATA)\")"
        );
        assert_eq!(
            to_commonmark("```\n<@U_DATA>, <@U_X|a&amp;b>\n```", lbl),
            "```\n@Lt. Cmdr. Data, @a&b\n```"
        );
    }

    /// A `!` typed straight before a mention or a labelled link stays a
    /// `!`; unescaped, markdown reads the link after it as an image.
    #[test]
    fn a_bang_before_a_construct_is_not_an_image() {
        let lbl = labels();
        assert_eq!(
            to_commonmark("wow!<@U_DATA>", lbl),
            "wow\\![@Lt. Cmdr. Data](slack://user?team=T01&id=U_DATA \
             \"@Lt. Cmdr. Data (slack:T01/U_DATA)\")"
        );
        assert_eq!(
            to_commonmark("!<https://x.test/i.png|pic>", lbl),
            "\\![pic](https://x.test/i.png)"
        );
        assert_eq!(
            to_commonmark("hi! there", lbl),
            "hi! there",
            "a lone `!` is left alone"
        );
    }

    #[test]
    fn blockquote_terminator() {
        let lbl = no_labels();
        let out = to_commonmark("> quoted\nplain follow", lbl);
        assert_eq!(out, "> quoted\n\nplain follow");
    }
}
