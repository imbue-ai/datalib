//! Shared "section anchor" helpers for provider renderers, and the
//! [`Section`] a renderer hands over so a document can be read piece by
//! piece without parsing its markdown.

use datalib_schema::markdowns::SectionSpan;
use datalib_schema::providers::Provider;
use datalib_schema::sections::{addressed_json, labels_json, Addressee, SectionRow};

/// One piece of a rendered document, in document order. `uuid` is the
/// piece's `data-section-uuid` — a message, a contact — so a row of the
/// grid names the piece it came from; `None` for a piece that belongs
/// to no row: the frontmatter and title, a `<details>` opener around a
/// run of asides. Concatenated in order, a document's sections are its
/// `.md` byte for byte ([`join`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub uuid: Option<String>,
    pub md: String,
    /// What the render store keeps of it (`sections`).
    pub kept: Kept,
}

/// What of a section the render store keeps, for whatever draws it
/// again: the viewer, an export, the search index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kept {
    /// The piece whole, as drawn, marked framed: from a renderer that has
    /// not split an item's body off its header, or a piece of no item's
    /// that still says something (a PDF's text).
    Whole,
    /// An item's body, without the header and wrapper `md` draws around
    /// it, and who the item was addressed to where it says so itself.
    Body {
        text: String,
        addressed: Vec<Addressee>,
        labels: Vec<String>,
    },
    /// Nothing: layout alone (front matter, a `<details>` opener), drawn
    /// again from the document's facts.
    Frame,
}

impl Section {
    pub fn keyed(uuid: &str, md: String) -> Self {
        Self {
            uuid: Some(uuid.to_string()),
            md,
            kept: Kept::Whole,
        }
    }

    /// An item's section that also says which part of it is the body.
    pub fn item(uuid: &str, md: String, text: String, addressed: Vec<Addressee>) -> Self {
        Self {
            kept: Kept::Body {
                text,
                addressed,
                labels: Vec::new(),
            },
            ..Self::keyed(uuid, md)
        }
    }

    /// The item's labels, where it is filed upstream; nothing for a piece
    /// that is no item's body.
    pub fn labelled(mut self, with: Vec<String>) -> Self {
        if let Kept::Body { labels, .. } = &mut self.kept {
            *labels = with;
        }
        self
    }

    pub fn unkeyed(md: String) -> Self {
        Self {
            uuid: None,
            md,
            kept: Kept::Whole,
        }
    }

    /// A piece that is layout alone, which the store does not keep.
    pub fn frame(md: String) -> Self {
        Self {
            kept: Kept::Frame,
            ..Self::unkeyed(md)
        }
    }
}

/// The rows the render store keeps of a document's sections, each at its
/// place among all of them.
pub fn stored(markdown_uuid: &str, sections: &[Section]) -> Vec<SectionRow> {
    sections
        .iter()
        .enumerate()
        .filter_map(|(seq, s)| {
            let (text, framed, addressed, labels) = match &s.kept {
                Kept::Frame => return None,
                Kept::Whole => (s.md.clone(), true, None, None),
                Kept::Body {
                    text,
                    addressed,
                    labels,
                } => (
                    text.clone(),
                    false,
                    addressed_json(addressed),
                    labels_json(labels),
                ),
            };
            Some(SectionRow {
                markdown_uuid: markdown_uuid.to_string(),
                seq: seq as i64,
                uuid: s.uuid.clone(),
                text,
                framed,
                addressed,
                labels,
            })
        })
        .collect()
}

/// The `.md` a list of sections spells.
pub fn join(sections: &[Section]) -> String {
    let mut out = String::with_capacity(sections.iter().map(|s| s.md.len()).sum());
    for s in sections {
        out.push_str(&s.md);
    }
    out
}

