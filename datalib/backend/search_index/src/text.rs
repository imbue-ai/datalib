//! The text the search indexes and embeds for a document, drawn from what
//! the render store keeps of it (docs/dev/plans/messages_not_documents.md,
//! step 3b). An item is its envelope line (who, to whom, when), then its
//! body. A page, a document whose first piece is kept whole, opens with a
//! line of the facts its front matter holds, and each whole piece is drawn
//! as the words a person reads on it: no front matter, tags, link targets
//! or escapes. Plain on purpose: a change here re-embeds every chunk it
//! touches, so nothing for show goes in, and [`INDEX_TEXT_VERSION`] moves
//! with every change to what is drawn.

use crate::IndexSection;

/// Raised with every change to what [`draw`] makes of any piece, or to
/// where its chunks fall (`chunk.rs`). The index keeps the version a
/// document was drawn with and draws it again when this moves;
/// `text_tests::the_golden_draw_is_this_versions` fails when the drawing
/// changes and this does not.
pub const INDEX_TEXT_VERSION: u32 = 5;

/// What ends the key the index files a document under: the version it was
/// drawn and chunked with.
pub fn key_suffix() -> String {
    format!(".t{INDEX_TEXT_VERSION}")
}

/// One piece of a document as the render store keeps it, with the facts
/// its item's row adds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Piece {
    pub uuid: Option<String>,
    pub text: String,
    /// `text` is the piece whole, header and all: drawn as it is.
    pub framed: bool,
    pub author: Option<String>,
    /// The author as an identifier (`email:…`, `tel:…`, `slack:…`).
    pub author_handle: Option<String>,
    /// As the source wrote it: RFC 3339, its own offset or `Z`.
    pub created_at: Option<String>,
    /// Who it was addressed to, in the item's order.
    pub addressed: Vec<Addressed>,
    /// Where it is filed upstream: an email's labels and mailboxes.
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Addressed {
    /// `to`, `cc` or `bcc`.
    pub role: String,
    pub name: String,
    pub handle: Option<String>,
}

/// What a document's front matter says, as its renderer handed it, and
/// the provider tag it was rendered under.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    pub provider: String,
    /// Each key with its value, in order; a value that is `null` left out.
    pub front: Vec<(String, String)>,
}

/// The facts a page's opening line names, and as what: what the page is,
/// where it is filed, who made it, its numbers. Everything else stays
/// out: ids, hashes, paths and stamps say nothing anyone searches for, and
/// the title is already the prompt's frame.
const PAGE_FACTS: &[(&str, &str)] = &[
    ("kind", "Kind"),
    ("calendar", "Calendar"),
    ("group", "Group"),
    ("project", "Project"),
    ("repo", "Repository"),
    ("pr_number", "Pull request"),
    ("mr_iid", "Merge request"),
    ("state", "State"),
    ("author", "Author"),
    ("head_ref", "Branch"),
    ("source_branch", "Branch"),
    ("base_ref", "Into"),
    ("target_branch", "Into"),
    ("book", "Book"),
    ("chapter", "Chapter"),
    ("edition", "Edition"),
    ("language", "Language"),
    ("page_count", "Pages"),
];

/// What every chunk of a document repeats: a page's facts line. `None`
/// for a conversation, whose chunks repeat their item's envelope instead.
pub fn head(pieces: &[Piece], facts: &Facts) -> Option<String> {
    let page = pieces.first().is_some_and(|p| p.framed);
    Some(facts_line(facts)).filter(|line| page && !line.is_empty())
}

/// A document's pieces as the sections the index takes.
pub fn draw(pieces: &[Piece], facts: &Facts) -> Vec<IndexSection> {
    let mut drawn: Vec<IndexSection> = pieces
        .iter()
        .enumerate()
        .map(|(i, p)| draw_one(p, i == 0))
        .collect();
    if let Some(line) = head(pieces, facts) {
        // A page whose opening piece was its front matter alone opens its
        // first piece with words instead, so the facts share its chunk.
        let at = drawn.iter().position(|s| !s.text.is_empty()).unwrap_or(0);
        let opening = &mut drawn[at];
        opening.text = format!("{line}\n\n{}", opening.text);
    }
    drawn
}

