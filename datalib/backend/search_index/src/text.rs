//! The text the search indexes and embeds for a document, drawn from what
//! the render store keeps of it (docs/dev/plans/messages_not_documents.md,
//! step 3b). An item is its envelope line (who, to whom, when), then its
//! body; a piece kept whole is drawn as it is. Plain on purpose: a change
//! here re-embeds every chunk it touches, so nothing for show goes in, and
//! [`INDEX_TEXT_VERSION`] moves with every change to what is drawn.

use crate::IndexSection;

/// Raised with every change to what [`draw`] makes of any piece. The
/// index keeps the version a document was drawn with and draws it again
/// when this moves; `text_tests::the_golden_draw_is_this_versions` fails
/// when the drawing changes and this does not.
pub const INDEX_TEXT_VERSION: u32 = 3;

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

/// A document's pieces as the sections the index takes.
pub fn draw(pieces: &[Piece]) -> Vec<IndexSection> {
    pieces.iter().map(draw_one).collect()
}

fn draw_one(piece: &Piece) -> IndexSection {
    if piece.framed {
        return IndexSection {
            uuid: piece.uuid.clone(),
            text: piece.text.clone(),
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
            flatten_line(line, &mut out);
        }
    }
    out
}

fn flatten_line(line: &str, out: &mut String) {
    let mut rest = line;
    while let Some(at) = rest.find(['`', '[']) {
        let (before, from) = rest.split_at(at);
        out.push_str(before);
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
                out.push_str(text);
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
    out.push_str(rest);
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
