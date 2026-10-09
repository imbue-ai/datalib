// Rendered markdown as the words a person reads in a one-line cell: the
// grid's Contents column, and a qmd hit's snippet. Markup goes — tags,
// images, link targets, heading and quote marks, emphasis, code fences,
// entities — and every run of whitespace, non-breaking included, is one
// space.

/// The words of `markdown` on one line. Reading stops once more than
/// `limit` characters are in hand, so a long body costs only its start.
///
/// A `<details>` block is folded on the page, and left out here — a
/// reply's Thinking is not the reply — unless the text is nothing else:
/// then one block reads as its summary and contents, as on a tool call's
/// own row, and several as their summaries.
pub fn plain_text(markdown: &str, limit: usize) -> String {
    let mut out = Words::default();
    let mut folded: Option<Folded> = None;
    let mut summaries = Words::default();
    let mut first_body: Option<Words> = None;
    let mut blocks = 0;
    let mut fence: Option<Fence> = None;
    let mut fold = |block: Folded| {
        summaries.push(&block.summary);
        first_body.get_or_insert(block.body);
    };
    for line in markdown.lines() {
        if out.chars > limit {
            break;
        }
        let mut rest = line.trim_start();
        if folded.is_none() {
            let Some(opened) = rest.strip_prefix("<details>") else {
                if let Some(text) = line_text(line, &mut fence) {
                    out.push(&text);
                }
                continue;
            };
            blocks += 1;
            let (summary, after) = summary_of(opened);
            folded = Some(Folded {
                summary: line_text(summary, &mut None).unwrap_or_default(),
                body: Words::default(),
            });
            rest = after;
        }
        let block = folded.as_mut().expect("inside a block");
        let (inside, closed) = match rest.find("</details>") {
            Some(at) => (&rest[..at], true),
            None => (rest, false),
        };
        if blocks == 1 && block.body.chars <= limit {
            if let Some(text) = line_text(inside, &mut fence) {
                block.body.push(&text);
            }
        }
        if closed {
            fence = None;
            fold(folded.take().unwrap());
        }
    }
    // A block the text ends inside — cut off — reads as closed.
    if let Some(block) = folded.take() {
        fold(block);
    }
    if !out.text.is_empty() {
        return out.text;
    }
    if let (1, Some(body)) = (blocks, first_body) {
        summaries.push(&body.text);
    }
    summaries.text
}

/// `<summary>…</summary>` at the start of a `<details>` line, and what
/// follows it.
fn summary_of(opened: &str) -> (&str, &str) {
    const OPEN: &str = "<summary>";
    const CLOSE: &str = "</summary>";
    match (opened.find(OPEN), opened.find(CLOSE)) {
        (Some(a), Some(b)) if a < b => (&opened[a + OPEN.len()..b], &opened[b + CLOSE.len()..]),
        _ => ("", opened),
    }
}

struct Folded {
    summary: String,
    body: Words,
}

#[derive(Default)]
struct Words {
    text: String,
    chars: usize,
}

impl Words {
    /// Appends `text`'s words, one space apart. A run of four or more
    /// punctuation marks and nothing else (`-::~:~::~`, `======`) is a
    /// divider someone drew, not a word.
    fn push(&mut self, text: &str) {
        for word in text.split_whitespace() {
            if word.len() >= 4 && word.chars().all(|c| c.is_ascii_punctuation()) {
                continue;
            }
            if !self.text.is_empty() {
                self.text.push(' ');
                self.chars += 1;
            }
            self.text.push_str(word);
            self.chars += word.chars().count();
        }
    }
}

/// A code fence's mark and how many of it opened the fence.
#[derive(Clone, Copy)]
struct Fence {
    mark: char,
    len: usize,
}

impl Fence {
    fn of(line: &str) -> Option<Self> {
        let mark = line.chars().next().filter(|c| matches!(c, '`' | '~'))?;
        let len = line.chars().take_while(|&c| c == mark).count();
        (len >= 3).then_some(Self { mark, len })
    }