/// `Source: GitHub`, then each fact [`PAGE_FACTS`] names, a line apiece.
fn facts_line(facts: &Facts) -> String {
    let value = |key: &str| {
        facts
            .front
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.trim())
            .filter(|v| !v.is_empty())
    };
    let source = value("source_label")
        .map(str::to_string)
        .or_else(|| Some(facts.provider.replace('_', " ")).filter(|p| !p.is_empty()));
    source
        .map(|s| format!("Source: {s}"))
        .into_iter()
        .chain(
            PAGE_FACTS
                .iter()
                .filter_map(|(key, label)| value(key).map(|v| format!("{label}: {v}"))),
        )
        .collect::<Vec<_>>()
        .join("\n")
}

fn draw_one(piece: &Piece, first: bool) -> IndexSection {
    if piece.framed {
        let text = if first {
            without_front_matter(&piece.text)
        } else {
            &piece.text
        };
        return IndexSection {
            uuid: piece.uuid.clone(),
            text: page_text(text),
            lead: None,
        };
    }
    let lead = envelope(piece);
    let labels: Vec<&str> = piece
        .labels
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    let labels = (!labels.is_empty()).then(|| format!("Labels: {}", labels.join(", ")));
    let body = flatten_links(piece.text.trim());
    let body = body.trim();
    let text: String = [lead.as_deref(), labels.as_deref(), Some(body)]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .map(|part| format!("{part}\n\n"))
        .collect();
    IndexSection {
        uuid: piece.uuid.clone(),
        text,
        lead,
    }
}

/// `Will Riker <riker@enterprise.starfleet> to Jean-Luc Picard; cc Deanna
/// Troi — Tue 3 Mar 2026 14:05`, or as much of it as is known.
fn envelope(piece: &Piece) -> Option<String> {
    let mut groups: Vec<String> = Vec::new();
    for role in ["to", "cc", "bcc"] {
        let names: Vec<String> = piece
            .addressed
            .iter()
            .filter(|a| a.role == role)
            .filter_map(|a| person(&a.name, a.handle.as_deref()))
            .collect();
        if !names.is_empty() {
            groups.push(format!("{role} {}", names.join(", ")));
        }
    }
    let author = person(
        piece.author.as_deref().unwrap_or_default(),
        piece.author_handle.as_deref(),
    )
    .unwrap_or_default();
    let who = [author, groups.join("; ")]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let when = piece.created_at.as_deref().map(when);
    match (who.is_empty(), when) {
        (false, Some(when)) => Some(format!("{who} — {when}")),
        (false, None) => Some(who),
        (true, Some(when)) => Some(when),
        (true, None) => None,
    }
}

/// A person by name, and by the address a person would type where the
/// handle is an email or a phone number: `Data <data@enterprise.starfleet>`.
/// Other handles (a Slack id) are nothing anyone types, and stay out.
fn person(name: &str, handle: Option<&str>) -> Option<String> {
    let typed = handle
        .and_then(datalib_handle::Handle::parse)
        .filter(|h| {
            matches!(
                h.kind(),
                datalib_handle::HandleKind::Email | datalib_handle::HandleKind::Tel
            )
        })
        .map(|h| h.value().to_string());
    let name = name.trim();
    let carries = |t: &str| {
        name.to_lowercase()
            .ends_with(&format!("<{}>", t.to_lowercase()))
    };
    match (name.is_empty(), typed) {
        (false, Some(t)) if t != name && !carries(&t) => Some(format!("{name} <{t}>")),
        (false, _) => Some(name.to_string()),
        (true, Some(t)) => Some(t),
        (true, None) => None,
    }
}

