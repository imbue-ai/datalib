//! Shared "section anchor" helpers for provider renderers, and the
//! [`Section`] a renderer hands over so a document can be read section by
//! section without parsing its markdown.

use datalib_schema::drawings::DrawingRow;
use datalib_schema::folds::FoldRow;
use datalib_schema::providers::Provider;
use datalib_schema::sections::SectionRow;

use crate::item::Item;

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
    /// The fold it opens or closes ([`Section::fold_open`]); `None` for a
    /// section that does neither.
    pub fold: Option<Fold>,
}

/// A `<details>` a renderer opens around a run of sections, or closes.
/// Every fold is drawn one way ([`draw_fold_open`], [`FOLD_CLOSE`]), so
/// what reads it back knows it from its class and summary alone and the
/// viewer can draw it around the pieces it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fold {
    /// Opens a fold drawn with `class` and `summary`, HTML the renderer
    /// escaped.
    Open { class: String, summary: String },
    /// Closes the fold it is in.
    Close,
}

/// What of a section the render store keeps in `sections`, for whatever
/// draws it again: the viewer, an export, the search index. Layout, which
/// it does not keep there, `drawings` keeps ([`drawn`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kept {
    /// The piece whole, as drawn, marked framed: from a renderer that has
    /// not handed its items over as facts, or a piece of no item's that
    /// still says something (a PDF's text).
    Whole,
    /// An item, as the facts its piece is drawn from.
    Item(Box<Item>),
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
            fold: None,
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

    /// An item in its box, drawn from its facts as whatever reads it back
    /// draws it ([`Item::draw`]), and kept as those facts.
    pub fn item(uuid: &str, class: String, item: Item) -> Self {
        Self {
            kept: Kept::Item(Box::new(item.clone())),
            ..Self::boxed(uuid, class, &item.draw())
        }
    }

    pub fn unkeyed(md: String) -> Self {
        Self {
            uuid: None,
            md,
            kept: Kept::Whole,
            class: None,
            fold: None,
        }
    }

    /// A piece that is layout alone, which the store does not keep.
    pub fn frame(md: String) -> Self {
        Self {
            kept: Kept::Frame,
            ..Self::unkeyed(md)
        }
    }

    /// Opens a fold around the sections after it, up to the
    /// [`Section::fold_close`] that ends it: a `<details>` of `class`, its
    /// `<summary>` saying `summary`, HTML the caller escaped. Layout, which
    /// the store keeps only as drawn.
    pub fn fold_open(class: &str, summary: &str) -> Self {
        Self {
            fold: Some(Fold::Open {
                class: class.to_string(),
                summary: summary.to_string(),
            }),
            ..Self::frame(draw_fold_open(class, summary))
        }
    }

    /// Closes the fold it is in. Layout, as [`Section::fold_open`].
    pub fn fold_close() -> Self {
        Self {
            fold: Some(Fold::Close),
            ..Self::frame(FOLD_CLOSE.to_string())
        }
    }

    /// `more` drawn after the section, in the same piece.
    pub fn then(mut self, more: &str) -> Self {
        self.md.push_str(more);
        self
    }

    /// The section kept whole and framed, as a piece that says something
    /// search reads, rather than as layout.
    pub fn kept_whole(self) -> Self {
        Self {
            kept: Kept::Whole,
            ..self
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
            let row = match &s.kept {
                Kept::Frame => return None,
                Kept::Whole => SectionRow {
                    markdown_uuid: markdown_uuid.to_string(),
                    seq: seq as i64,
                    uuid: s.uuid.clone(),
                    text: s.md.clone(),
                    framed: true,
                    ..SectionRow::default()
                },
                Kept::Item(item) => item.row(markdown_uuid, seq as i64, s.uuid.clone()),
            };
            Some(SectionRow {
                class: s.class.clone(),
                ..row
            })
        })
        .collect()
}

/// How the store keeps what `sections` does not: each piece of layout,
/// whole. With [`stored`] it spells the document back ([`spell`]).
pub fn drawn(markdown_uuid: &str, sections: &[Section]) -> Vec<DrawingRow> {
    sections
        .iter()
        .enumerate()
        .filter(|(_, s)| s.kept == Kept::Frame)
        .map(|(seq, s)| DrawingRow {
            markdown_uuid: markdown_uuid.to_string(),
            seq: seq as i64,
            md: s.md.clone(),
        })
        .collect()
}