    fn is_closed_by(self, line: &str) -> bool {
        Self::of(line).is_some_and(|f| f.mark == self.mark && f.len >= self.len)
            && line.trim_start_matches(self.mark).trim().is_empty()
    }
}

/// One line's words. Inside a code fence the text is literal — a
/// backslash or a `*` there is what was typed — so it is kept as is.
fn line_text(line: &str, fence: &mut Option<Fence>) -> Option<String> {
    let line = line.trim();
    if let Some(open) = *fence {
        if open.is_closed_by(line) {
            *fence = None;
            return None;
        }
        return Some(line.to_string());
    }
    if let Some(open) = Fence::of(line) {
        *fence = Some(open);
        return None;
    }
    if is_rule(line) {
        return None;
    }
    let line = without_block_marks(line);
    let line = unescaped(line);
    let line = without_images(&line);
    let line = without_link_targets(&line);
    let line = without_tags(&line);
    Some(without_emphasis(&line))
}

/// A horizontal rule: three or more of one of `-`, `*`, `_`, spaces
/// allowed between (`* * *`).
fn is_rule(line: &str) -> bool {
    let marks: Vec<char> = line.chars().filter(|c| !c.is_whitespace()).collect();
    marks.len() >= 3 && matches!(marks[0], '-' | '*' | '_') && marks.iter().all(|&c| c == marks[0])
}

/// A quote's `>`s, then a heading's `#`s — only where one is followed by a
/// space, so `#general` and `#4` stay words.
fn without_block_marks(line: &str) -> &str {
    let mut rest = line;
    while let Some(r) = rest.strip_prefix('>') {
        rest = r.trim_start();
    }
    let hashes = rest.len() - rest.trim_start_matches('#').len();
    if (1..=6).contains(&hashes) && rest[hashes..].starts_with(' ') {
        rest = rest[hashes..].trim_start();
    }
    rest
}

/// The rendered bodies carry escaped HTML (`&lt;br&gt;`) as well as real
/// tags, and the line breaks and indents an escape writes as references;
/// decoding first lets one pass strip both. `&amp;` goes last so
/// `&amp;lt;` stays the text `&lt;`. Backslash escapes wait for
/// [`without_emphasis`], so an escaped `*` is not read as emphasis.
fn unescaped(line: &str) -> String {
    line.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&#10;", " ")
        .replace("&#13;", " ")
        .replace("&#32;", " ")
        .replace("&#9;", " ")
        .replace("&amp;", "&")
}

/// `![alt](target)` says nothing in a cell — mostly a logo — and goes whole.
fn without_images(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find("![") {
        let Some(end) = link_end(&rest[at + 1..]) else {
            break;
        };
        out.push_str(&rest[..at]);
        rest = &rest[at + 1 + end..];
    }
    out.push_str(rest);
    out
}

