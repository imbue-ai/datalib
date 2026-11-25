//! An item's piece of a document as facts, and the one drawing of them.
//!
//! A renderer hands an item over as an [`Item`]; the render store keeps its
//! facts in `sections`, and whatever reads the document draws the piece
//! from them with [`Item::draw`], the function render drew it with. So
//! nothing drawn around a body is stored, and what is read is what was
//! rendered.

use datalib_handle::Handle;
use datalib_schema::sections::{
    addressed_json, labels_json, parse_addressed, parse_labels, parse_reactions, reactions_json,
    AddressRole, Addressee, ItemForm, Reaction, SectionRow,
};

use crate::html::escape_md_inline;
use crate::message::{
    chip_link, labels_html, message_stamp_text_html, source_link, timestamp_html, MessageHeader,
};

/// What an item's piece says and shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub form: ItemForm,
    /// What the item says, as markdown: what search reads of it.
    pub body: String,
    /// Who it was addressed to, where it says so itself.
    pub addressed: Vec<Addressee>,
    /// Where it is filed upstream.
    pub labels: Vec<String>,
    /// Who wrote it, as shown; `None` for nobody.
    pub author: Option<String>,
    /// Who wrote it, as a handle (`Handle::as_str`).
    pub author_handle: Option<String>,
    /// When: an RFC 3339 instant, `Z` for an unknown clock, or a day.
    pub at: Option<String>,
    /// Words the source says beside the author (`APPROVED`, `reply`).
    pub tags: Vec<String>,
    /// The item's own page at its source.
    pub link: Option<String>,
    /// In the order they are drawn.
    pub reactions: Vec<Reaction>,
    pub replies: Option<Replies>,
    /// A chat message that continues its author's run: drawn with no
    /// header, under the first message's.
    pub continues: bool,
}

/// How many replies a thread has upstream, and how many of them the
/// mirror holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Replies {
    pub upstream: u64,
    pub held: u64,
}

impl Item {
    /// An item of `form` saying `body`, with no other facts yet.
    pub fn new(form: ItemForm, body: String) -> Self {
        Self {
            form,
            body,
            addressed: Vec::new(),
            labels: Vec::new(),
            author: None,
            author_handle: None,
            at: None,
            tags: Vec::new(),
            link: None,
            reactions: Vec::new(),
            replies: None,
            continues: false,
        }
    }

    /// What is inside the item's box. Each part is a block of markdown
    /// with a blank line after it.
    pub fn draw(&self) -> String {
        match self.form {
            ItemForm::Event => format!(
                "*<small>{ts} — system: {body}</small>*\n\n",
                ts = self.stamp(),
                body = self.body,
            ),
            ItemForm::Message | ItemForm::Chat => {
                let chat = self.form == ItemForm::Chat;
                let blocks = [
                    Some(if chat && self.continues {
                        self.continued()
                    } else {
                        self.header()
                    }),
                    recipients_line(&self.addressed),
                    Some(self.body.clone()).filter(|b| !b.is_empty()),
                    if chat {
                        reaction_pills(&self.reactions)
                    } else {
                        reactions_list(&self.reactions)
                    },
                    self.replies.and_then(replies_line),
                ];
                blocks.into_iter().flatten().map(|b| b + "\n\n").collect()
            }
        }
    }

    /// What a message that continues a run shows in place of its header:
    /// its time and its link, small, and no `##`, so the viewer finds no
    /// header in it.
    fn continued(&self) -> String {
        let link = self
            .link
            .as_deref()
            .map(|url| format!(" {}", source_link(url)))
            .unwrap_or_default();
        let labels = labels_html(&self.labels())
            .map(|l| format!(" {l}"))
            .unwrap_or_default();
        format!(
            "<span class=\"msg-cont\">{}{labels}{link}</span>",
            self.stamp()
        )
    }

    fn labels(&self) -> Vec<&str> {
        self.labels.iter().map(String::as_str).collect()
    }

    fn header(&self) -> String {
        let handle = self.author_handle.as_deref().and_then(Handle::parse);
        let tags: Vec<&str> = self.tags.iter().map(String::as_str).collect();
        MessageHeader {
            author: self.author.as_deref().unwrap_or(""),
            handle: handle.as_ref(),
            stamp: self.at.as_deref(),
            tags: &tags,
            labels: &self.labels(),
            source_url: self.link.as_deref(),
            ..Default::default()
        }
        .render()
    }