/// A body's links as their text: `[@Jean-Luc Picard](slack://user?… "…")`
/// says `@Jean-Luc Picard`, an image its alt text, and an email or phone
/// link `Name <address>`, the address one would type. The rest of a link
/// (an id, a URL, a hover) is for show. Code is left as written: a fenced
/// block, and an inline span.
fn flatten_links(body: &str) -> String {
    flatten(body, Mode::Item)
}

/// A piece kept whole, as its words: [`flatten_links`], and also its tags
/// gone (their text kept, a `<br>` a line break), its `&amp;`-style
/// escapes and markdown's backslash escapes read, and a table's rule rows
/// and empty header gone. Code is left as written.
fn page_text(text: &str) -> String {
    let flat = flatten(text, Mode::Page);
    let lines: Vec<&str> = flat
        .lines()
        .map(str::trim_end)
        .filter(|l| !is_table_rule(l))
        .collect();
    let mut joined = lines.join("\n");
    while joined.contains("\n\n\n") {
        joined = joined.replace("\n\n\n", "\n\n");
    }
    let joined = joined.trim();
    if joined.is_empty() {
        String::new()
    } else {
        format!("{joined}\n")
    }
}

/// `| --- | :-: |` or `| | |`: layout, not words.
fn is_table_rule(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// The YAML block a rendered document opens with, cut off: the facts line
/// says what of it is worth reading.
fn without_front_matter(text: &str) -> &str {
    text.strip_prefix("---\n")
        .and_then(|rest| rest.find("\n---\n").map(|end| &rest[end + 5..]))
        .unwrap_or(text)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// An item's body: links as their text.
    Item,
    /// A piece kept whole: links, tags and escapes as their text.
    Page,
}

fn flatten(body: &str, mode: Mode) -> String {
    let mut out = String::with_capacity(body.len());
    let mut fenced = false;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            out.push_str(line);
        } else if fenced {
            out.push_str(line);
        } else {
            flatten_line(line, &mut out, mode);
        }
    }
    out
}

fn flatten_line(line: &str, out: &mut String, mode: Mode) {
    let triggers: &[char] = match mode {
        Mode::Item => &['`', '['],
        Mode::Page => &['`', '[', '<', '\\'],
    };
    let mut rest = line;
    while let Some(at) = rest.find(triggers) {
        let (before, from) = rest.split_at(at);
        push_words(before, out, mode);
        if let Some(after) = from.strip_prefix('<') {
            rest = match tag_at(from) {
                Some(tag) => {
                    out.push_str(tag.says);
                    &from[tag.len..]
                }
                None => {
                    out.push('<');
                    after
                }
            };
            continue;
        }
        if let Some(escaped) = from.strip_prefix('\\') {
            match escaped.chars().next().filter(char::is_ascii_punctuation) {
                Some(c) => {
                    out.push(c);
                    rest = &escaped[1..];
                }
                None => {
                    out.push('\\');
                    rest = escaped;
                }
            }
            continue;
        }
        if from.starts_with('`') {
            let ticks = from.len() - from.trim_start_matches('`').len();
            let fence = &from[..ticks];
            match from[ticks..].find(fence) {
                Some(end) => {
                    out.push_str(&from[..ticks + end + ticks]);
                    rest = &from[ticks + end + ticks..];
                }
                None => {
                    out.push_str(from);
                    return;
                }
            }
            continue;
        }
        match link_at(from) {
            Some((text, dest, len)) => {
                if out.ends_with('!') {
                    out.pop();
                }
                match mode {
                    Mode::Item => out.push_str(text),
                    Mode::Page => flatten_line(text, out, mode),
                }
                let typed = dest
                    .strip_prefix("mailto:")
                    .or_else(|| dest.strip_prefix("tel:"))
                    .filter(|a| !a.is_empty() && *a != text);
                if let Some(address) = typed {
                    out.push_str(&format!(" <{address}>"));
                }
                rest = &from[len..];
            }
            None => {
                out.push('[');
                rest = &from[1..];
            }
        }
    }
    push_words(rest, out, mode);
}

