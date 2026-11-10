//! Escaping for upstream text a renderer splices into its markdown.
//!
//! The UI renders every document with markdown-it `html: true`, so a
//! name or a subject written verbatim is parsed as HTML and markdown. A
//! string that is plain text upstream goes through one of these at the
//! point it becomes markup; a string the source itself authored as
//! markup (a Notion page, an email's HTML part) does not. Which helper
//! is a question of where the text lands:
//!
//! - between raw HTML tags, in an HTML block: [`escape_text`] (between
//!   tags on a markdown line, like the message header's author span,
//!   markdown still applies: [`escape_md_inline`]);
//! - in a double-quoted attribute: [`escape_attr`];
//! - on one markdown line — a list item, a table cell, link text, a
//!   heading, a paragraph: [`escape_md_inline`];
//! - a multi-line plain-text body: [`escape_md_block`];
//! - text in another markup that must not open markdown's own
//!   constructs (Slack's mrkdwn): [`escape_md_syntax`];
//! - markdown someone else wrote goes in as written: the page renders
//!   each message alone and sanitizes it
//!   (docs/dev/data_architecture_parse_and_render.md), and
//!   [`close_open_blocks`] closes what it leaves open;
//! - a link destination: [`md_link_dest`];
//! - inside a code span: [`md_code_span`].
//!
//! Here rather than in `datalib_etl`: that crate sits upstream of ~130
//! test targets, and only the render side writes markup.

/// Escape text that lands between tags. A line break becomes a
/// character reference: in an HTML block a blank line ends the block and
/// markdown resumes, so the text must not carry one.
pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        push_html_escaped(&mut out, c);
    }
    out
}

/// Escape a value going inside a double-quoted attribute; a line break
/// becomes a character reference, as in [`escape_text`].
pub fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("&quot;"),
            _ => push_html_escaped(&mut out, c),
        }
    }
    out
}

/// Plain text bound for one markdown line, escaped so it reads as typed:
/// HTML-escaped, every character markdown reads as inline syntax
/// backslash-escaped, and a leading character that would open a block
/// (`# heading`, `- item`, `1. item`) escaped too. A line break becomes a
/// space, since it would end the construct the text sits in.
///
/// Not for text inside a raw HTML block, where markdown is not parsed
/// and the backslashes would show: use [`escape_text`] there.
pub fn escape_md_inline(s: &str) -> String {
    let one_line = s.replace(['\r', '\n'], " ");
    escape_md_line(&one_line, true)
}