    fn stamp(&self) -> String {
        match &self.at {
            Some(at) => message_stamp_text_html(at),
            None => timestamp_html(None, None),
        }
    }

    /// The item as the render store keeps it, at `seq` of `markdown_uuid`.
    pub fn row(&self, markdown_uuid: &str, seq: i64, uuid: Option<String>) -> SectionRow {
        let count = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
        SectionRow {
            markdown_uuid: markdown_uuid.to_string(),
            seq,
            uuid,
            text: self.body.clone(),
            framed: false,
            form: Some(self.form.as_str().to_string()),
            author: self.author.clone(),
            author_handle: self.author_handle.clone(),
            at: self.at.clone(),
            tags: labels_json(&self.tags),
            link: self.link.clone(),
            reactions: reactions_json(&self.reactions),
            replies: self.replies.map(|r| count(r.upstream)),
            replies_held: self.replies.map(|r| count(r.held)),
            addressed: addressed_json(&self.addressed),
            labels: labels_json(&self.labels),
            continues: self.continues,
            class: None,
        }
    }

    /// The item a row keeps. `None` for a row that keeps a piece whole,
    /// or one in a form this build does not know.
    pub fn of_row(row: &SectionRow) -> Option<Self> {
        let form = ItemForm::parse(row.form.as_deref()?)?;
        let list = |json: &Option<String>| json.as_deref().and_then(parse_labels);
        let count = |n: Option<i64>| u64::try_from(n.unwrap_or(0)).unwrap_or(0);
        Some(Self {
            form,
            body: row.text.clone(),
            addressed: row
                .addressed
                .as_deref()
                .and_then(parse_addressed)
                .unwrap_or_default(),
            labels: list(&row.labels).unwrap_or_default(),
            author: row.author.clone(),
            author_handle: row.author_handle.clone(),
            at: row.at.clone(),
            tags: list(&row.tags).unwrap_or_default(),
            link: row.link.clone(),
            reactions: row
                .reactions
                .as_deref()
                .and_then(parse_reactions)
                .unwrap_or_default(),
            replies: row.replies.map(|upstream| Replies {
                upstream: count(Some(upstream)),
                held: count(row.replies_held),
            }),
            continues: row.continues,
        })
    }
}

/// A person named in an item: a chip where there is a handle this build
/// reads, else their name as text in `class`.
fn named(name: &str, handle: Option<&str>, class: &str) -> String {
    match handle.and_then(Handle::parse) {
        Some(h) => chip_link(name, &h),
        None => format!("<span class=\"{class}\">{}</span>", escape_md_inline(name)),
    }
}

/// Who an item was addressed to, one paragraph straight under its header:
/// `To [Will Riker](mailto:…), …; Cc …`. A paragraph with inline HTML, not
/// an HTML block, because markdown is not parsed inside a block and the
/// chips have to be.
pub fn recipients_line(addressed: &[Addressee]) -> Option<String> {
    let groups: Vec<String> = [
        (AddressRole::To, "To"),
        (AddressRole::Cc, "Cc"),
        (AddressRole::Bcc, "Bcc"),
    ]
    .into_iter()
    .filter_map(|(role, label)| {
        let names: Vec<String> = addressed
            .iter()
            .filter(|a| a.role == role)
            .map(|a| named(&a.name, a.handle.as_deref(), "msg-recipient"))
            .collect();
        (!names.is_empty()).then(|| {
            format!(
                "<span class=\"msg-recipients-role\">{label}</span> {}",
                names.join(", ")
            )
        })
    })
    .collect();
    (!groups.is_empty()).then(|| {
        format!(
            "<span class=\"msg-recipients\">{}</span>",
            groups.join("; ")
        )
    })
}

/// One bullet per reaction, each anchored by its own uuid so its grid
/// row's click lands on it.
fn reactions_list(reactions: &[Reaction]) -> Option<String> {
    let bullets: Vec<String> = reactions
        .iter()
        .map(|r| {
            format!(
                "- <span id=\"m-{uuid}\" data-section-uuid=\"{uuid}\">{emoji} {who}</span>",
                uuid = r.uuid,
                emoji = escape_md_inline(&r.emoji),
                who = reactor(r),
            )
        })
        .collect();
    (!bullets.is_empty()).then(|| bullets.join("\n"))
}

