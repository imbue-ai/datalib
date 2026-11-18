//! Shared "section anchor" helpers for provider renderers, and the
//! [`Section`] a renderer hands over so a document can be read section by
//! section without parsing its markdown.

use datalib_schema::drawings::{self, DrawingRow};
use datalib_schema::providers::Provider;
use datalib_schema::sections::{addressed_json, labels_json, Addressee, SectionRow};

/// One section of a rendered document, in document order. `uuid` is the
/// section's `data-section-uuid` — a message, a contact — so a row of the
/// grid names the section it came from; `None` for a section that belongs
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

/// What of a section the render store keeps in `sections`, for whatever
/// draws it again: the viewer, an export, the search index. What it does
/// not keep there as drawn, `drawings` keeps ([`drawn`]).
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
    /// No section: layout alone (front matter, a `<details>` opener),
    /// kept whole as its drawing.
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

/// How the store keeps what `sections` does not hold as drawn: each
/// message's drawing with its body cut out, and each piece of layout
/// whole. With [`stored`] it spells the document back ([`spell`]).
pub fn drawn(markdown_uuid: &str, sections: &[Section]) -> Vec<DrawingRow> {
    sections
        .iter()
        .enumerate()
        .filter_map(|(seq, s)| {
            let body = match &s.kept {
                Kept::Whole => return None,
                Kept::Body { text, .. } => Some(text.as_str()),
                Kept::Frame => None,
            };
            let (md, body_at) = drawings::cut(&s.md, body)?;
            Some(DrawingRow {
                markdown_uuid: markdown_uuid.to_string(),
                seq: seq as i64,
                md,
                body_at,
            })
        })
        .collect()
}

/// One section of a document as the store spells it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpelledSection {
    /// The item it is; `None` for a section that is no item's.
    pub uuid: Option<String>,
    pub md: String,
}

/// A document spelled back from what the store keeps of it, section by
/// section in order. `None` when the rows cannot spell it: a drawing whose
/// cut falls inside a character.
pub fn spell(sections: &[SectionRow], drawn: &[DrawingRow]) -> Option<Vec<SpelledSection>> {
    let mut by_seq: std::collections::BTreeMap<i64, (Option<&SectionRow>, Option<&DrawingRow>)> =
        std::collections::BTreeMap::new();
    for s in sections {
        by_seq.entry(s.seq).or_default().0 = Some(s);
    }
    for d in drawn {
        by_seq.entry(d.seq).or_default().1 = Some(d);
    }
    by_seq
        .into_values()
        .map(|piece| match piece {
            (Some(s), None) => Some(SpelledSection {
                uuid: s.uuid.clone(),
                md: s.text.clone(),
            }),
            (s, Some(d)) => Some(SpelledSection {
                uuid: s.and_then(|s| s.uuid.clone()),
                md: d.spell(s.map(|s| s.text.as_str()))?,
            }),
            (None, None) => unreachable!("every entry has a row"),
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

    /// What the store keeps of a document spells it back byte for byte:
    /// layout from its drawing, a message from its drawing with the body
    /// put back, a whole piece from its text.
    #[test]
    fn the_stored_pieces_spell_the_document() {
        let sections = vec![
            Section::frame("---\ntitle: Bridge\n---\n\n# Bridge\n\n".into()),
            Section::item(
                "m1",
                "<div id=\"m-m1\">\n\n**Riker** — 09:00\n\nEngage.\n\n</div>\n\n".into(),
                "Engage.".into(),
                Vec::new(),
            ),
            Section::item(
                "m2",
                "<div id=\"m-m2\">\n\nSaid.\n\n\n*[attached]*\n</div>\n\n".into(),
                "Said.\n\n*[attached]*".into(),
                Vec::new(),
            ),
            Section::keyed("p", "A page's own words.\n".into()),
        ];
        let rows = stored("doc", &sections);
        let drawings = drawn("doc", &sections);
        assert_eq!(drawings.len(), 3, "the layout and both messages");
        assert_eq!(
            drawings[1].body_at,
            sections[1].md.find("Engage.").map(|i| i as i64)
        );
        assert_eq!(
            drawings[2].body_at, None,
            "a body not drawn verbatim is kept whole"
        );
        let spelled = spell(&rows, &drawings).unwrap();
        let joined: String = spelled.iter().map(|p| p.md.as_str()).collect();
        assert_eq!(joined, join(&sections));
        let uuids: Vec<Option<&str>> = spelled.iter().map(|p| p.uuid.as_deref()).collect();
        assert_eq!(uuids, [None, Some("m1"), Some("m2"), Some("p")]);
    }

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