/// The folds the store keeps of a document's sections, each at its place
/// among all of them.
pub fn folds(markdown_uuid: &str, sections: &[Section]) -> Vec<FoldRow> {
    sections
        .iter()
        .enumerate()
        .filter_map(|(seq, s)| {
            let (class, summary) = match s.fold.as_ref()? {
                Fold::Open { class, summary } => (Some(class.clone()), Some(summary.clone())),
                Fold::Close => (None, None),
            };
            Some(FoldRow {
                markdown_uuid: markdown_uuid.to_string(),
                seq: seq as i64,
                class,
                summary,
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
    /// The fold it opens or closes, where it does.
    pub fold: Option<Fold>,
}

impl SpelledSection {
    /// A section as the store would spell it back, for what has the
    /// section in hand.
    pub fn of(s: &Section) -> Self {
        Self {
            uuid: s.uuid.clone(),
            md: s.md.clone(),
            class: s.class.clone(),
            fold: s.fold.clone(),
        }
    }
}

/// A document spelled back from what the store keeps of it, section by
/// section in order: an item drawn from its facts, a whole piece as its
/// text, layout as its drawing.
pub fn spell(
    sections: &[SectionRow],
    drawn: &[DrawingRow],
    folds: &[FoldRow],
) -> Vec<SpelledSection> {
    let fold_at: std::collections::HashMap<i64, Fold> = folds
        .iter()
        .map(|f| {
            let fold = match (&f.class, &f.summary) {
                (Some(class), summary) => Fold::Open {
                    class: class.clone(),
                    summary: summary.clone().unwrap_or_default(),
                },
                (None, _) => Fold::Close,
            };
            (f.seq, fold)
        })
        .collect();
    let mut by_seq: std::collections::BTreeMap<i64, SpelledSection> =
        std::collections::BTreeMap::new();
    for d in drawn {
        by_seq.insert(
            d.seq,
            SpelledSection {
                uuid: None,
                md: d.md.clone(),
                class: None,
                fold: None,
            },
        );
    }
    for s in sections {
        by_seq.insert(
            s.seq,
            SpelledSection {
                uuid: s.uuid.clone(),
                md: spell_section(s),
                class: s.class.clone(),
                fold: None,
            },
        );
    }
    by_seq
        .into_iter()
        .map(|(seq, piece)| SpelledSection {
            fold: fold_at.get(&seq).cloned(),
            ..piece
        })
        .collect()
}

/// A section as the store keeps it, drawn: an item from its facts in its
/// box, anything else as its text.
fn spell_section(s: &SectionRow) -> String {
    let Some(item) = Item::of_row(s) else {
        if let Some(form) = &s.form {
            tracing::warn!(%form, seq = s.seq, "an item in a form this build does not know; drawn as its body");
        }
        return s.text.clone();
    };
    let inside = item.draw();
    match (&s.uuid, &s.class) {
        (Some(uuid), Some(class)) => draw_box(uuid, class, &inside),
        _ => inside,
    }
}

/// A document's body as the viewer is sent it: each message in its box as
/// what is inside the box, each other piece as written, and each fold as a
/// node around the pieces it holds. Drawn back ([`Node::draw`]), the
/// nodes are the sections they came from, joined.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(untagged)]
pub enum Node {
    Fold {
        /// Its class.
        fold: String,
        summary: String,
        pieces: Vec<Node>,
    },
    Piece {
        uuid: Option<String>,
        /// The class of its box, where it is in one; `md` is then what is
        /// inside the box.
        #[serde(skip_serializing_if = "Option::is_none")]
        class: Option<String>,
        md: String,
    },
}

impl Node {
    /// The node as markdown, each box and fold drawn as render draws it.
    pub fn draw(&self, out: &mut String) {
        match self {
            Node::Fold {
                fold,
                summary,
                pieces,
            } => {
                out.push_str(&draw_fold_open(fold, summary));
                for p in pieces {
                    p.draw(out);
                }
                out.push_str(FOLD_CLOSE);
            }
            Node::Piece {
                uuid: Some(uuid),
                class: Some(class),
                md,
            } => out.push_str(&draw_box(uuid, class, md)),
            Node::Piece { md, .. } => out.push_str(md),
        }
    }
}

/// The nodes `spelled` makes ([`Node`]). A fold whose drawing is not the
/// one its class and summary make, or a close with no fold open, is kept
/// as the piece it is, and a box likewise; neither happens to a document
/// this build rendered.
pub fn nodes(spelled: Vec<SpelledSection>) -> Vec<Node> {
    let mut top: Vec<Node> = Vec::new();
    let mut open: Vec<(String, String, Vec<Node>)> = Vec::new();
    let place =
        |open: &mut Vec<(String, String, Vec<Node>)>, top: &mut Vec<Node>, n: Node| match open
            .last_mut()
        {
            Some((_, _, pieces)) => pieces.push(n),
            None => top.push(n),
        };
    for s in spelled {
        match &s.fold {
            Some(Fold::Open { class, summary }) => {
                if let Some(tail) = s.md.strip_prefix(&draw_fold_open(class, summary)) {
                    let mut pieces = Vec::new();
                    if !tail.is_empty() {
                        pieces.push(Node::Piece {
                            uuid: None,
                            class: None,
                            md: tail.to_string(),
                        });
                    }
                    open.push((class.clone(), summary.clone(), pieces));
                    continue;
                }
                tracing::warn!(%class, "a fold is not drawn the way its class and summary say; sent as a piece");
            }
            Some(Fold::Close) if s.md == FOLD_CLOSE => {
                if let Some((fold, summary, pieces)) = open.pop() {
                    let n = Node::Fold {
                        fold,
                        summary,
                        pieces,
                    };
                    place(&mut open, &mut top, n);
                    continue;
                }
            }
            _ => {}
        }
        let n = piece(s);
        place(&mut open, &mut top, n);
    }
    while let Some((fold, summary, pieces)) = open.pop() {
        let n = Node::Fold {
            fold,
            summary,
            pieces,
        };
        place(&mut open, &mut top, n);
    }
    top
}

/// A section as a piece: a message in its box as what is inside it.
fn piece(s: SpelledSection) -> Node {
    if let (Some(uuid), Some(class)) = (&s.uuid, &s.class) {
        if let Some(inside) = inside_box(uuid, class, &s.md) {
            return Node::Piece {
                md: inside.to_string(),
                uuid: s.uuid,
                class: s.class,
            };
        }
        tracing::warn!(%uuid, %class, "a piece is not in the box it names; sent whole");
    }
    Node::Piece {
        uuid: s.uuid,
        class: None,
        md: s.md,
    }
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

/// How a fold opens ([`Section::fold_open`]).
pub fn draw_fold_open(class: &str, summary: &str) -> String {
    format!("<details class=\"{class}\">\n<summary>{summary}</summary>\n\n")
}

/// How a fold closes, the blank line after it included.
pub const FOLD_CLOSE: &str = "</details>\n\n";

#[cfg(test)]
mod tests {
    use super::*;

    /// What the store keeps of a document spells it back byte for byte:
    /// layout from its drawing, an item drawn from its facts, a whole piece
    /// from its text. Nothing of an item is drawn into `drawings`.
    #[test]
    fn the_stored_pieces_spell_the_document() {
        use datalib_schema::sections::ItemForm;
        let riker = Item {
            author: Some("Riker".into()),
            labels: vec!["Bridge".into()],
            ..Item::new(ItemForm::Message, "Engage.".into())
        };
        let sections = vec![
            Section::frame("---\ntitle: Bridge\n---\n\n# Bridge\n\n".into()),
            Section::item("m1", "msg".into(), riker),
            Section::item(
                "m2",
                "msg unread".into(),
                Item::new(ItemForm::Event, "Worf joined".into()),
            ),
            Section::keyed("p", "A page's own words.\n".into()),
        ];
        let rows = stored("doc", &sections);
        let drawings = drawn("doc", &sections);
        assert_eq!(
            drawings.iter().map(|d| d.seq).collect::<Vec<_>>(),
            [0],
            "the layout alone"
        );
        assert_eq!(rows[0].text, "Engage.");
        let spelled = spell(&rows, &drawings, &[]);
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

    /// A piece is kept by its place among all of them: an item as its
    /// facts, a piece not handed over as one whole and framed, and layout
    /// not at all.
    #[test]
    fn the_store_keeps_items_and_whole_pieces_but_not_layout() {
        use datalib_schema::sections::{AddressRole, Addressee, ItemForm};
        let item = Item {
            addressed: vec![Addressee {
                role: AddressRole::To,
                name: "Picard".into(),
                handle: None,
            }],
            labels: vec!["Bridge".into()],
            ..Item::new(ItemForm::Message, "hi".into())
        };
        let sections = vec![
            Section::frame("# Title\n".into()),
            Section::item("a", "msg".into(), item.clone()),
            Section::keyed("b", "<div>Data: yes</div>\n".into()),
            Section::unkeyed("Page 2 text\n".into()),
        ];
        let whole = |seq: i64, uuid: Option<&str>, text: &str| SectionRow {
            markdown_uuid: "doc".into(),
            seq,
            uuid: uuid.map(String::from),
            text: text.into(),
            framed: true,
            ..SectionRow::default()
        };
        assert_eq!(
            stored("doc", &sections),
            vec![
                SectionRow {
                    class: Some("msg".into()),
                    ..item.row("doc", 1, Some("a".into()))
                },
                whole(2, Some("b"), "<div>Data: yes</div>\n"),
                whole(3, None, "Page 2 text\n"),
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
        use datalib_schema::sections::ItemForm;
        let sections = vec![
            Section::frame("# Title\n".into()),
            Section::item("a", "msg".into(), Item::new(ItemForm::Message, "hi".into())),
            Section::boxed("b", "msg".into(), "yes\n"),
        ];
        let rows = stored("doc", &sections);
        assert_eq!(
            rows.iter().map(|r| r.class.as_deref()).collect::<Vec<_>>(),
            [Some("msg"), Some("msg")]
        );
        let spelled = spell(&rows, &drawn("doc", &sections), &[]);
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

    /// A document of `sections` as the store keeps and spells it back.
    fn kept(sections: &[Section]) -> Vec<SpelledSection> {
        spell(
            &stored("doc", sections),
            &drawn("doc", sections),
            &folds("doc", sections),
        )
    }

    /// Folds come back as nodes around the pieces they hold, nested as
    /// they nest, a message as what is inside its box; drawn back, the
    /// nodes are the document as rendered, byte for byte.
    #[test]
    fn folds_are_nodes_around_their_pieces() {
        let sections = vec![
            Section::frame("# Bridge\n\n".into()),
            Section::fold_open("branch", "✎ Another version · 2 messages"),
            Section::boxed("a", "msg".into(), "Engage.\n"),
            Section::fold_open("tool-group", "🛠 1 tool step"),
            Section::boxed("b", "msg".into(), "ls\n"),
            Section::fold_close(),
            Section::fold_close(),
            Section::boxed("c", "msg".into(), "Make it so.\n"),
        ];
        let nodes = nodes(kept(&sections));
        let box_of = |uuid: &str, md: &str| Node::Piece {
            uuid: Some(uuid.into()),
            class: Some("msg".into()),
            md: md.into(),
        };
        assert_eq!(
            nodes,
            vec![
                Node::Piece {
                    uuid: None,
                    class: None,
                    md: "# Bridge\n\n".into()
                },
                Node::Fold {
                    fold: "branch".into(),
                    summary: "✎ Another version · 2 messages".into(),
                    pieces: vec![
                        box_of("a", "Engage.\n"),
                        Node::Fold {
                            fold: "tool-group".into(),
                            summary: "🛠 1 tool step".into(),
                            pieces: vec![box_of("b", "ls\n")],
                        },
                    ],
                },
                box_of("c", "Make it so.\n"),
            ]
        );
        let mut drawn = String::new();
        nodes.iter().for_each(|n| n.draw(&mut drawn));
        assert_eq!(drawn, join(&sections));
    }

    /// A fold kept as text search reads, with more written after its
    /// opening inside it (Drive's quote of what a thread is on), keeps that
    /// as the fold's first piece.
    #[test]
    fn what_a_fold_opening_says_after_its_summary_is_its_first_piece() {
        let sections = vec![
            Section::fold_open("comment-thread", "💬 2 comments")
                .then("> Captain&#39;s log\n\n")
                .kept_whole(),
            Section::boxed("a", "msg".into(), "Noted.\n"),
            Section::fold_close().kept_whole(),
        ];
        assert_eq!(
            stored("doc", &sections).len(),
            3,
            "kept as text search reads"
        );
        let nodes = nodes(kept(&sections));
        let Node::Fold { pieces, .. } = &nodes[0] else {
            panic!("{nodes:?}")
        };
        assert_eq!(
            pieces[0],
            Node::Piece {
                uuid: None,
                class: None,
                md: "> Captain&#39;s log\n\n".into()
            }
        );
        let mut drawn = String::new();
        nodes.iter().for_each(|n| n.draw(&mut drawn));
        assert_eq!(drawn, join(&sections));
    }

    /// A close with no fold open is the piece it is, not a fold.
    #[test]
    fn a_close_with_nothing_open_is_a_piece() {
        let nodes = nodes(kept(&[
            Section::fold_close(),
            Section::boxed("a", "msg".into(), "x\n"),
        ]));
        assert!(matches!(&nodes[0], Node::Piece { md, .. } if md == FOLD_CLOSE));
    }
}