/// One pill per emoji, saying how many reacted with it, in the order the
/// reactions are drawn. Each reactor is in the pill, anchored by the
/// reaction's own uuid so its grid row's click lands on it; the viewer
/// shows them on hover.
fn reaction_pills(reactions: &[Reaction]) -> Option<String> {
    let mut emojis: Vec<(&str, Vec<&Reaction>)> = Vec::new();
    for r in reactions {
        match emojis.iter_mut().find(|(e, _)| *e == r.emoji) {
            Some((_, rs)) => rs.push(r),
            None => emojis.push((&r.emoji, vec![r])),
        }
    }
    let pills: Vec<String> = emojis
        .into_iter()
        .map(|(emoji, rs)| {
            let who: Vec<String> = rs
                .iter()
                .map(|r| {
                    format!(
                        "<span id=\"m-{uuid}\" data-section-uuid=\"{uuid}\">{}</span>",
                        reactor(r),
                        uuid = r.uuid,
                    )
                })
                .collect();
            format!(
                "<span class=\"msg-reaction\">{emoji} {n}<span class=\"msg-reactors\"> {who}</span></span>",
                emoji = escape_md_inline(emoji),
                n = rs.iter().map(|r| r.count).sum::<u32>(),
                who = who.join(", "),
            )
        })
        .collect();
    (!pills.is_empty()).then(|| pills.join(" "))
}

fn reactor(r: &Reaction) -> String {
    match r.handle.as_deref().and_then(Handle::parse) {
        Some(h) => chip_link(&r.name, &h),
        None => escape_md_inline(&r.name),
    }
}