/// Multi-line plain text — a note, a description, a text message — as
/// markdown that reads as typed. Each line is HTML-escaped, and a line
/// that would open a heading, a fence, a list, a quote, a table or a code
/// block, or underline the line above into a heading, has that marker
/// escaped. Line breaks are kept, as `\n`: markdown reads a lone `\r` as
/// one too. Inline emphasis (`*really*`) is left to render as emphasis,
/// which is what a person typing it meant; links and images are escaped,
/// so a sender cannot dress text up as a link.
pub fn escape_md_block(s: &str) -> String {
    s.split('\n')
        .flat_map(|line| line.strip_suffix('\r').unwrap_or(line).split('\r'))
        .map(|line| escape_md_line(line, false))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Text written in another markup — Slack's mrkdwn — with markdown's
/// own syntax escaped, so only that markup's constructs survive: links
/// and images (`[`, `]`), table cells (`|`), backslash escapes, and
/// whatever would open a block at the start of a line, which the first
/// line is only when `starts_line`. HTML entities, `*`, `_`, `~` and
/// backticks are the other markup's and left alone.
pub fn escape_md_syntax(text: &str, starts_line: bool) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for (n, line) in text.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
        }
        let at_line_start = n > 0 || starts_line;
        let body = if at_line_start {
            line.trim_start()
        } else {
            line
        };
        push_indent(&mut out, &line[..line.len() - body.len()], body);
        let marker = block_marker_at(body).filter(|_| at_line_start);
        let chars: Vec<(usize, char)> = body.char_indices().collect();
        for (k, &(i, c)) in chars.iter().enumerate() {
            let next = chars.get(k + 1).map(|&(_, c)| c);
            let syntax = match c {
                '[' | ']' | '|' => true,
                '\\' => next.is_none_or(|n| n.is_ascii_punctuation()),
                _ => false,
            };
            if syntax || Some(i) == marker {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out
}

/// A URL as a markdown link destination. One that markdown would read
/// to its end is left as it is; anything else goes in `<…>`, which takes
/// spaces and parentheses, with the angle brackets and line breaks that
/// would end it percent-encoded.
pub fn md_link_dest(url: &str) -> String {
    let bare = !url
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>' | '\\'));
    if bare && !url.is_empty() {
        return url.to_string();
    }
    let mut out = String::with_capacity(url.len() + 2);
    out.push('<');
    for c in url.chars() {
        match c {
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '\n' => out.push_str("%0A"),
            '\r' => out.push_str("%0D"),
            _ => out.push(c),
        }
    }
    out.push('>');
    out
}

/// Text inside a code span. Markdown takes a code span's content
/// literally — an entity there shows as `&lt;`, so nothing is
/// HTML-escaped — and the only way out is a backtick run, which the
/// fence is made longer than.
pub fn md_code_span(s: &str) -> String {
    let one_line = s.replace(['\r', '\n'], " ");
    let longest_run = one_line
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest_run + 1);
    // Markdown strips one space from each end when both ends have one,
    // so a space added each side keeps a leading or trailing backtick
    // from joining the fence, and text that starts and ends with a space
    // keeps both.
    let spaced =
        one_line.starts_with(' ') && one_line.ends_with(' ') && !one_line.trim().is_empty();
    if longest_run > 0 || spaced {
        format!("{fence} {one_line} {fence}")
    } else {
        format!("{fence}{one_line}{fence}")
    }
}

/// A fenced code block around `body`, shown exactly as it is. The fence
/// is longer than any run of backticks inside, so the body cannot close
/// it early; `lang` keeps only what an info string can carry.
pub fn md_code_block(lang: &str, body: &str) -> String {
    let longest_run = body.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest_run.max(2) + 1);
    let lang: String = lang
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '`')
        .collect();
    format!("{fence}{lang}\n{body}\n{fence}")
}

/// `md` with any block it leaves open closed at its end: a fence, or an
/// HTML block that only a closing tag or marker ends (`<pre`, `<!--`,
/// `<?`, `<!X`, `<![CDATA[`). Markdown from upstream is pasted into a
/// page of many messages, and a block one message leaves open would
/// swallow every message after it.
pub fn close_open_blocks(md: &str) -> String {
    let mut open: Option<String> = None;
    for line in md.split('\n') {
        let indent = line.len() - line.trim_start_matches(' ').len();
        let trimmed = &line[indent..];
        open = match open {
            Some(fence) if fence_opener(&fence).is_some() => {
                (!closes_fence(trimmed, &fence)).then_some(fence)
            }
            Some(end) => (!ends_html_block(line, &end)).then_some(end),
            None if indent > 3 => None,
            None => opens_fence(trimmed).or_else(|| {
                let end = html_block_end(trimmed)?;
                (!ends_html_block(trimmed, &end)).then_some(end)
            }),
        };
    }
    match open {
        Some(close) => format!("{md}\n{close}"),
        None => md.to_string(),
    }
}

/// [`fence_opener`], less a backtick run with a backtick after it on its
/// line, which markdown reads as a code span.
pub(crate) fn opens_fence(line: &str) -> Option<String> {
    let fence = fence_opener(line)?;
    (!(fence.starts_with('`') && line[fence.len()..].contains('`'))).then_some(fence)
}

