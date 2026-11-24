//! What someone typed in Discord as CommonMark. Discord's markdown is
//! nearly the same: what differs is written over (an underline, a line
//! break, a mention), and what Discord does not read as markdown (a
//! table, raw HTML, a fourth-level heading) is escaped so that it shows
//! as typed. TRANSLATE.md §"A message's text" has the rules.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use datalib_etl_render::html::{
    code_span_parts, escape_md_block, escape_md_inline, md_code_block, md_code_span, md_link_dest,
};
use datalib_etl_render::inputs::Lookup;
use datalib_etl_render::message::{chip_link, timestamp_html};
use datalib_handle::Handle;
use regex::{Captures, Regex};

/// What a mention resolves to, each lookup declared as an input of the
/// conversation being rendered.
#[derive(Clone, Copy)]
pub struct Labels<'a> {
    pub users: Lookup<'a, BTreeMap<String, String>>,
    pub channels: Lookup<'a, BTreeMap<String, String>>,
    pub roles: Lookup<'a, BTreeMap<String, String>>,
}

/// Everything in a line that is not plain typing, in the order it is
/// looked for.
static CONSTRUCT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"<@!?(?P<user>\d+)>",
        r"|<@&(?P<role>\d+)>",
        r"|<#(?P<channel>\d+)>",
        r"|<a?:(?P<emoji>\w+):\d+>",
        r"|<t:(?P<time>-?\d{1,13})(?::[a-zA-Z])?>",
        r"|</(?P<command>[^:<>\n]+):\d+>",
        r"|<id:(?P<guide>\w+)>",
        r"|\[(?P<label>[^\[\]\n]+)\]\(<?(?P<dest>https?://[^\s<>()]*(?:\([^\s<>()]*\)[^\s<>()]*)*)>?\)",
        r"|<(?P<angle>https?://[^\s<>]+)>",
        r"|(?P<url>https?://[^\s<>]+)",
        r"|\\(?P<escaped>[[:punct:]])",
    ))
    .unwrap()
});
static STRIKE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"~~([^~\n]+?)~~").unwrap());
static UNDERLINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"__([^_\n](?:[^\n]*?[^_\n])?)__").unwrap());
static FENCE_LANG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\w+#.-]+\n").unwrap());
static HEADING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#{1,3}\s+(.*)$").unwrap());
static LIST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\s*(?:[-*]|\d{1,9}\.)\s+)(\S.*)$").unwrap());

/// Brackets what is held aside while the typing around it is escaped:
/// private-use characters, dropped from what came in.
const OPEN: char = '\u{E000}';
const CLOSE: char = '\u{E001}';
static HELD: LazyLock<Regex> = LazyLock::new(|| Regex::new("\u{E000}([0-9]+)\u{E001}").unwrap());
static HELD_ALONE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^\u{E000}([0-9]+)\u{E001}$").unwrap());

#[derive(Default)]
struct Holder {
    held: Vec<String>,
    /// Which of them are blocks, and stand on lines of their own.
    blocks: Vec<bool>,
}

impl Holder {
    fn hold(&mut self, md: String, block: bool) -> String {
        self.held.push(md);
        self.blocks.push(block);
        format!("{OPEN}{}{CLOSE}", self.held.len() - 1)
    }

    fn is_block(&self, line: &str) -> bool {
        HELD_ALONE
            .captures(line)
            .and_then(|c| c[1].parse::<usize>().ok())
            .and_then(|n| self.blocks.get(n).copied())
            .unwrap_or(false)
    }

    fn release(&self, md: &str) -> String {
        HELD.replace_all(md, |c: &Captures<'_>| {
            c[1].parse::<usize>()
                .ok()
                .and_then(|n| self.held.get(n).cloned())
                .unwrap_or_default()
        })
        .into_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    Plain,
    Quote,
    List,
    Block,
    Blank,
}

