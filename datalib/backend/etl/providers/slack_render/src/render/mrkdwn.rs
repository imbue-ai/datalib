//! Slack mrkdwn → CommonMark converter.

use datalib_html_entities::decode;
use std::collections::BTreeMap;
use std::sync::LazyLock;

use datalib_etl_render::html::{
    code_span_parts, escape_md_inline, escape_md_syntax, escape_name_inline, md_code_block,
    md_code_span, md_link_dest,
};
use datalib_etl_render::inputs::Lookup;
use datalib_etl_render::message::chip_link;
use datalib_handle::Handle;
use regex::{Captures, Regex};

use super::emoji::emojize;

static USER_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<@([UW][A-Z0-9_]+)(?:\|([^>]+))?>").unwrap());
// The label may be empty (`<#C123|>`): Slack emits that for a channel
// the message's author can see but the reader may not.
static CHANNEL_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<#([CG][A-Z0-9_]+)(?:\|([^>]*))?>").unwrap());
static SUBTEAM_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<!subteam\^[A-Z0-9_]+(?:\|([^>]+))?>").unwrap());
static SPECIAL_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<!(here|channel|everyone)(?:\|[^>]+)?>").unwrap());
static URL_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<((?:https?|mailto):[^|>\s]+)(?:\|([^>]*))?>").unwrap());
// Bold/strike use look-around-style boundary checks but Rust's `regex`
// crate has no look-around. We compensate by including the boundary
// char in the match and re-emitting it. See [`reflow_bold`] / [`reflow_strike`].
static BOLD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|[^\w*])\*([^\s*][^*\n]*?[^\s*]|[^\s*])\*([^\w*]|$)").unwrap());
static STRIKE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|[^\w~])~([^\s~][^~\n]*?[^\s~]|[^\s~])~([^\w~]|$)").unwrap());
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
    decode(&emojize(&mentions_as_text(text, labels))).into_owned()
}

/// Each user and channel mention as the name it shows, still
/// entity-encoded as Slack's text is.
fn mentions_as_text(text: &str, labels: Labels<'_>) -> String {
    let replaced = USER_REF
        .replace_all(text, |caps: &Captures<'_>| {
            format!("@{}", user_label(caps, labels, slack_encode))
        })
        .into_owned();
    CHANNEL_REF
        .replace_all(&replaced, |caps: &Captures<'_>| {
            format!("#{}", channel_label(caps, labels, slack_encode))
        })
        .into_owned()
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

/// Plain text as Slack writes it in a message, escaping the three
/// characters its Formatting reference names; every other `&` it writes
/// is `&amp;`, so `datalib_html_entities::decode` reads it back exactly.
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
        Some(label) => name(&decode(label.as_str())),
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
    let shown = format!("@{}", emojize(&user_label(caps, labels, str::to_string)));
    match Handle::slack(labels.team_id, &caps[1]) {
        Some(handle) => chip_link(&shown, &handle),
        None => escape_name_inline(&shown),
    }
}

fn channel_label(caps: &Captures<'_>, labels: Labels<'_>, name: fn(&str) -> String) -> String {
    let cid = &caps[1];
    match caps.get(2).map(|m| m.as_str()).filter(|l| !l.is_empty()) {
        Some(label) => name(&decode(label)),
        None => labels
            .channels
            .get(cid)
            .map_or_else(|| cid.to_string(), |n| name(n)),
    }
}

/// A person's or a channel's name on a markdown line, emoji shortcodes
/// and all.
fn md_name(name: &str) -> String {
    escape_md_inline(&emojize(name))
}