/// What ends the HTML block `line` opens, for the kinds that a blank
/// line does not end.
fn html_block_end(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix('<') {
        let lower = rest.to_ascii_lowercase();
        for tag in ["pre", "script", "style", "textarea"] {
            let after = lower.strip_prefix(tag);
            if after.is_some_and(|a| a.is_empty() || a.starts_with([' ', '\t', '>'])) {
                return Some(format!("</{tag}>"));
            }
        }
    }
    let end = if line.starts_with("<!--") {
        "-->"
    } else if line.starts_with("<?") {
        "?>"
    } else if line.starts_with("<![CDATA[") {
        "]]>"
    } else if line
        .strip_prefix("<!")
        .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_alphabetic()))
    {
        ">"
    } else {
        return None;
    };
    Some(end.to_string())
}

/// Whether `line` holds what ends an HTML block. Any of the four
/// closing tags ends a `<pre>`-kind block, whichever opened it.
fn ends_html_block(line: &str, end: &str) -> bool {
    if end.starts_with("</") {
        let lower = line.to_ascii_lowercase();
        return ["</pre>", "</script>", "</style>", "</textarea>"]
            .iter()
            .any(|t| lower.contains(t));
    }
    let after_opener = match end {
        "-->" => line.strip_prefix("<!--").unwrap_or(line),
        "?>" => line.strip_prefix("<?").unwrap_or(line),
        _ => line,
    };
    after_opener.contains(end)
}

/// The backticks or tildes opening a fenced block, if `line` opens one.
fn fence_opener(line: &str) -> Option<String> {
    let mark = line.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let run: String = line.chars().take_while(|&c| c == mark).collect();
    (run.len() >= 3).then_some(run)
}

pub(crate) fn closes_fence(line: &str, fence: &str) -> bool {
    let mark = fence.chars().next().expect("a fence has a mark");
    let run = line.chars().take_while(|&c| c == mark).count();
    run >= fence.len() && line[run..].trim().is_empty()
}

/// `text` cut into its code spans (`true`, backticks included) and the
/// text between them (`false`), in order. A backtick run with no run of
/// the same length after it opens nothing, as in markdown.
pub fn code_span_parts(text: &str) -> Vec<(&str, bool)> {
    let mut parts = Vec::new();
    let mut plain_from = 0;
    let mut at = 0;
    while let Some(i) = text[at..].find('`') {
        let open = at + i;
        let run = backtick_run(&text[open..]);
        let after = open + run;
        match closing_backtick_run(&text[after..], run) {
            Some(close) => {
                let end = after + close + run;
                if plain_from < open {
                    parts.push((&text[plain_from..open], false));
                }
                parts.push((&text[open..end], true));
                plain_from = end;
                at = end;
            }
            None => at = after,
        }
    }
    if plain_from < text.len() {
        parts.push((&text[plain_from..], false));
    }
    parts
}

fn backtick_run(s: &str) -> usize {
    s.len() - s.trim_start_matches('`').len()
}

/// Where the next run of exactly `run` backticks starts in `s`: the end
/// of a code span opened by that many.
fn closing_backtick_run(s: &str, run: usize) -> Option<usize> {
    let mut at = 0;
    while let Some(i) = s[at..].find('`') {
        let start = at + i;
        let len = backtick_run(&s[start..]);
        if len == run {
            return Some(start);
        }
        at = start + len;
    }
    None
}

fn push_html_escaped(out: &mut String, c: char) {
    match c {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '\n' => out.push_str("&#10;"),
        '\r' => out.push_str("&#13;"),
        _ => out.push(c),
    }
}

