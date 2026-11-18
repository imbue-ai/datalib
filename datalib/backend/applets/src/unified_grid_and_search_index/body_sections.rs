//! Where each section of a document's body lies, as the UI counts it: the
//! layout render stored for the file, cut to the body the route sends
//! (its front matter dropped) and measured in UTF-16 code units, the way
//! JavaScript indexes a string. The UI renders each message from it as a
//! fragment of its own (`docs/dev/data_architecture_parse_and_render.md`
//! §"One section cannot reach the next").

use datalib_schema::markdowns::SectionSpan;
use serde::Serialize;

/// One section of the body: the `data-section-uuid` it wraps, or none for
/// a piece of the page's frame, and its length in UTF-16 code units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BodySection {
    pub uuid: Option<String>,
    pub len: usize,
}

/// The sections of `file[skip..]`. `None` when the layout does not spell
/// `file` — a render rewrote the file after the index was read — or when
/// the front matter would cut into a message; the body is then drawn
/// whole.
pub fn body_sections(file: &str, skip: usize, layout: &[SectionSpan]) -> Option<Vec<BodySection>> {
    let total: usize = layout.iter().map(|s| s.bytes).sum();
    if total != file.len() || skip > file.len() {
        return None;
    }
    let mut out = Vec::with_capacity(layout.len());
    let mut start = 0;
    for span in layout {
        let end = start + span.bytes;
        if !file.is_char_boundary(end) {
            return None;
        }
        let from = start.max(skip);
        if span.uuid.is_some() && from > start {
            return None;
        }
        if from < end || span.uuid.is_some() {
            out.push(BodySection {
                uuid: span.uuid.clone(),
                len: file[from..end].encode_utf16().count(),
            });
        }
        start = end;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(uuid: Option<&str>, text: &str) -> SectionSpan {
        SectionSpan {
            uuid: uuid.map(str::to_string),
            bytes: text.len(),
        }
    }

    fn sliced(body: &str, sections: &[BodySection]) -> Vec<String> {
        let units: Vec<u16> = body.encode_utf16().collect();
        let mut at = 0;
        sections
            .iter()
            .map(|s| {
                let piece = String::from_utf16(&units[at..at + s.len]).unwrap();
                at += s.len;
                piece
            })
            .collect()
    }

    /// The pieces the UI cuts are the renderer's pieces, front matter
    /// dropped, however many UTF-16 units a character takes.
    #[test]
    fn the_ui_cuts_the_body_where_the_renderer_put_its_pieces() {
        let front = "---\ntitle: \"Ten Forward\"\n---\n\n# Ten Forward\n\n";
        let a = "<div id=\"m-a\" data-section-uuid=\"a\" class=\"msg\">\n\nTea, Earl Grey 🫖\n\n</div>\n\n";
        let b =
            "<div id=\"m-b\" data-section-uuid=\"b\" class=\"msg\">\n\nMake it so.\n\n</div>\n\n";
        let file = format!("{front}{a}{b}");
        let layout = [span(None, front), span(Some("a"), a), span(Some("b"), b)];
        let skip = "---\ntitle: \"Ten Forward\"\n---\n".len();
        let body = &file[skip..];
        let sections = body_sections(&file, skip, &layout).unwrap();
        assert_eq!(
            sections
                .iter()
                .map(|s| s.uuid.as_deref())
                .collect::<Vec<_>>(),
            [None, Some("a"), Some("b")]
        );
        assert_eq!(sliced(body, &sections), ["\n# Ten Forward\n\n", a, b]);
    }

    /// A layout from before the file was rewritten would cut messages in
    /// the wrong places; the body is sent without one.
    #[test]
    fn a_layout_that_does_not_spell_the_file_is_not_sent() {
        let file = "<div>a</div>\n";
        assert_eq!(
            body_sections(file, 0, &[span(Some("a"), "<div>a</div>")]),
            None
        );
        let wide = [SectionSpan {
            uuid: Some("a".into()),
            bytes: 3,
        }];
        assert_eq!(
            body_sections("é€", 0, &wide),
            None,
            "not on a char boundary"
        );
    }

    /// Front matter that ran into a message would leave half a wrapper.
    #[test]
    fn front_matter_may_not_cut_into_a_message() {
        let file = "---\n---\n<div>a</div>\n";
        let layout = [span(Some("a"), file)];
        assert_eq!(body_sections(file, 8, &layout), None);
        assert!(body_sections(file, 0, &layout).is_some());
    }
}