/// Render Slack mrkdwn `text` into CommonMark. A `<#C…|name>` carries
/// its own label; a bare `<#C…>` or `<#C…|>` is looked up in
/// `labels.channels`.
///
/// Markdown's own syntax in what someone typed — a `[x](url)`, a `# `,
/// a `|` — is escaped, since Slack shows it as typed. Code is written
/// as markdown where it is found, and it and each name stand in as a
/// placeholder until the end, so none of the passes that read mrkdwn
/// can read into them.
pub fn to_commonmark(text: &str, labels: Labels<'_>) -> String {
    let text = text.replace([HELD_OPEN, HELD_CLOSE, CONSTRUCT_OPEN, CONSTRUCT_CLOSE], "");
    let mut held: Vec<String> = Vec::new();
    let mut out = String::with_capacity(text.len() + 8);
    for piece in code_and_text(&text) {
        let code = match piece {
            Piece::Text(typed) => {
                let starts_line = out.is_empty() || out.ends_with('\n');
                out.push_str(&escape_typed_markdown(&typed, starts_line));
                continue;
            }
            Piece::Block(body) => md_code_block("", &code_text(&body, labels)),
            Piece::Span(inner) => md_code_span(&code_text(&span_text(&inner), labels)),
        };
        held.push(code);
        out.push_str(&held_mark(held.len() - 1));
    }

    let mut hold = |md: String| {
        held.push(md);
        held_mark(held.len() - 1)
    };

    out = USER_REF
        .replace_all(&out, |caps: &Captures<'_>| hold(user_mention(caps, labels)))
        .into_owned();

    out = CHANNEL_REF
        .replace_all(&out, |caps: &Captures<'_>| {
            hold(format!("#{}", channel_label(caps, labels, md_name)))
        })
        .into_owned();

    out = SUBTEAM_REF
        .replace_all(&out, |caps: &Captures<'_>| {
            let name = caps.get(1).map(|m| m.as_str()).unwrap_or("group");
            hold(format!("@{}", md_name(&decode(name))))
        })
        .into_owned();

    out = SPECIAL_REF
        .replace_all(&out, |caps: &Captures<'_>| format!("@{}", &caps[1]))
        .into_owned();

    out = URL_REF
        .replace_all(&out, |caps: &Captures<'_>| {
            let url = decode(&caps[1]);
            match caps.get(2).map(|m| m.as_str()) {
                Some(label) if !label.is_empty() && label != &caps[1] => {
                    format!(
                        "[{}]({})",
                        escape_backticks(&escape_md_syntax(
                            &label.replace(['\r', '\n'], " "),
                            false
                        )),
                        md_link_dest(&url)
                    )
                }
                _ => format!(
                    "<{}>",
                    url.replace('<', "%3C")
                        .replace('>', "%3E")
                        .replace('`', "%60")
                ),
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

    out = terminate_blockquotes(&quote_marks(&out));
    out = emojize(&out);
    HELD.replace_all(&out, |caps: &Captures<'_>| {
        caps[1]
            .parse::<usize>()
            .ok()
            .and_then(|n| held.get(n).cloned())
            .unwrap_or_default()
    })
    .into_owned()
}

/// Brackets the index of markdown held aside until the end: code, or a
/// name. Private-use characters, which are taken out of Slack's text
/// first.
const HELD_OPEN: char = '\u{E000}';
const HELD_CLOSE: char = '\u{E001}';
static HELD: LazyLock<Regex> = LazyLock::new(|| Regex::new("\u{E000}([0-9]+)\u{E001}").unwrap());

fn held_mark(n: usize) -> String {
    format!("{HELD_OPEN}{n}{HELD_CLOSE}")
}

/// A piece of Slack text: what someone typed, or the code in it.
enum Piece {
    Text(String),
    /// A ``` block's content.
    Block(String),
    /// Inline code's content, between its backticks.
    Span(String),
}

/// `text` cut into its code and the text around it. A ``` block may
/// hold blank lines; inline code, as in markdown, never crosses one.
/// Slack lets a block start and end mid-line, and a fence needs lines
/// of its own, so the text beside a block is moved off its lines. A
/// `<…>` construct stays whole in one piece: a mention's label may hold
/// a backtick, which must not open code that cuts the mention in two.
fn code_and_text(text: &str) -> Vec<Piece> {
    let masked = Masked::new(text);
    let mut pieces = Vec::new();
    let mut run = String::new();
    let mut after_block = false;
    for (part, is_code) in code_span_parts(&masked.text) {
        let Some(body) = is_code.then(|| block_body(part)).flatten() else {
            if after_block && !part.starts_with('\n') {
                run.push('\n');
                run.push_str(part.trim_start_matches([' ', '\t']));
            } else {
                run.push_str(part);
            }
            after_block = false;
            continue;
        };
        run.truncate(run.trim_end_matches([' ', '\t']).len());
        if (!run.is_empty() || !pieces.is_empty()) && !run.ends_with('\n') {
            run.push('\n');
        }
        cut_spans(&run, &masked, &mut pieces);
        run.clear();
        let body = body.strip_prefix('\n').unwrap_or(body);
        let body = body.strip_suffix('\n').unwrap_or(body);
        pieces.push(Piece::Block(masked.unmask(body)));
        after_block = true;
    }
    cut_spans(&run, &masked, &mut pieces);
    pieces
}

/// The content of a ``` block, if `part` is one.
fn block_body(part: &str) -> Option<&str> {
    part.strip_prefix("```")?
        .strip_suffix("```")
        .filter(|code| !code.starts_with('`') && !code.ends_with('`'))
}

/// The inline code in `run`, found a paragraph at a time.
fn cut_spans(run: &str, masked: &Masked, pieces: &mut Vec<Piece>) {
    let mut paragraph = 0;
    let mut at = 0;
    for line in run.split_inclusive('\n') {
        let next = at + line.len();
        if is_blank(line) {
            cut_paragraph(&run[paragraph..at], masked, pieces);
            push_text(pieces, masked.unmask(line));
            paragraph = next;
        }
        at = next;
    }
    cut_paragraph(&run[paragraph..], masked, pieces);
}

/// A blank line, to markdown: spaces and tabs at most.
fn is_blank(line: &str) -> bool {
    line.trim_end_matches(['\n', '\r'])
        .trim_matches([' ', '\t'])
        .is_empty()
}

fn cut_paragraph(paragraph: &str, masked: &Masked, pieces: &mut Vec<Piece>) {
    for (part, is_code) in code_span_parts(paragraph) {
        if is_code {
            let ticks = part.len() - part.trim_start_matches('`').len();
            let inner = &part[ticks..part.len() - ticks];
            pieces.push(Piece::Span(masked.unmask(inner)));
        } else {
            push_text(pieces, masked.unmask(part));
        }
    }
}

fn push_text(pieces: &mut Vec<Piece>, text: String) {
    match pieces.last_mut() {
        Some(Piece::Text(before)) => before.push_str(&text),
        _ => pieces.push(Piece::Text(text)),
    }
}

/// What markdown shows of a code span's content: each line end as a
/// space, and one space off each end when both ends have one.
fn span_text(inner: &str) -> String {
    let line = inner.replace("\r\n", " ").replace(['\r', '\n'], " ");
    match line.strip_prefix(' ').and_then(|l| l.strip_suffix(' ')) {
        Some(core) if !line.trim_matches(' ').is_empty() => core.to_string(),
        _ => line,
    }
}

/// Code as it shows: a mention is the name it shows, a link its URL, an
/// entity the character it stands for. Slack draws no emoji in code.
fn code_text(code: &str, labels: Labels<'_>) -> String {
    let text = mentions_as_text(code, labels);
    let text = SUBTEAM_REF.replace_all(&text, |caps: &Captures<'_>| {
        format!("@{}", caps.get(1).map_or("group", |m| m.as_str()))
    });
    let text = SPECIAL_REF.replace_all(&text, |caps: &Captures<'_>| format!("@{}", &caps[1]));
    let text = URL_REF.replace_all(&text, |caps: &Captures<'_>| caps[1].to_string());
    decode(&text).into_owned()
}

/// What someone typed, with markdown's syntax escaped: not inside a
/// `<…>`, which is Slack's own markup (a typed `<` arrives as `&lt;`).
fn escape_typed_markdown(text: &str, starts_line: bool) -> String {
    let masked = Masked::new(text);
    // A `!` typed straight before a construct that becomes a link would
    // make that link an image.
    let escaped = escape_md_syntax(&masked.text, starts_line).replace(
        &format!("!{CONSTRUCT_OPEN}"),
        &format!("\\!{CONSTRUCT_OPEN}"),
    );
    masked.unmask(&escape_backticks(&escaped))
}

/// Every backtick outside code escaped, so the only backticks markdown
/// reads are the fences of the code written here. One left as it is
/// pairs with such a fence and shows the code's content as markdown.
fn escape_backticks(text: &str) -> String {
    text.replace('`', "\\`")
}

/// Slack text with each `<…>` construct held aside as a placeholder.
struct Masked {
    text: String,
    constructs: Vec<String>,
}

impl Masked {
    fn new(text: &str) -> Self {
        let mut constructs: Vec<String> = Vec::new();
        let text = SLACK_CONSTRUCT
            .replace_all(text, |caps: &Captures<'_>| {
                constructs.push(caps[0].to_string());
                format!("{CONSTRUCT_OPEN}{}{CONSTRUCT_CLOSE}", constructs.len() - 1)
            })
            .into_owned();
        Self { text, constructs }
    }

    fn unmask(&self, text: &str) -> String {
        CONSTRUCT_HELD
            .replace_all(text, |caps: &Captures<'_>| {
                caps[1]
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| self.constructs.get(n).cloned())
                    .unwrap_or_default()
            })
            .into_owned()
    }
}