/// Text outside code, links and tags; on a page with its `&amp;`-style
/// references read, a no-break space as a space.
fn push_words(text: &str, out: &mut String, mode: Mode) {
    match mode {
        Mode::Item => out.push_str(text),
        Mode::Page => out.push_str(&datalib_html_entities::decode(text).replace('\u{a0}', " ")),
    }
}

struct Tag {
    /// What the tag says in the text: nothing; a line break for `<br>`;
    /// `: ` after a `<dt>`'s term, which its `<dd>` defines.
    says: &'static str,
    len: usize,
}

/// The HTML tag `s` opens with: `<name …>`, `</name>` or `<!-- … -->`. A
/// `<` that opens no tag (`<https://…>`, `<picard@…>`, `< 9`) is `None`.
/// `<a class="source-link" …>↗</a>`, the page's link to its upstream, goes
/// whole.
fn tag_at(s: &str) -> Option<Tag> {
    if let Some(comment) = s.strip_prefix("<!--") {
        let end = comment.find("-->")?;
        return Some(Tag {
            says: "",
            len: 4 + end + 3,
        });
    }
    let name = s[1..].strip_prefix('/').unwrap_or(&s[1..]);
    let name_len = name
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(name.len());
    if name_len == 0 || !name.as_bytes()[0].is_ascii_alphabetic() {
        return None;
    }
    if !matches!(
        name[name_len..].chars().next(),
        Some('>' | '/' | ' ' | '\t')
    ) {
        return None;
    }
    let close = s.find('>')?;
    let tag = &s[..=close];
    if tag.starts_with("<a ") && tag.contains("source-link") {
        let end = s.find("</a>").map(|e| e + 4).unwrap_or(close + 1);
        return Some(Tag { says: "", len: end });
    }
    let closing = s[1..].starts_with('/');
    let says = match name[..name_len].to_ascii_lowercase().as_str() {
        "br" => "\n",
        "dt" if closing => ": ",
        _ => "",
    };
    Some(Tag {
        says,
        len: close + 1,
    })
}

/// The markdown link `s` opens with, `[text](dest "title")`: its text, its
/// destination and its length. `None` for a `[` that opens no link.
fn link_at(s: &str) -> Option<(&str, &str, usize)> {
    let mut depth = 0;
    let mut close = None;
    let mut chars = s.char_indices().skip(1);
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '[' => depth += 1,
            ']' if depth == 0 => {
                close = Some(i);
                break;
            }
            ']' => depth -= 1,
            _ => {}
        }
    }
    let close = close?;
    let after = &s[close + 1..];
    if !after.starts_with('(') {
        return None;
    }
    let mut quoted = false;
    let mut end = None;
    let mut chars = after.char_indices().skip(1);
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' => quoted = !quoted,
            ')' if !quoted => {
                end = Some(i);
                break;
            }
            '\n' => return None,
            _ => {}
        }
    }
    let end = end?;
    let inside = after[1..end].trim();
    let dest = inside
        .strip_prefix('<')
        .and_then(|d| d.split_once('>').map(|(d, _)| d))
        .unwrap_or_else(|| inside.split_whitespace().next().unwrap_or(""));
    Some((&s[1..close], dest, close + 1 + end + 1))
}

/// The weekday and month as words, on the clock the source wrote: a search
/// for "a Tuesday in March" has words to meet, and the same stamp draws
/// the same way on every machine. A stamp that does not parse is kept as
/// written.
fn when(stamp: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(stamp) {
        Ok(t) => t.format("%a %-d %b %Y %H:%M").to_string(),
        Err(_) => stamp.to_string(),
    }
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod text_tests;