/// One line of plain text. `all_inline` escapes every inline marker
/// (for a name or a title, which has no emphasis to keep); otherwise
/// only the ones that make links, images, code, tables and strikethrough.
fn escape_md_line(line: &str, all_inline: bool) -> String {
    let body = line.trim_start();
    let indent = &line[..line.len() - body.len()];
    let block_marker_at = block_marker_at(body);
    let chars: Vec<(usize, char)> = body.char_indices().collect();
    let mut out = String::with_capacity(line.len() + 8);
    push_indent(&mut out, indent, body);
    for (n, &(i, c)) in chars.iter().enumerate() {
        let prev = n.checked_sub(1).map(|p| chars[p].1);
        let next = chars.get(n + 1).map(|&(_, c)| c);
        let inline_syntax = match c {
            '[' | ']' | '`' => true,
            // A backslash escapes only punctuation, or breaks the line
            // at its end.
            '\\' => next.is_none_or(|n| n.is_ascii_punctuation()),
            // Strikethrough takes two.
            '~' => prev == Some('~') || next == Some('~'),
            // A pipe is a table cell in a block as much as on a line.
            '|' => true,
            '*' => all_inline,
            // Between two letters or digits an underscore cannot open or
            // close emphasis: `snake_case`, `:robot_face:`.
            '_' => all_inline && !(is_word(prev) && is_word(next)),
            _ => false,
        };
        if inline_syntax || Some(i) == block_marker_at {
            out.push('\\');
            out.push(c);
        } else {
            push_html_escaped(&mut out, c);
        }
    }
    out
}

/// Four columns of indent open a code block. Written as a reference, the
/// first of them is text rather than indent, and shows the same.
fn push_indent(out: &mut String, indent: &str, body: &str) {
    match indent.chars().next() {
        Some(first @ (' ' | '\t')) if !body.is_empty() && indent_columns(indent) >= 4 => {
            out.push_str(if first == ' ' { "&#32;" } else { "&#9;" });
            out.push_str(&indent[1..]);
        }
        _ => out.push_str(indent),
    }
}

/// How far markdown reads a run of spaces and tabs as indenting a line:
/// a tab reaches the next multiple of four.
fn indent_columns(indent: &str) -> usize {
    let mut columns = 0;
    for c in indent.chars() {
        match c {
            ' ' => columns += 1,
            '\t' => columns += 4 - columns % 4,
            _ => break,
        }
    }
    columns
}

fn is_word(c: Option<char>) -> bool {
    c.is_some_and(char::is_alphanumeric)
}