static SLACK_CONSTRUCT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^<>]*>").unwrap());
const CONSTRUCT_OPEN: char = '\u{E002}';
const CONSTRUCT_CLOSE: char = '\u{E003}';
static CONSTRUCT_HELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("\u{E002}([0-9]+)\u{E003}").unwrap());

/// Slack's entities stay entities in what someone typed, so a `<b>`
/// shows as typed rather than being read as HTML; Slack's own `&gt;`
/// quote mark opening a line becomes markdown's.
fn quote_marks(text: &str) -> String {
    text.split('\n')
        .map(|line| match line.strip_prefix("&gt;") {
            Some(rest) => format!(">{rest}"),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
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

    static USERS: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
        let mut m = BTreeMap::new();
        m.insert("U_PICARD".to_string(), "Jean-Luc Picard".to_string());
        m.insert("U_DATA".to_string(), "Lt. Cmdr. Data".to_string());
        m
    });
    static CHANNELS: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
        let mut m = BTreeMap::new();
        m.insert("C_BRIDGE".to_string(), "bridge".to_string());
        m
    });
    static NONE: LazyLock<BTreeMap<String, String>> = LazyLock::new(BTreeMap::new);
    static INPUTS: LazyLock<Inputs> = LazyLock::new(Inputs::default);

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

    /// The regression: `Foo::new::bar` in code showed as `Foo:🆕:bar`.
    #[test]
    fn code_keeps_its_shortcodes() {
        let lbl = no_labels();
        assert_eq!(
            to_commonmark("`Foo::new::bar` is :new::skin-tone-2:", lbl),
            "`Foo::new::bar` is 🆕🏻"
        );
        assert_eq!(
            to_commonmark("```\nlet x = a::new::b;\n```\n:+1::skin-tone-3:", lbl),
            "```\nlet x = a::new::b;\n```\n👍🏼"
        );
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
        static ODD: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
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
        assert_eq!(
            to_commonmark("`<#C_BRIDGE> <!here> <https://x.test/a|b>`", lbl),
            "`#bridge @here https://x.test/a`",
            "a channel is its name and a link its URL"
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

    /// Slack's ``` block may hold code on its own opening and closing
    /// lines. Passed through, markdown read `Kitchen` as the fence's
    /// language and never closed it, so the rest of the thread showed as
    /// code.
    #[test]
    fn a_code_block_on_the_fence_lines_is_closed() {
        let lbl = no_labels();
        assert_eq!(
            to_commonmark("```Kitchen\nPassword: a1```\nafter", lbl),
            "```\nKitchen\nPassword: a1\n```\nafter"
        );
        assert_eq!(
            to_commonmark("see ```x = 1``` then *this*", lbl),
            "see\n```\nx = 1\n```\nthen **this**"
        );
        assert_eq!(
            to_commonmark("```a ```` b```", lbl),
            "`````\na ```` b\n`````",
            "a fence is longer than any backtick run inside it"
        );
    }

    /// Markdown never lets a code span cross a blank line. Paired across
    /// one, the text between was left as code — unescaped, its entities
    /// decoded — and markdown read it as an HTML block or a heading: a
    /// second message, drawn from someone else.
    #[test]
    fn backticks_across_a_blank_line_are_text() {
        let lbl = no_labels();
        assert_eq!(
            to_commonmark("`x\n\n&lt;img src=x&gt;\n\n`", lbl),
            "\\`x\n\n&lt;img src=x&gt;\n\n\\`"
        );
        assert_eq!(
            to_commonmark("`x\n\n## [Picard](tel:+15550100)\n\n`", lbl),
            "\\`x\n\n\\## \\[Picard\\](tel:+15550100)\n\n\\`"
        );
        assert_eq!(
            to_commonmark("`a\n\nb` `[c](tel:+1)`", lbl),
            "\\`a\n\nb` `\\[c\\](tel:+1)\\`",
            "a backtick left alone by the blank line pairs with the next one"
        );
        assert_eq!(
            to_commonmark("```\nfirst\n\nsecond\n```", lbl),
            "```\nfirst\n\nsecond\n```",
            "a ``` block holds blank lines"
        );
    }

    /// A heading or an HTML block can start on the line after a code
    /// span opens, with no blank line, and ends the paragraph the span
    /// was in. Code that runs over lines is written on one line, as
    /// markdown shows it, so no line inside it can start a block.
    #[test]
    fn code_over_lines_is_one_line() {
        let lbl = no_labels();
        assert_eq!(
            to_commonmark("`a\n## [b](tel:+1)`", lbl),
            "`a ## [b](tel:+1)`"
        );
        assert_eq!(to_commonmark("`a\n&lt;div&gt;`", lbl), "`a <div>`");
    }

    /// A backtick outside code is escaped. Left as it is, it paired with
    /// the fence of the code after it, which markdown then showed as
    /// markdown: here, a link's label pairing with the code's fence.
    #[test]
    fn a_backtick_outside_code_pairs_with_nothing() {
        let lbl = no_labels();
        assert_eq!(
            to_commonmark("<https://e.test|a`b> `&lt;img src=x&gt;`", lbl),
            "[a\\`b](https://e.test) `<img src=x>`"
        );
        assert_eq!(
            to_commonmark("` x ``&lt;img src=x&gt;``", lbl),
            "\\` x `<img src=x>`",
            "code is written with a fence of its own length"
        );
    }

    /// A link's URL may hold a backtick, which markdown reads as part of
    /// the link. Code found again after the link was written paired that
    /// backtick with a later one and decoded the entities between.
    #[test]
    fn a_backtick_in_a_url_opens_no_code() {
        let lbl = no_labels();
        assert_eq!(
            to_commonmark("<https://a.test/`> &lt;img src=x&gt; `y`", lbl),
            "<https://a.test/%60> &lt;img src=x&gt; `y`"
        );
    }

    #[test]
    fn blockquote_terminator() {
        let lbl = no_labels();
        let out = to_commonmark("> quoted\nplain follow", lbl);
        assert_eq!(out, "> quoted\n\nplain follow");
    }
}