/// `[text](target)` keeps its text; a bare ` (https://…)` goes too.
fn without_link_targets(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find('[') {
        match link_end(&rest[at..]) {
            Some(end) => {
                let link = &rest[at..at + end];
                let text = &link[1..link.find("](").unwrap_or(1)];
                out.push_str(&rest[..at]);
                out.push_str(text);
                rest = &rest[at + end..];
            }
            None => {
                out.push_str(&rest[..=at]);
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    let mut bare = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while let Some(at) = rest.find("(http") {
        let Some(close) = rest[at..].find(')') else {
            break;
        };
        bare.push_str(rest[..at].trim_end());
        rest = &rest[at + close + 1..];
    }
    bare.push_str(rest);
    bare
}

/// The byte length of the `[text](target)` that `s` opens with, if it
/// opens with one.
fn link_end(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    if !s[i + 1..].starts_with('(') {
                        return None;
                    }
                    let close = s[i + 1..].find(')')?;
                    return Some(i + 1 + close + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Tags go; one that breaks a line (`<br>`, `</p>`, `<div …>`) leaves a
/// space so the words either side do not run together. Only what reads as
/// a tag — `<name`, `</name`, `<!` — so `Worf <worf@enterprise.test>`
/// keeps its address.
fn without_tags(line: &str) -> String {
    const BREAKS: &[&str] = &[
        "br", "p", "div", "li", "tr", "td", "th", "h1", "h2", "h3", "h4", "h5", "h6", "summary",
        "details",
    ];
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find('<') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let Some(name) = tag_name(after) else {
            out.push('<');
            rest = after;
            continue;
        };
        if BREAKS.contains(&name.to_ascii_lowercase().as_str()) {
            out.push(' ');
        }
        // A tag the line cut off — qmd's snippets end mid-word — goes too.
        rest = after.find('>').map_or("", |close| &after[close + 1..]);
    }
    out.push_str(rest);
    out
}

fn tag_name(after_lt: &str) -> Option<&str> {
    if after_lt.starts_with('!') {
        return Some("");
    }
    let s = after_lt.strip_prefix('/').unwrap_or(after_lt);
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or(s.len());
    let name = &s[..end];
    let next = s[end..].chars().next();
    let named = name.starts_with(|c: char| c.is_ascii_alphabetic());
    (named && matches!(next, None | Some('>' | '/' | ' ' | '\t'))).then_some(name)
}

/// `**strong**`, `__strong__`, `` `code` `` and `*emphasis*`, and a
/// backslash escape. A lone `*` with space both sides is arithmetic and
/// stays; `_emphasis_` is left alone, since snake_case is far commoner.
/// A code span's text is literal, backslashes and all.
fn without_emphasis(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let before = out.chars().next_back();
        let after = chars.get(i + 1).copied();
        match c {
            '\\' if after.is_some_and(|a| a.is_ascii_punctuation()) => {
                out.push(after.unwrap());
                i += 2;
                continue;
            }
            '`' => {
                let ticks = chars[i..].iter().take_while(|&&t| t == '`').count();
                let body = i + ticks;
                let close = (body..chars.len()).find(|&j| {
                    chars[j..].iter().take_while(|&&t| t == '`').count() == ticks
                        && chars.get(j.wrapping_sub(1)) != Some(&'`')
                });
                match close {
                    Some(end) => {
                        out.extend(&chars[body..end]);
                        i = end + ticks;
                    }
                    None => i = body,
                }
                continue;
            }
            '*' | '_' if after == Some(c) => {
                i += 2;
                continue;
            }
            '*' => {
                let opens = before.is_none_or(char::is_whitespace)
                    && after.is_some_and(|a| !a.is_whitespace());
                let closes = before.is_some_and(|b| !b.is_whitespace())
                    && after.is_none_or(|a| a.is_whitespace() || a.is_ascii_punctuation());
                if !(opens || closes) {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::plain_text;

    fn text(md: &str) -> String {
        plain_text(md, usize::MAX)
    }

    /// An email opens on its sender's logo, as an image inside a link.
    #[test]
    fn an_image_goes_whole_and_a_link_keeps_its_words() {
        assert_eq!(
            text(
                "[![Starfleet](https://sf.test/logo.png)](https://sf.test)\n\n\
                 # Your shore leave is approved\n\n\
                 Report to [Transporter Room 3](https://sf.test/tr3?a=1&amp;b=2) at **0900**."
            ),
            "Your shore leave is approved Report to Transporter Room 3 at 0900."
        );
    }

    /// Plain text a renderer escaped reads as it was typed.
    #[test]
    fn an_escaped_character_reads_as_itself() {
        assert_eq!(
            text("\\# Ops \\[1\\] snake\\_case C:\\Users &lt;b&gt;"),
            "# Ops [1] snake_case C:\\Users"
        );
    }

    /// Inside a code fence the text is what was typed: a shell command's
    /// `\(` and `'*.rs'` read as themselves.
    #[test]
    fn a_fenced_line_reads_literally() {
        assert_eq!(
            text("```\nfind . \\( -name '*.rs' \\) -o -name \"a\\|b\"\n```\nDone *now*."),
            "find . \\( -name '*.rs' \\) -o -name \"a\\|b\" Done now."
        );
    }

    /// An escaped backslash is one backslash, and an escaped `*` is not
    /// emphasis.
    #[test]
    fn an_escape_is_read_once() {
        assert_eq!(
            text("x \\\\( y \\*really\\* ***both*** `a\\|b`"),
            "x \\( y *really* both a\\|b"
        );
    }

    /// A reply's reasoning is folded on the page, and left out of the cell.
    #[test]
    fn a_folded_block_beside_other_text_reads_as_its_summary() {
        assert_eq!(
            text(
                "Plot a course.\n<details><summary>Thinking</summary>\n\n\
                 > The Neutral Zone is closer.\n\n</details>\n\nCourse laid in."
            ),
            "Plot a course. Course laid in."
        );
    }

    /// A tool call's own row is nothing but the folded block, so the
    /// block's contents are what it says.
    #[test]
    fn a_folded_tool_call_reads_as_its_summary_and_arguments() {
        assert_eq!(
            text(
                "<details><summary>Tool use: Bash</summary>\n\n```json\n\
                 {\n  \"command\": \"ls /holodeck\"\n}\n```\n\n</details>"
            ),
            "Tool use: Bash { \"command\": \"ls /holodeck\" }"
        );
    }

    #[test]
    fn quote_and_heading_marks_go_but_a_hash_word_stays() {
        assert_eq!(
            text("> > quoted\n## When\n#bridge #4 - Deck 12"),
            "quoted When #bridge #4 - Deck 12"
        );
    }

    #[test]
    fn an_address_in_angle_brackets_is_not_a_tag() {
        assert_eq!(
            text("Worf <worf@enterprise.test><br>a <b>bold</b> 3 < 4"),
            "Worf <worf@enterprise.test> a bold 3 < 4"
        );
    }

    #[test]
    fn emphasis_marks_go_and_arithmetic_stays() {
        assert_eq!(
            text("*Riker commented on his own photo.* 2 * 3 is `six`, \\[file\\] snake_case"),
            "Riker commented on his own photo. 2 * 3 is six, [file] snake_case"
        );
    }

    /// A text of several folded blocks and nothing else says what they are.
    #[test]
    fn folded_blocks_alone_read_as_their_summaries() {
        assert_eq!(
            text(
                "<details><summary>Tool use: scan</summary>\n\nbody\n\n</details>\n\
                 <details><summary>Tool result: scan</summary>\n\nbody\n\n</details>"
            ),
            "Tool use: scan Tool result: scan"
        );
    }

    /// Dividers from HTML mail and meeting invites.
    #[test]
    fn rules_and_divider_runs_go() {
        assert_eq!(
            text("Join the briefing\n* * *\n-::~:~::~:~:~::-\nBridge, 0900 -- sharp..."),
            "Join the briefing Bridge, 0900 -- sharp..."
        );
    }

    /// A line break inside an HTML block, or an indent that would open a
    /// code block, is written as a character reference and reads as a
    /// space; a typed reference stays as typed.
    #[test]
    fn a_whitespace_reference_is_a_space() {
        assert_eq!(
            text("<summary>Tool use: a&#10;&#10;b</summary>\n&#32;   code &amp;#10;"),
            "Tool use: a b code &#10;"
        );
    }

    /// Non-breaking spaces from HTML mail are spaces.
    #[test]
    fn every_run_of_whitespace_is_one_space() {
        assert_eq!(
            text("Data\u{a0}\u{a0} has\n\n\taccepted"),
            "Data has accepted"
        );
    }

    /// A body of many thousand lines is read only as far as the cell needs.
    #[test]
    fn reading_stops_past_the_limit() {
        let body = "Engage.\n".repeat(10_000);
        let out = plain_text(&body, 20);
        assert!(out.chars().count() <= 20 + "Engage.".len() + 1, "{out}");
        assert!(out.starts_with("Engage. Engage."));
    }
}