/// `text` as CommonMark.
pub fn to_commonmark(text: &str, labels: Labels<'_>) -> String {
    let text = text.replace([OPEN, CLOSE], "").replace('\r', "");
    let mut holder = Holder::default();
    let mut masked = String::with_capacity(text.len());
    for (part, is_code) in code_span_parts(&text) {
        if is_code {
            masked.push_str(&code(part, &mut holder));
        } else {
            masked.push_str(&hold_constructs(part, labels, &mut holder));
        }
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut before: Option<Line> = None;
    let mut quoting = false;
    for line in masked.split('\n') {
        let (kind, md) = one_line(line, &mut quoting, &mut holder);
        if kind == Line::Blank {
            before = Some(Line::Blank);
            continue;
        }
        match before {
            None => {}
            Some(b) if b == kind && kind == Line::List => out.push('\n'),
            // A line break is one in Discord; markdown needs telling.
            Some(b) if b == kind && kind != Line::Block => out.push_str("  \n"),
            Some(_) if out.is_empty() => {}
            Some(_) => out.push_str("\n\n"),
        }
        out.push_str(&md);
        before = Some(kind);
    }
    holder.release(&out)
}

fn one_line(line: &str, quoting: &mut bool, holder: &mut Holder) -> (Line, String) {
    if !*quoting && line.starts_with(">>> ") {
        *quoting = true;
        return (Line::Quote, format!("> {}", typed(&line[4..], holder)));
    }
    if *quoting {
        return (
            Line::Quote,
            format!("> {}", typed(line, holder)).trim_end().to_string(),
        );
    }
    if let Some(quoted) = line.strip_prefix("> ") {
        return (Line::Quote, format!("> {}", typed(quoted, holder)));
    }
    if line.trim().is_empty() {
        return (Line::Blank, String::new());
    }
    if holder.is_block(line) {
        return (Line::Block, line.to_string());
    }
    // A heading in a message would be read as the message's own header.
    if let Some(c) = HEADING.captures(line) {
        return (Line::Plain, format!("**{}**", typed(c[1].trim(), holder)));
    }
    if let Some(small) = line.strip_prefix("-# ") {
        return (
            Line::Plain,
            format!("<small>{}</small>", typed(small, holder)),
        );
    }
    if let Some(c) = LIST.captures(line) {
        return (Line::List, format!("{}{}", &c[1], typed(&c[2], holder)));
    }
    (Line::Plain, typed(line.trim_end(), holder))
}

/// Typing on one line: emphasis as it is, an underline and a strike as
/// Discord reads them, and everything else as typed.
fn typed(line: &str, holder: &mut Holder) -> String {
    let struck = STRIKE
        .replace_all(line, |c: &Captures<'_>| {
            let mark = holder.hold("~~".to_string(), false);
            format!("{mark}{}{mark}", &c[1])
        })
        .into_owned();
    let underlined = UNDERLINE
        .replace_all(&struck, |c: &Captures<'_>| {
            let open = holder.hold("<u>".to_string(), false);
            let close = holder.hold("</u>".to_string(), false);
            format!("{open}{}{close}", &c[1])
        })
        .into_owned();
    escape_md_block(&underlined)
}

/// A code span, or a block on lines of its own.
fn code(part: &str, holder: &mut Holder) -> String {
    let block = part
        .strip_prefix("```")
        .and_then(|p| p.strip_suffix("```"))
        .filter(|body| !body.trim().is_empty() && !body.starts_with('`'));
    if let Some(body) = block {
        let lang = FENCE_LANG.find(body).map_or("", |m| m.as_str().trim_end());
        let body = match lang {
            "" => body.strip_prefix('\n').unwrap_or(body),
            lang => &body[lang.len() + 1..],
        };
        let body = body.strip_suffix('\n').unwrap_or(body);
        let held = holder.hold(md_code_block(lang, body), true);
        return format!("\n{held}\n");
    }
    let ticks = part.len() - part.trim_start_matches('`').len();
    let inner = part.get(ticks..part.len() - ticks).unwrap_or(part);
    holder.hold(md_code_span(inner.trim()), false)
}

