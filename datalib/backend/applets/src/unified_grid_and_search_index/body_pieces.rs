//! A document's body as the pieces its renderer handed over, each message
//! its own string, so the UI renders each as a fragment of its own
//! (`docs/dev/data_architecture_parse_and_render.md` §"One section cannot
//! reach the next") without cutting a joined string apart again. A message
//! in a box comes as the box's class and what is inside it, and the UI
//! draws the box.

use datalib_etl_render::section::{draw_box, inside_box, SpelledSection};
use serde::Serialize;

/// One piece of a document's body: the message it is, or none for a piece
/// of the page's frame (a title, a `<details>` opener), and its markdown:
/// what is inside its box where it has one (`class`), else all of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BodyPiece {
    pub uuid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    pub md: String,
}

impl BodyPiece {
    fn of(s: SpelledSection) -> Self {
        let inside = match (&s.uuid, &s.class) {
            (Some(uuid), Some(class)) => match inside_box(uuid, class, &s.md) {
                Some(inside) => Some(inside.to_string()),
                None => {
                    // The store keeps the drawing a renderer made, and only
                    // `Section::boxed` names a class, so this is a bug.
                    tracing::warn!(%uuid, %class, "a piece is not in the box it names; sent whole");
                    None
                }
            },
            _ => None,
        };
        match inside {
            Some(md) => Self {
                uuid: s.uuid,
                class: s.class,
                md,
            },
            None => Self {
                uuid: s.uuid,
                class: None,
                md: s.md,
            },
        }
    }

    /// The piece as markdown, its box drawn around it where it has one.
    pub fn drawn(&self) -> std::borrow::Cow<'_, str> {
        match (&self.uuid, &self.class) {
            (Some(uuid), Some(class)) => draw_box(uuid, class, &self.md).into(),
            _ => self.md.as_str().into(),
        }
    }
}

/// The pieces of a spelled document, its front matter dropped. Front
/// matter is drawn at the head of the first piece of the frame; a document
/// whose first piece is a message has none to drop.
pub fn body_pieces(spelled: Vec<SpelledSection>) -> Vec<BodyPiece> {
    let mut pieces: Vec<BodyPiece> = spelled.into_iter().map(BodyPiece::of).collect();
    if let Some(first) = pieces.first_mut().filter(|p| p.uuid.is_none()) {
        first.md = strip_frontmatter(&first.md).to_string();
        if first.md.is_empty() {
            pieces.remove(0);
        }
    }
    pieces
}

/// The body as one string: its pieces in order, each box drawn.
pub fn joined(pieces: &[BodyPiece]) -> String {
    pieces.iter().map(|p| p.drawn()).collect()
}

/// Strip a leading YAML front-matter block: a `---` line, then everything
/// up to the next line that is `---` by itself. Text trimming, not
/// parsing; a line that only starts with `---` closes nothing.
fn strip_frontmatter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text;
    };
    let mut at = 0;
    for line in rest.split_inclusive('\n') {
        at += line.len();
        if line.trim_end_matches(['\n', '\r']) == "---" {
            return &rest[at..];
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spelled(uuid: Option<&str>, md: &str) -> SpelledSection {
        SpelledSection {
            uuid: uuid.map(str::to_string),
            md: md.to_string(),
            class: None,
        }
    }

    fn piece(uuid: Option<&str>, class: Option<&str>, md: &str) -> BodyPiece {
        BodyPiece {
            uuid: uuid.map(str::to_string),
            class: class.map(str::to_string),
            md: md.to_string(),
        }
    }

    /// A message in its box goes out as the box's class and what is inside
    /// it, and joined with its box drawn it is the message as rendered.
    #[test]
    fn a_boxed_message_goes_out_as_what_is_inside_its_box() {
        let boxed = datalib_etl_render::section::Section::boxed("a", "msg".into(), "Engage.\n");
        let pieces = body_pieces(vec![SpelledSection {
            uuid: boxed.uuid.clone(),
            md: boxed.md.clone(),
            class: boxed.class.clone(),
        }]);
        assert_eq!(pieces, [piece(Some("a"), Some("msg"), "Engage.\n")]);
        assert_eq!(joined(&pieces), boxed.md);
    }

    /// A drawing that is not in the box its class names goes out whole,
    /// with no class, rather than cut somewhere wrong.
    #[test]
    fn a_piece_not_in_its_box_goes_out_whole() {
        let pieces = body_pieces(vec![SpelledSection {
            uuid: Some("a".into()),
            md: "<div>Engage.</div>\n".into(),
            class: Some("msg".into()),
        }]);
        assert_eq!(pieces, [piece(Some("a"), None, "<div>Engage.</div>\n")]);
    }

    /// The pieces are the renderer's, front matter dropped, and joined they
    /// are the body an agent reads.
    #[test]
    fn the_pieces_are_the_renderers_without_the_front_matter() {
        let front = "---\ntitle: \"Ten Forward\"\n---\n\n# Ten Forward\n\n";
        let a = "<div id=\"m-a\" data-section-uuid=\"a\" class=\"msg\">\n\nTea, Earl Grey 🫖\n\n</div>\n\n";
        let b =
            "<div id=\"m-b\" data-section-uuid=\"b\" class=\"msg\">\n\nMake it so.\n\n</div>\n\n";
        let pieces = body_pieces(vec![
            spelled(None, front),
            spelled(Some("a"), a),
            spelled(Some("b"), b),
        ]);
        assert_eq!(
            pieces,
            [
                piece(None, None, "\n# Ten Forward\n\n"),
                piece(Some("a"), None, a),
                piece(Some("b"), None, b),
            ]
        );
        assert_eq!(joined(&pieces), format!("\n# Ten Forward\n\n{a}{b}"));
    }

    /// A message that opens with a dash rule is what it says, not front
    /// matter: only the frame's first piece can hold front matter.
    #[test]
    fn a_message_is_never_taken_for_front_matter() {
        let a = "---\nnot: front\n---\nbody\n";
        let pieces = body_pieces(vec![spelled(Some("a"), a)]);
        assert_eq!(pieces[0].md, a);
    }

    /// Front matter alone leaves no empty piece behind.
    #[test]
    fn a_frame_piece_of_front_matter_alone_goes() {
        let pieces = body_pieces(vec![
            spelled(None, "---\nt: x\n---\n"),
            spelled(Some("a"), "x"),
        ]);
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].uuid.as_deref(), Some("a"));
    }

    /// Frontmatter trimming is text handling, not parsing — a body
    /// without it is returned unchanged rather than treated as broken.
    #[test]
    fn front_matter_is_trimmed_as_text() {
        assert_eq!(strip_frontmatter("---\ntitle: x\n---\nbody\n"), "body\n");
        assert_eq!(strip_frontmatter("body only\n"), "body only\n");
        assert_eq!(
            strip_frontmatter("---\nunterminated\n"),
            "---\nunterminated\n"
        );
        assert_eq!(strip_frontmatter("---\n---\n\nbody"), "\nbody");
    }

    /// Only a `---` line by itself closes the block; one that starts a
    /// value used to, and the rest of the front matter became body
    /// (#992).
    #[test]
    fn a_dash_run_inside_the_front_matter_closes_nothing() {
        assert_eq!(
            strip_frontmatter("---\ntitle: a\n---b\n---x: y\n---\n\nbody\n"),
            "\nbody\n"
        );
    }
}
