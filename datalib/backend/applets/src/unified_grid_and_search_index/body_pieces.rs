//! A document's body as the pieces its renderer handed over, each message
//! its own string, so the UI renders each as a fragment of its own
//! (`docs/dev/data_architecture_parse_and_render.md` §"One section cannot
//! reach the next") without cutting a joined string apart again. A message
//! in a box comes as the box's class and what is inside it, and a fold as
//! a node around the pieces it holds ([`Node`]); the UI draws both.

use datalib_etl_render::section::{nodes, Node, SpelledSection};

/// The nodes of a spelled document, its front matter dropped. Front matter
/// is drawn at the head of the first piece of the frame; a document whose
/// first piece is a message, or opens a fold, has none to drop.
pub fn body_pieces(mut spelled: Vec<SpelledSection>) -> Vec<Node> {
    if let Some(first) = spelled
        .first_mut()
        .filter(|s| s.uuid.is_none() && s.fold.is_none())
    {
        first.md = strip_frontmatter(&first.md).to_string();
        if first.md.is_empty() {
            spelled.remove(0);
        }
    }
    nodes(spelled)
}

/// The body as one string: its nodes in order, each box and fold drawn.
pub fn joined(nodes: &[Node]) -> String {
    let mut out = String::new();
    for n in nodes {
        n.draw(&mut out);
    }
    out
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
    use datalib_etl_render::section::Section;

    fn piece(uuid: Option<&str>, class: Option<&str>, md: &str) -> Node {
        Node::Piece {
            uuid: uuid.map(str::to_string),
            class: class.map(str::to_string),
            md: md.to_string(),
        }
    }

    fn spelled(sections: &[Section]) -> Vec<SpelledSection> {
        sections.iter().map(SpelledSection::of).collect()
    }

    /// The pieces are the renderer's, front matter dropped, a message in
    /// its box as what is inside it, and joined they are the body an agent
    /// reads.
    #[test]
    fn the_pieces_are_the_renderers_without_the_front_matter() {
        let sections = [
            Section::unkeyed("---\ntitle: \"Ten Forward\"\n---\n\n# Ten Forward\n\n".into()),
            Section::boxed("a", "msg".into(), "Tea, Earl Grey 🫖\n\n"),
            Section::keyed("b", "<div>Make it so.</div>\n\n".into()),
        ];
        let nodes = body_pieces(spelled(&sections));
        assert_eq!(
            nodes,
            [
                piece(None, None, "\n# Ten Forward\n\n"),
                piece(Some("a"), Some("msg"), "Tea, Earl Grey 🫖\n\n"),
                piece(Some("b"), None, "<div>Make it so.</div>\n\n"),
            ]
        );
        let whole = datalib_etl_render::section::join(&sections);
        assert_eq!(
            Some(joined(&nodes).as_str()),
            whole.strip_prefix("---\ntitle: \"Ten Forward\"\n---\n")
        );
    }

    /// A message that opens with a dash rule is what it says, not front
    /// matter: only the frame's first piece can hold front matter.
    #[test]
    fn a_message_is_never_taken_for_front_matter() {
        let a = "---\nnot: front\n---\nbody\n";
        let nodes = body_pieces(spelled(&[Section::keyed("a", a.into())]));
        assert_eq!(nodes, [piece(Some("a"), None, a)]);
    }

    /// Front matter alone leaves no empty piece behind.
    #[test]
    fn a_frame_piece_of_front_matter_alone_goes() {
        let nodes = body_pieces(spelled(&[
            Section::unkeyed("---\nt: x\n---\n".into()),
            Section::keyed("a", "x".into()),
        ]));
        assert_eq!(nodes, [piece(Some("a"), None, "x")]);
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
