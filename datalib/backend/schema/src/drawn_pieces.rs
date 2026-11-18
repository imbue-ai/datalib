// How each piece of a rendered document is drawn, where `sections` does not
// already hold it as drawn: a chat message's wrapper, header and reactions
// around its body, and the layout between items (front matter, the title, a
// `<details>` opener). With `sections` it spells the document byte for byte,
// so nothing has to keep the document as a file
// (docs/dev/plans/documents_to_blocks.md, step 5). Held by the render store
// alone.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// One piece's drawing, carried by its document. A piece with a row here
/// and in `sections` is `md` with the section's `text` put back at
/// `body_at`; a piece only here is `md`; a piece only in `sections` is its
/// `text`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(table = "drawn_pieces", primary_key = "markdown_uuid, seq")]
pub struct DrawnPieceRow {
    #[col(sql = "VARCHAR(96)")]
    pub markdown_uuid: String,
    /// Its place in the document, as in `sections`.
    #[col(sql = "INTEGER")]
    pub seq: i64,
    /// The piece as drawn, with its body cut out where `body_at` says.
    #[col(sql = "TEXT")]
    pub md: String,
    /// Where in `md`, in bytes, the piece's `sections.text` goes back in.
    /// Null when `md` is the whole piece: layout alone, or a body that is
    /// not drawn verbatim inside its piece.
    #[col(sql = "INTEGER")]
    pub body_at: Option<i64>,
}

impl DrawnPieceRow {
    /// The piece spelled out: `md`, with `body` put back where it was cut.
    /// `None` when `body_at` does not fall on a character boundary of `md`,
    /// which a row this build wrote never does.
    pub fn spell(&self, body: Option<&str>) -> Option<String> {
        match (self.body_at, body) {
            (Some(at), Some(body)) => {
                let at = usize::try_from(at).ok()?;
                let (before, after) = (self.md.get(..at)?, self.md.get(at..)?);
                Some(format!("{before}{body}{after}"))
            }
            _ => Some(self.md.clone()),
        }
    }
}

/// How to keep `drawn`, a piece whose stored body is `body`: cut the body
/// out where it first appears verbatim, or keep the piece whole. `None`
/// when the piece is its body exactly, so `sections` alone spells it.
pub fn cut(drawn: &str, body: Option<&str>) -> Option<(String, Option<i64>)> {
    match body {
        Some(body) if body == drawn => None,
        Some(body) if !body.is_empty() => match drawn.find(body) {
            Some(at) => Some((
                format!("{}{}", &drawn[..at], &drawn[at + body.len()..]),
                Some(at as i64),
            )),
            None => Some((drawn.to_string(), None)),
        },
        _ => Some((drawn.to_string(), None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(md: &str, body_at: Option<i64>) -> DrawnPieceRow {
        DrawnPieceRow {
            markdown_uuid: "doc".into(),
            seq: 0,
            md: md.into(),
            body_at,
        }
    }

    /// A body drawn verbatim is cut out and put back; one that is not, or a
    /// piece of layout alone, is kept whole; a piece that is its body
    /// needs no drawing.
    #[test]
    fn a_piece_is_spelled_back_from_its_cut() {
        let drawn = "<div id=\"m-1\">\n\n**Riker** — 09:00\n\nEngage.\n\n</div>\n\n";
        let (md, at) = cut(drawn, Some("Engage.")).unwrap();
        assert_eq!(at, drawn.find("Engage.").map(|i| i as i64));
        assert!(!md.contains("Engage."));
        assert_eq!(row(&md, at).spell(Some("Engage.")).unwrap(), drawn);

        let (md, at) = cut(drawn, Some("Engage.\n\n🖖 attachment")).unwrap();
        assert_eq!((md.as_str(), at), (drawn, None));
        assert_eq!(row(&md, at).spell(Some("ignored")).unwrap(), drawn);

        let (md, at) = cut("---\ntitle: Bridge\n---\n", None).unwrap();
        assert_eq!(
            row(&md, at).spell(None).unwrap(),
            "---\ntitle: Bridge\n---\n"
        );

        assert_eq!(cut("Whole piece.", Some("Whole piece.")), None);
    }

    /// A body cut at a byte that is no character boundary is refused, not
    /// spliced into broken text.
    #[test]
    fn a_cut_off_a_character_boundary_spells_nothing() {
        assert_eq!(row("é", Some(1)).spell(Some("x")), None);
    }
}