fn hold_constructs(text: &str, labels: Labels<'_>, holder: &mut Holder) -> String {
    CONSTRUCT
        .replace_all(text, |c: &Captures<'_>| {
            let get = |name: &str| c.name(name).map(|m| m.as_str());
            if let Some(id) = get("user") {
                return holder.hold(user_mention(id, labels), false);
            }
            if let Some(id) = get("role") {
                let name = labels.roles.get(id).map_or(id, String::as_str);
                return holder.hold(format!("@{}", escape_md_inline(name)), false);
            }
            if let Some(id) = get("channel") {
                let name = labels.channels.get(id).map_or(id, String::as_str);
                return holder.hold(format!("#{}", escape_md_inline(name)), false);
            }
            if let Some(name) = get("emoji") {
                return holder.hold(format!(":{}:", escape_md_inline(name)), false);
            }
            if let Some(secs) = get("time").and_then(|s| s.parse::<i64>().ok()) {
                let ms = secs.checked_mul(1000);
                return holder.hold(timestamp_html(ms, None), false);
            }
            if let Some(name) = get("command") {
                return holder.hold(format!("/{}", escape_md_inline(name)), false);
            }
            if let Some(guide) = get("guide") {
                return holder.hold(escape_md_inline(guide), false);
            }
            if let (Some(label), Some(dest)) = (get("label"), get("dest")) {
                let link = format!("[{}]({})", escape_md_inline(label), md_link_dest(dest));
                return holder.hold(link, false);
            }
            if let Some(url) = get("angle") {
                return holder.hold(format!("<{url}>"), false);
            }
            if let Some(url) = get("url") {
                let (url, after) = url_end(url);
                return format!("{}{after}", holder.hold(format!("<{url}>"), false));
            }
            match get("escaped") {
                Some(c) => holder.hold(format!("\\{c}"), false),
                None => c[0].to_string(),
            }
        })
        .into_owned()
}

