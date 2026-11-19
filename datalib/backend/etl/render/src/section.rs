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
    /// The class of the box `md` is drawn in ([`Section::boxed`]); `None`
    /// for a section in no box.
    pub class: Option<String>,
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
            class: None,
        }
    }

    /// An item's section drawn in its box: the `<div>` naming the item,
    /// with `class`, around `inside`. Every box is drawn this one way, so
    /// what reads it back knows the box from the uuid and the class alone
    /// ([`inside_box`]) and the viewer can draw the box itself.
    pub fn boxed(uuid: &str, class: String, inside: &str) -> Self {
        Self {
            md: draw_box(uuid, &class, inside),
            class: Some(class),
            ..Self::keyed(uuid, String::new())
        }
    }

    /// An item's section that also says which part of it is the body.
    pub fn item(uuid: &str, md: String, text: String, addressed: Vec<Addressee>) -> Self {
        Self::keyed(uuid, md).with_body(text, addressed)
    }

    /// The section kept as an item's body, `text`, rather than whole.
    pub fn with_body(self, text: String, addressed: Vec<Addressee>) -> Self {
        Self {
            kept: Kept::Body {
                text,
                addressed,
                labels: Vec::new(),
            },
            ..self
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
            class: None,
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
                class: s.class.clone(),
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
    /// The class of the box `md` is drawn in, where it is in one.
    pub class: Option<String>,
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
                class: s.class.clone(),
            }),
            (s, Some(d)) => Some(SpelledSection {
                uuid: s.and_then(|s| s.uuid.clone()),
                md: d.spell(s.map(|s| s.text.as_str()))?,
                class: s.and_then(|s| s.class.clone()),
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

    /// A keyed piece in its box ([`Section::boxed`]).
    pub fn boxed(&mut self, uuid: &str, class: String, inside: &str) {
        self.flush();
        self.done.push(Section::boxed(uuid, class, inside));
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

/// A message's box class: `msg`, `msg--{provider}` so per-provider CSS can
/// apply without each renderer inventing its own scheme, then `more`.
pub fn msg_class(provider: Provider, more: &[&str]) -> String {
    let mut class = format!("msg msg--{provider}");
    for c in more {
        class.push(' ');
        class.push_str(c);
    }
    class
}

/// The opening tag of an item's box ([`Section::boxed`]).
pub fn box_open(uuid: &str, class: &str) -> String {
    format!(
        r#"<div {attrs} class="{class}">"#,
        attrs = section_attrs(uuid)
    )
}

/// What ends an item's box, the blank line after it included.
const BOX_CLOSE: &str = "</div>\n\n";

/// What `md`, a section of `uuid` drawn in a box of `class` by
/// [`Section::boxed`], holds inside its box. `None` when `md` is not that
/// box: the drawing came from somewhere else.
pub fn inside_box<'a>(uuid: &str, class: &str, md: &'a str) -> Option<&'a str> {
    md.strip_prefix(box_open(uuid, class).as_str())?
        .strip_prefix("\n\n")?
        .strip_suffix(BOX_CLOSE)
}

/// `inside` drawn in the box of `uuid` and `class`: [`Section::boxed`]'s
/// markdown, for what has only the box's facts and what is inside it.
pub fn draw_box(uuid: &str, class: &str, inside: &str) -> String {
    format!("{}\n\n{inside}{BOX_CLOSE}", box_open(uuid, class))
}

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
                class: None,
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
    fn box_open_shape() {
        assert_eq!(
            box_open("abc", &msg_class(Provider::Slack, &["unread"])),
            r#"<div id="m-abc" data-section-uuid="abc" class="msg msg--slack unread">"#
        );
    }

    /// A box is drawn one way, so its inside comes back out of the drawing
    /// from the uuid and class alone, and only from that box.
    #[test]
    fn a_box_gives_back_what_is_inside_it() {
        let inside = "## Riker\n\n<div>Engage.</div>\n";
        let s = Section::boxed("m1", msg_class(Provider::Slack, &[]), inside);
        assert_eq!(
            s.md,
            "<div id=\"m-m1\" data-section-uuid=\"m1\" class=\"msg msg--slack\">\n\n\
             ## Riker\n\n<div>Engage.</div>\n</div>\n\n"
        );
        let class = s.class.as_deref().unwrap();
        assert_eq!(inside_box("m1", class, &s.md), Some(inside));
        assert_eq!(draw_box("m1", class, inside), s.md);
        assert_eq!(inside_box("m2", class, &s.md), None, "another item's box");
        assert_eq!(inside_box("m1", "msg", &s.md), None, "another class");
        assert_eq!(inside_box("m1", class, "Engage.\n"), None, "no box");
    }

    /// The store keeps a box's class with its section, and spells it back.
    #[test]
    fn the_store_keeps_the_box() {
        let sections = vec![
            Section::frame("# Title\n".into()),
            Section::boxed("a", "msg".into(), "hi\n").with_body("hi".into(), Vec::new()),
            Section::boxed("b", "msg".into(), "yes\n"),
        ];
        let rows = stored("doc", &sections);
        assert_eq!(
            rows.iter().map(|r| r.class.as_deref()).collect::<Vec<_>>(),
            [Some("msg"), Some("msg")]
        );
        let spelled = spell(&rows, &drawn("doc", &sections)).unwrap();
        assert_eq!(
            spelled
                .iter()
                .map(|s| s.class.as_deref())
                .collect::<Vec<_>>(),
            [None, Some("msg"), Some("msg")]
        );
        assert_eq!(
            spelled.iter().map(|s| s.md.as_str()).collect::<String>(),
            join(&sections)
        );
    }
}