fn replies_line(replies: Replies) -> Option<String> {
    let noun = match replies.upstream {
        0 => return None,
        1 => "reply",
        _ => "replies",
    };
    let missing = match replies.upstream.saturating_sub(replies.held) {
        0 => String::new(),
        n => format!(" · {n} not mirrored yet"),
    };
    Some(format!(
        "<span class=\"msg-replies\">{} {noun}{missing}</span>",
        replies.upstream
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn riker() -> Item {
        Item {
            addressed: vec![
                Addressee {
                    role: AddressRole::Cc,
                    name: "Deanna Troi".into(),
                    handle: None,
                },
                Addressee {
                    role: AddressRole::To,
                    name: "Data".into(),
                    handle: Some("email:data@enterprise.starfleet".into()),
                },
            ],
            labels: vec!["Bridge <crew>".into(), "Inbox".into()],
            author: Some("Will Riker".into()),
            author_handle: Some("email:riker@enterprise.starfleet".into()),
            at: Some("2364-04-11T09:00:00+01:00".into()),
            tags: vec!["reply".into()],
            link: Some("https://example.invalid/m/1".into()),
            reactions: vec![Reaction {
                count: 1,
                uuid: "r1".into(),
                emoji: "🖖".into(),
                name: "Spock".into(),
                handle: None,
            }],
            replies: Some(Replies {
                upstream: 3,
                held: 1,
            }),
            ..Item::new(ItemForm::Message, "Engage.".into())
        }
    }

    /// Every part of a message is drawn from its facts, in order, a blank
    /// line after each.
    #[test]
    fn a_message_is_drawn_from_its_facts() {
        let drawn = riker().draw();
        let blocks: Vec<&str> = drawn.split("\n\n").collect();
        assert_eq!(blocks.len(), 6, "{drawn}");
        assert!(blocks[0].starts_with("## [Will Riker](mailto:riker@enterprise.starfleet "));
        assert!(blocks[0].contains("<span class=\"msg-tag\">reply</span>"));
        assert!(blocks[0].contains("datetime=\"2364-04-11T09:00:00+01:00\""));
        assert!(blocks[0].contains(
            "</time> <span class=\"msg-labels\"><span class=\"msg-label\">Bridge &lt;crew&gt;</span> \
             <span class=\"msg-label\">Inbox</span></span> <a class=\"source-link\""
        ));
        assert!(blocks[0].ends_with("noreferrer\">↗</a>"));
        assert_eq!(
            blocks[1],
            "<span class=\"msg-recipients\"><span class=\"msg-recipients-role\">To</span> \
             [Data](mailto:data@enterprise.starfleet \"Data <data@enterprise.starfleet>\"); \
             <span class=\"msg-recipients-role\">Cc</span> \
             <span class=\"msg-recipient\">Deanna Troi</span></span>"
        );
        assert_eq!(blocks[2], "Engage.");
        assert_eq!(
            blocks[3],
            "- <span id=\"m-r1\" data-section-uuid=\"r1\">🖖 Spock</span>"
        );
        assert_eq!(
            blocks[4],
            "<span class=\"msg-replies\">3 replies · 2 not mirrored yet</span>"
        );
        assert_eq!(blocks[5], "");
    }

    /// In a chat, a run's later message has no header but its time and
    /// link, and reactions are one pill per emoji naming who reacted.
    #[test]
    fn a_chat_message_continues_a_run_and_counts_its_reactions() {
        let spock = |uuid: &str, emoji: &str, name: &str| Reaction {
            count: 1,
            uuid: uuid.into(),
            emoji: emoji.into(),
            name: name.into(),
            handle: None,
        };
        let item = Item {
            form: ItemForm::Chat,
            continues: true,
            reactions: vec![
                spock("r1", "👍", "Picard"),
                spock("r2", "👍", "Riker"),
                spock("r3", "🖖", "Spock"),
            ],
            ..riker()
        };
        let drawn = item.draw();
        let blocks: Vec<&str> = drawn.split("\n\n").collect();
        assert!(!drawn.contains("## "), "{drawn}");
        assert!(
            blocks[0].starts_with("<span class=\"msg-cont\"><time class=\"msg-ts\""),
            "{drawn}"
        );
        assert!(blocks[0].ends_with("noreferrer\">↗</a></span>"), "{drawn}");
        assert!(
            blocks[0].contains("<span class=\"msg-label\">Inbox</span>"),
            "{drawn}"
        );
        assert_eq!(
            blocks[3],
            "<span class=\"msg-reaction\">👍 2<span class=\"msg-reactors\"> \
             <span id=\"m-r1\" data-section-uuid=\"r1\">Picard</span>, \
             <span id=\"m-r2\" data-section-uuid=\"r2\">Riker</span></span></span> \
             <span class=\"msg-reaction\">🖖 1<span class=\"msg-reactors\"> \
             <span id=\"m-r3\" data-section-uuid=\"r3\">Spock</span></span></span>"
        );
        let first = Item {
            continues: false,
            ..item.clone()
        };
        assert!(first.draw().starts_with("## [Will Riker]"));
        assert_eq!(Item::of_row(&item.row("doc", 0, None)), Some(item));
    }

    /// A message with nothing but a header is its header; one with no
    /// author or time says so rather than inventing either.
    #[test]
    fn a_bare_message_is_its_header() {
        let item = Item::new(ItemForm::Message, String::new());
        assert_eq!(
            item.draw(),
            "## <span class=\"msg-ts\">(no timestamp)</span>\n\n"
        );
    }

    #[test]
    fn an_event_is_one_small_line() {
        let item = Item {
            at: Some("2364-04-11T09:00:00Z".into()),
            ..Item::new(ItemForm::Event, "Worf joined".into())
        };
        assert_eq!(
            item.draw(),
            "*<small><time class=\"msg-ts\" datetime=\"2364-04-11T09:00:00Z\" \
             title=\"2364-04-11 09:00:00 UTC\">Sat Apr 11th, 2364 at 09:00</time> \
             — system: Worf joined</small>*\n\n"
        );
    }

    /// What the store keeps of an item is the item: read back, it draws
    /// the same piece.
    #[test]
    fn an_item_round_trips_through_its_row() {
        let item = riker();
        let row = item.row("doc", 3, Some("m1".into()));
        assert!(!row.framed);
        assert_eq!(row.form.as_deref(), Some("message"));
        assert_eq!(Item::of_row(&row), Some(item.clone()));
        let event = Item::new(ItemForm::Event, "Worf joined".into());
        assert_eq!(Item::of_row(&event.row("doc", 0, None)), Some(event));
    }

    /// A row kept whole, or in a form a newer build wrote, is no item.
    #[test]
    fn a_whole_piece_or_an_unknown_form_is_no_item() {
        let mut row = riker().row("doc", 0, None);
        row.form = None;
        assert_eq!(Item::of_row(&row), None);
        row.form = Some("page".into());
        assert_eq!(Item::of_row(&row), None);
    }

    #[test]
    fn replies_say_how_many_are_not_mirrored() {
        let line = |upstream, held| replies_line(Replies { upstream, held });
        assert_eq!(line(0, 0), None);
        assert_eq!(
            line(1, 1).as_deref(),
            Some("<span class=\"msg-replies\">1 reply</span>")
        );
        assert_eq!(
            line(2, 5).as_deref(),
            Some("<span class=\"msg-replies\">2 replies</span>"),
            "a reply held after upstream deleted it is none missing"
        );
    }
}