/// A bare link cut off from the punctuation typed after it.
fn url_end(url: &str) -> (&str, &str) {
    let mut end = url.len();
    while let Some(last) = url[..end].chars().next_back() {
        let unmatched =
            last == ')' && url[..end].matches(')').count() > url[..end].matches('(').count();
        if unmatched
            || matches!(
                last,
                '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '*' | '|' | '~' | ']'
            )
        {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    url.split_at(end)
}

fn user_mention(id: &str, labels: Labels<'_>) -> String {
    let shown = format!("@{}", labels.users.get(id).map_or(id, String::as_str));
    match Handle::discord(id) {
        Some(handle) => chip_link(&shown, &handle),
        None => escape_md_inline(&shown),
    }
}

static MENTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<@!?(\d+)>").unwrap());

/// The people `text` names, each once, in the order first named. Not the
/// message's own `mentions`, which also holds who it answers.
pub fn mentioned(text: &str) -> Vec<Handle> {
    let mut out: Vec<Handle> = Vec::new();
    for c in MENTION.captures_iter(text) {
        if let Some(handle) = Handle::discord(&c[1]).filter(|h| !out.contains(h)) {
            out.push(handle);
        }
    }
    out
}

/// `md` as a block quote.
pub fn quote(md: &str) -> String {
    md.lines()
        .map(|line| format!("> {line}").trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_render::inputs::Inputs;

    fn convert(text: &str) -> String {
        let inputs = Inputs::default();
        let users: BTreeMap<String, String> =
            [("1702".to_string(), "Will *Riker*".to_string())].into();
        let channels: BTreeMap<String, String> = [("71".to_string(), "bridge".to_string())].into();
        let roles: BTreeMap<String, String> = [("81".to_string(), "officers".to_string())].into();
        to_commonmark(
            text,
            Labels {
                users: inputs.lookup("users", &users),
                channels: inputs.lookup("channels", &channels),
                roles: inputs.lookup("roles", &roles),
            },
        )
    }

    #[test]
    fn mentions_read_as_names_and_a_person_is_a_chip() {
        assert_eq!(
            convert("<@1702> and <@!9> to <#71>, <@&81>"),
            "[@Will \\*Riker\\*](datalib:handle/discord/1702 \"@Will *Riker* (discord:1702)\") and \
             [@9](datalib:handle/discord/9 \"@9 (discord:9)\") to #bridge, @officers"
        );
        assert_eq!(convert("<#404> </warp:5> <:borg:6>"), "#404 /warp :borg:");
        let handles: Vec<String> = mentioned("<@1702> <@!1702> <@&81> <@9>")
            .iter()
            .map(|h| h.as_str().to_string())
            .collect();
        assert_eq!(handles, ["discord:1702", "discord:9"]);
    }

    /// Discord breaks a line where one was typed.
    #[test]
    fn a_line_break_stays_one() {
        assert_eq!(convert("Make it so.\nEngage."), "Make it so.  \nEngage.");
        assert_eq!(convert("One.\n\n\nTwo."), "One.\n\nTwo.");
    }

    #[test]
    fn emphasis_stays_and_an_underline_and_a_strike_are_discords() {
        assert_eq!(convert("**bold** *it* _it_"), "**bold** *it* _it_");
        assert_eq!(convert("__under__ ~~gone~~"), "<u>under</u> ~~gone~~");
        assert_eq!(convert("\\*not\\* so"), "\\*not\\* so");
    }

    /// A `##` line in a body would be styled as a message's own header.
    #[test]
    fn a_heading_is_bold_and_never_a_heading() {
        assert_eq!(convert("## Orders\nEngage."), "**Orders**  \nEngage.");
        assert!(convert("#### four").starts_with("\\#### four"));
        assert_eq!(convert("-# by the way"), "<small>by the way</small>");
    }

    #[test]
    fn quotes_and_lists_end_where_they_end() {
        assert_eq!(convert("> one\n> two\nthree"), "> one  \n> two\n\nthree");
        assert_eq!(convert(">>> one\ntwo"), "> one  \n> two");
        assert_eq!(convert("- a\n- b\nc"), "- a\n- b\n\nc");
    }

    #[test]
    fn code_is_shown_as_typed() {
        assert_eq!(
            convert("run `<@1702> **x**` now"),
            "run `<@1702> **x**` now"
        );
        assert_eq!(
            convert("see ```rust\nlet x = \"*\";\n``` done"),
            "see\n\n```rust\nlet x = \"*\";\n```\n\n done"
        );
        assert_eq!(convert("```one line```"), "```\none line\n```");
    }

    #[test]
    fn links_keep_their_punctuation_outside() {
        assert_eq!(
            convert("see https://a.test/x_y_z. And (https://b.test/w(1))"),
            "see <https://a.test/x_y_z>. And (<https://b.test/w(1)>)"
        );
        assert_eq!(
            convert("[the *log*](https://a.test/1)"),
            "[the \\*log\\*](https://a.test/1)"
        );
        assert_eq!(convert("<https://a.test/>"), "<https://a.test/>");
        assert_eq!(
            convert("[x](javascript:alert)"),
            "\\[x\\](javascript:alert)"
        );
    }

    /// What Discord shows as typed is never markup here.
    #[test]
    fn markup_discord_does_not_read_shows_as_typed() {
        assert_eq!(convert("<b>x</b> & y"), "&lt;b&gt;x&lt;/b&gt; &amp; y");
        assert_eq!(convert("a | b"), "a \\| b");
        assert_eq!(convert("![x](y)"), "!\\[x\\](y)");
        assert_eq!(convert("\u{E000}0\u{E001}"), "0");
    }

    #[test]
    fn a_time_is_written_as_every_stamp_is() {
        let got = convert("at <t:1714521600:R>");
        assert!(got.contains("datetime=\"2024-05-01T00:00:00"), "{got}");
    }
}
