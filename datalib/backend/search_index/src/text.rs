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
pub const INDEX_TEXT_VERSION: u32 = 1;

/// One piece of a document as the render store keeps it, with the facts
/// its item's row adds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Piece {
    pub uuid: Option<String>,
    pub text: String,
    /// `text` is the piece whole, header and all: drawn as it is.
    pub framed: bool,
    pub author: Option<String>,
    /// As the source wrote it: RFC 3339, its own offset or `Z`.
    pub created_at: Option<String>,
    /// `(role, name)`, role `to`, `cc` or `bcc`, in the item's order.
    pub addressed: Vec<(String, String)>,
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
    let body = piece.text.trim();
    let text = match (&lead, body.is_empty()) {
        (Some(lead), false) => format!("{lead}\n\n{body}\n\n"),
        (Some(lead), true) => format!("{lead}\n\n"),
        (None, false) => format!("{body}\n\n"),
        (None, true) => String::new(),
    };
    IndexSection {
        uuid: piece.uuid.clone(),
        text,
        lead,
    }
}

/// `Will Riker to Jean-Luc Picard; cc Deanna Troi — Tue 3 Mar 2026 14:05`,
/// or as much of it as is known.
fn envelope(piece: &Piece) -> Option<String> {
    let mut groups: Vec<String> = Vec::new();
    for role in ["to", "cc", "bcc"] {
        let names: Vec<&str> = piece
            .addressed
            .iter()
            .filter(|(r, name)| r == role && !name.trim().is_empty())
            .map(|(_, name)| name.trim())
            .collect();
        if !names.is_empty() {
            groups.push(format!("{role} {}", names.join(", ")));
        }
    }
    let author = piece.author.as_deref().map(str::trim).unwrap_or_default();
    let who = [author.to_string(), groups.join("; ")]
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