/// Where the one character is that would make this line open a block —
/// a heading's `#`, a list item's bullet or the `.` of its `1.`, a rule,
/// or the underline that turns the line above into a heading. `None`
/// for a line that opens nothing, like `#hashtag`, `-5°C` or `1.1937`.
fn block_marker_at(body: &str) -> Option<usize> {
    let first = body.chars().next()?;
    let ends_marker = |at: usize| {
        body[at..]
            .chars()
            .next()
            .is_none_or(|c| c == ' ' || c == '\t')
    };
    let only = |mark: char| {
        body.trim_end()
            .chars()
            .all(|c| c == mark || c == ' ' || c == '\t')
    };
    let hashes = body.len() - body.trim_start_matches('#').len();
    let digits = body.bytes().take_while(u8::is_ascii_digit).count();
    match first {
        '#' if hashes <= 6 && ends_marker(hashes) => Some(0),
        '-' | '+' | '*' if ends_marker(1) || only(first) => Some(0),
        '=' if only('=') => Some(0),
        '_' if only('_') => Some(0),
        '0'..='9'
            if digits <= 9
                && matches!(body.as_bytes().get(digits), Some(b'.' | b')'))
                && ends_marker(digits + 1) =>
        {
            Some(digits)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_escapes_markup_without_double_escaping() {
        assert_eq!(escape_text("Riker & Troi"), "Riker &amp; Troi");
        assert_eq!(escape_text("<b>hi</b>"), "&lt;b&gt;hi&lt;/b&gt;");
        // The ampersand of an escape we just emitted is not re-escaped,
        // which is what escaping `&` first buys.
        assert_eq!(escape_text("a < b & c"), "a &lt; b &amp; c");
    }

    #[test]
    fn attr_escapes_the_quote_that_would_break_out() {
        assert_eq!(
            escape_attr("https://e.com/?a=b&c=\"d\""),
            "https://e.com/?a=b&amp;c=&quot;d&quot;"
        );
    }

    #[test]
    fn an_inline_value_reads_as_typed() {
        assert_eq!(
            escape_md_inline("<script>x</script> & co"),
            "&lt;script&gt;x&lt;/script&gt; &amp; co"
        );
        assert_eq!(escape_md_inline("# Ops"), "\\# Ops");
        assert_eq!(escape_md_inline("- Bob"), "\\- Bob");
        assert_eq!(escape_md_inline("1. Picard"), "1\\. Picard");
        // Only a marker followed by a space opens anything.
        assert_eq!(escape_md_inline("#ops -5°C 1.1937"), "#ops -5°C 1.1937");
        assert_eq!(escape_md_inline("---"), "\\---");
        assert_eq!(escape_md_inline("*Bob* [x](y)"), "\\*Bob\\* \\[x\\](y)");
        assert_eq!(escape_md_inline("a | b\nc"), "a \\| b c");
        // A leading `*` is escaped once, not twice.
        assert_eq!(escape_md_inline("***"), "\\*\\*\\*");
        assert_eq!(escape_md_inline("> quote"), "&gt; quote");
        assert_eq!(escape_md_inline("Riker, Will"), "Riker, Will");
        assert_eq!(
            escape_md_inline(":robot_face: _hi_"),
            ":robot_face: \\_hi\\_"
        );
        assert_eq!(
            escape_md_inline("C:\\Users ~/x ~~y~~"),
            "C:\\Users ~/x \\~\\~y\\~\\~"
        );
    }

    #[test]
    fn a_block_keeps_its_lines_and_emphasis_but_opens_no_structure() {
        let typed =
            "Hi <b>Data</b>,\n# not a heading\n*really*\n---\n![x](https://e.invalid/p.png)";
        assert_eq!(
            escape_md_block(typed),
            "Hi &lt;b&gt;Data&lt;/b&gt;,\n\\# not a heading\n*really*\n\\---\n!\\[x\\](https://e.invalid/p.png)"
        );
    }

    #[test]
    fn a_link_destination_cannot_be_closed_early() {
        assert_eq!(
            md_link_dest("https://e.invalid/a?b=1&c=2"),
            "https://e.invalid/a?b=1&c=2"
        );
        assert_eq!(
            md_link_dest("https://e.invalid/a_(b)"),
            "<https://e.invalid/a_(b)>"
        );
        assert_eq!(
            md_link_dest("https://e.invalid/a b"),
            "<https://e.invalid/a b>"
        );
        assert_eq!(
            md_link_dest("https://e.invalid/<x>"),
            "<https://e.invalid/%3Cx%3E>"
        );
    }

    #[test]
    fn a_code_block_outlasts_the_fences_inside_it() {
        assert_eq!(md_code_block("json", "{}"), "```json\n{}\n```");
        assert_eq!(
            md_code_block("rust `x`\n", "```\n</details>\n```"),
            "````rust\n```\n</details>\n```\n````"
        );
    }

    #[test]
    fn a_code_span_outlasts_the_backticks_inside_it() {
        assert_eq!(md_code_span("<id>"), "`<id>`");
        assert_eq!(md_code_span("a`b"), "`` a`b ``");
        // Markdown strips one space from each end when both have one.
        assert_eq!(md_code_span(" a "), "`  a  `");
        assert_eq!(md_code_span("  "), "`  `");
    }

    /// A blank line ends an HTML block, so text between tags carries
    /// its line breaks as references (#992).
    #[test]
    fn text_between_tags_carries_no_line_break() {
        assert_eq!(escape_text("a\n\n<b>"), "a&#10;&#10;&lt;b&gt;");
        assert_eq!(escape_attr("a\r\nb"), "a&#13;&#10;b");
    }

    /// Each of these opened a block the text did not ask for: markdown
    /// reads a lone `\r` as a line break, four columns of indent as code,
    /// `___` as a rule and a `-|-` row as a table's.
    #[test]
    fn a_block_opens_nothing_markdown_reads_into_it() {
        assert_eq!(escape_md_block("a\r- b\r\nc"), "a\n\\- b\nc");
        assert_eq!(escape_md_block("a\n\n    code"), "a\n\n&#32;   code");
        assert_eq!(escape_md_block("\tcode"), "&#9;code");
        assert_eq!(escape_md_block("  not code\n    "), "  not code\n    ");
        assert_eq!(escape_md_block("___\n_ _ _"), "\\___\n\\_ _ _");
        assert_eq!(escape_md_block("a|b\n-|-"), "a\\|b\n-\\|-");
        assert_eq!(escape_md_block("-\t-\t-"), "\\-\t-\t-");
    }

    /// The grid's Contents cell (`plain_text`) reads an escaped string
    /// back as it was typed. Strings are drawn from markdown's
    /// punctuation, less what `plain_text` drops on purpose whatever the
    /// escaping: anything shaped like a tag, a run of punctuation with no
    /// word in it (a divider), a bare `(http…)`, and runs of whitespace,
    /// which it reads as one space. Emphasis is a block's to keep, so a
    /// block's strings have no `*` or `_`.
    #[test]
    fn plain_text_reads_an_escaped_string_as_typed() {
        const PUNCT: &[char] = &[
            '[', ']', '(', ')', '!', '#', '-', '+', '=', '|', '\\', '~', '`', '*', '_', ':', '/',
            '.', '&', ';', '"', '\'', '{', '}', '%', '$', '1',
        ];
        let plain = |md: &str| datalib_schema::plain_text::plain_text(md, usize::MAX);
        let mut seed: u64 = 0x1701_d00d_5eed_cafe;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut failures = Vec::new();
        for _ in 0..5000 {
            let words: Vec<String> = (0..1 + next() % 4)
                .map(|_| {
                    let mut w = String::from(if next() % 2 == 0 { "a" } else { "bm" });
                    for _ in 0..next() % 6 {
                        w.push(PUNCT[(next() % PUNCT.len() as u64) as usize]);
                    }
                    w
                })
                .collect();
            let typed = words.join(" ");
            if plain(&escape_md_inline(&typed)) != typed {
                failures.push(("inline", typed.clone(), escape_md_inline(&typed)));
            }
            let block: String = words
                .join("\n")
                .chars()
                .filter(|c| !matches!(c, '*' | '_'))
                .collect();
            let squashed = block.split_whitespace().collect::<Vec<_>>().join(" ");
            if plain(&escape_md_block(&block)) != squashed {
                failures.push(("block", block.clone(), escape_md_block(&block)));
            }
        }
        failures.truncate(10);
        assert!(failures.is_empty(), "{failures:#?}");
    }
    /// A message that leaves a block open no longer takes the messages
    /// after it into that block.
    #[test]
    fn a_block_left_open_is_closed_at_the_end() {
        assert_eq!(
            close_open_blocks("```rust\nfn main() {"),
            "```rust\nfn main() {\n```"
        );
        assert_eq!(
            close_open_blocks("a\n````\n```\nb"),
            "a\n````\n```\nb\n````"
        );
        assert_eq!(close_open_blocks("<PRE>x\ny"), "<PRE>x\ny\n</pre>");
        assert_eq!(close_open_blocks("<!-- note\nmore"), "<!-- note\nmore\n-->");
        assert_eq!(close_open_blocks("<![CDATA[x"), "<![CDATA[x\n]]>");
        assert_eq!(close_open_blocks("<!DOCTYPE"), "<!DOCTYPE\n>");
    }

    #[test]
    fn a_closed_block_is_left_alone() {
        for md in [
            "```\ncode\n```\nafter",
            "~~~\n```\n~~~",
            "<pre>x</pre>",
            "<!-- one line -->",
            "<!--\n-->",
            "<script>\n</style>",
            "<?php ?>",
            "<!DOCTYPE html>",
            "    ```",
            "> ```",
            "<prefix>",
            "`` ``` ``",
            "```x = 1```",
        ] {
            assert_eq!(close_open_blocks(md), md, "{md:?}");
        }
    }
}