/// blake3 of the `.md` a list of sections spells, as hex:
/// `markdowns.content_hash`.
pub fn content_hash(sections: &[Section]) -> String {
    let mut hasher = blake3::Hasher::new();
    for s in sections {
        hasher.update(s.md.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// Where each section lies in the `.md` they spell: what
/// `markdowns.section_layout` stores, so the UI can render each message
/// as a fragment of its own without trusting the text to say where one
/// ends.
pub fn layout(sections: &[Section]) -> Vec<SectionSpan> {
    sections
        .iter()
        .map(|s| SectionSpan {
            uuid: s.uuid.clone(),
            bytes: s.md.len(),
        })
        .collect()
}

/// A document written top to bottom as text, with the pieces that wrap a
/// row's `data-section-uuid` handed over keyed and the text between them
/// unkeyed: the [`Section`]s a page made this way is.
#[derive(Debug, Default)]
pub struct Pieces {
    done: Vec<Section>,
    open: String,
}

impl Pieces {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_str(&mut self, s: &str) {
        self.open.push_str(s);
    }

    pub fn push(&mut self, c: char) {
        self.open.push(c);
    }

    pub fn keyed(&mut self, uuid: &str, md: String) {
        self.flush();
        self.done.push(Section::keyed(uuid, md));
    }

    pub fn into_sections(mut self) -> Vec<Section> {
        self.flush();
        self.done
    }

    fn flush(&mut self) {
        if !self.open.is_empty() {
            self.done
                .push(Section::unkeyed(std::mem::take(&mut self.open)));
        }
    }
}

impl std::fmt::Write for Pieces {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.open.push_str(s);
        Ok(())
    }
}

/// HTML attribute fragment that anchors a navigable section.
/// Identical shape across providers: `id="m-{uuid}"` for in-page
/// `#anchor` links and `data-section-uuid="{uuid}"` for the frontend's
/// row→preview lookup.
pub fn section_attrs(uuid: &str) -> String {
    format!(r#"id="m-{uuid}" data-section-uuid="{uuid}""#)
}

/// Opening tag for a per-message wrapper div. Pair with
/// [`MSG_DIV_CLOSE`]. `provider` tags the element so per-provider CSS
/// (avatar, accent color, etc.) can apply without each renderer
/// inventing its own class scheme.
pub fn msg_div_open(msg_uuid: &str, provider: Provider) -> String {
    msg_div_open_with(msg_uuid, provider, &[])
}

/// [`msg_div_open`] with further classes after the two it always has.
pub fn msg_div_open_with(msg_uuid: &str, provider: Provider, classes: &[&str]) -> String {
    let mut class = format!("msg msg--{provider}");
    for c in classes {
        class.push(' ');
        class.push_str(c);
    }
    format!(
        r#"<div {attrs} class="{class}">"#,
        attrs = section_attrs(msg_uuid),
    )
}

/// Closes a div opened with [`msg_div_open`]. A constant rather than
/// a function so renderers can hard-code it in `const`-evaluable
/// contexts; the close tag has no per-message state to thread in.
pub const MSG_DIV_CLOSE: &str = "</div>";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joined_sections_are_the_document() {
        let sections = vec![
            Section::unkeyed("---\n---\n\n".into()),
            Section::keyed("a", "<div>a</div>\n".into()),
            Section::keyed("b", "<div>b</div>\n".into()),
        ];
        assert_eq!(join(&sections), "---\n---\n\n<div>a</div>\n<div>b</div>\n");
        assert_eq!(sections[0].uuid, None);
        assert_eq!(sections[1].uuid.as_deref(), Some("a"));
    }

    #[test]
    fn a_layout_spans_the_joined_document() {
        let sections = vec![
            Section::unkeyed("---\n---\n\n".into()),
            Section::keyed("a", "<div>é</div>\n".into()),
        ];
        let spans = layout(&sections);
        assert_eq!(
            spans.iter().map(|s| s.bytes).sum::<usize>(),
            join(&sections).len()
        );
        assert_eq!(spans[1].uuid.as_deref(), Some("a"));
        assert_eq!(spans[1].bytes, "<div>é</div>\n".len(), "bytes, not chars");
    }

    /// A piece is kept by its place among all of them: an item's body
    /// unframed with its addressees, a piece not split off whole and
    /// framed, and layout not at all.
    #[test]
    fn the_store_keeps_bodies_and_whole_pieces_but_not_layout() {
        use datalib_schema::sections::AddressRole;
        let to = vec![Addressee {
            role: AddressRole::To,
            name: "Picard".into(),
            handle: None,
        }];
        let sections = vec![
            Section::frame("# Title\n".into()),
            Section::item(
                "a",
                "<div>Riker: hi</div>\n".into(),
                "hi".into(),
                to.clone(),
            )
            .labelled(vec!["Bridge".into()]),
            Section::keyed("b", "<div>Data: yes</div>\n".into()).labelled(vec!["ignored".into()]),
            Section::unkeyed("Page 2 text\n".into()),
        ];
        let row = |seq: i64, uuid: Option<&str>, text: &str, framed: bool, addressed, labels| {
            SectionRow {
                markdown_uuid: "doc".into(),
                seq,
                uuid: uuid.map(String::from),
                text: text.into(),
                framed,
                addressed,
                labels,
            }
        };
        assert_eq!(
            stored("doc", &sections),
            vec![
                row(
                    1,
                    Some("a"),
                    "hi",
                    false,
                    addressed_json(&to),
                    labels_json(&["Bridge".into()])
                ),
                row(2, Some("b"), "<div>Data: yes</div>\n", true, None, None),
                row(3, None, "Page 2 text\n", true, None, None),
            ]
        );
    }

    #[test]
    fn keyed_pieces_split_the_text_around_them() {
        use std::fmt::Write as _;
        let mut p = Pieces::new();
        p.push_str("head\n");
        p.keyed("a", "<div>a</div>\n".into());
        p.keyed("b", "<div>b</div>\n".into());
        let _ = writeln!(p, "tail");
        let sections = p.into_sections();
        let uuids: Vec<_> = sections.iter().map(|s| s.uuid.as_deref()).collect();
        assert_eq!(uuids, [None, Some("a"), Some("b"), None]);
        assert_eq!(join(&sections), "head\n<div>a</div>\n<div>b</div>\ntail\n");
        assert!(Pieces::new().into_sections().is_empty());
    }

    #[test]
    fn attrs_shape() {
        assert_eq!(
            section_attrs("abc-123"),
            r#"id="m-abc-123" data-section-uuid="abc-123""#
        );
    }

    #[test]
    fn div_open_shape() {
        assert_eq!(
            msg_div_open("abc", Provider::Slack),
            r#"<div id="m-abc" data-section-uuid="abc" class="msg msg--slack">"#
        );
    }
}
