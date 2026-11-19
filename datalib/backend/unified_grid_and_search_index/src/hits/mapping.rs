//! Land a search hit on a grid row. The search index names the item it
//! matched, so a hit lands by id and reads no text; the caller then reads
//! what the landed item says, and only that, for [`GridIndex::quote`].

use std::collections::{HashMap, HashSet};

/// One document a ranking found: by its words or by its meaning.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub score: f64,
    pub snippet: String,
    /// Where in the document the ranking matched, for [`GridIndex::land`].
    pub place: Option<Place>,
}

/// Where in its document a ranked hit matched. A ranking reads no text, so
/// that a long chat costs nothing until a page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// The item the search index matched: the message a keyword hit is
    /// in, the first a Meaning chunk covers. `None` for a piece that is no
    /// item's (a page's text), which lands on the document. Never a place
    /// in the file: the index holds text drawn for it, not the file's.
    Item { uuid: Option<String> },
}

/// Where a hit landed: the row, and the item whose text to quote, where
/// the hit matched one.
#[derive(Debug, Clone)]
pub struct Landed {
    pub row: GridRowRef,
    pub quote: Option<String>,
}

/// The bits of a `grid_rows` row that the hit↔row mapping needs.
#[derive(Debug, Clone)]
pub struct GridRowRef {
    pub uuid: String,
    pub kind: String,
    pub markdown_uuid: String,
    pub provider: String,
    /// The row that is the whole document, as opposed to a place inside it.
    pub is_document: bool,
}

/// A search's words as hits are placed by: lowercased runs of letters and
/// digits, two or more long.
pub fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 2)
        .map(String::from)
        .collect()
}

/// Whether lowercased `text` holds `term` as a word, or a word sharing its
/// first five letters when it has that many: the keyword index stems, so
/// "imminent" found "imminently". Looked up in place, with no word list.
fn holds(text: &str, term: &str) -> bool {
    let stem: String = term.chars().take(5).collect();
    let whole = stem.len() == term.len();
    let at_edge = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
    text.match_indices(stem.as_str()).any(|(at, _)| {
        at_edge(text[..at].chars().next_back())
            && (!whole || at_edge(text[at + stem.len()..].chars().next()))
    })
}

/// A set of `GridRowRef`s, looked up by row and by document.
pub struct GridIndex {
    by_uuid: HashMap<String, GridRowRef>,
    by_document: HashMap<String, Vec<GridRowRef>>,
    /// The search's words, lowercased: a quote starts at the first line of
    /// its message that holds one.
    terms: Vec<String>,
    /// The rows the search's other terms keep, when it has any: a hit
    /// lands only on one of them.
    allowed: Option<HashSet<String>>,
}

impl GridIndex {
    pub fn new(rows: impl IntoIterator<Item = GridRowRef>) -> Self {
        let mut by_uuid: HashMap<String, GridRowRef> = HashMap::new();
        let mut by_document: HashMap<String, Vec<GridRowRef>> = HashMap::new();
        for r in rows {
            by_uuid.insert(r.uuid.clone(), r.clone());
            by_document
                .entry(r.markdown_uuid.clone())
                .or_default()
                .push(r);
        }
        Self {
            by_uuid,
            by_document,
            terms: Vec::new(),
            allowed: None,
        }
    }

    /// Quote each hit from the first line holding one of `free_text`'s
    /// words.
    pub fn with_terms(mut self, free_text: &str) -> Self {
        self.terms = words(free_text);
        self.terms.sort();
        self.terms.dedup();
        self
    }

    /// Land each hit only on a row of `allowed`, the rows the search's
    /// other terms keep, so a document whose hit fell on a message they
    /// drop is placed on one they keep rather than lost.
    pub fn within(mut self, allowed: HashSet<String>) -> Self {
        self.allowed = Some(allowed);
        self
    }

    /// The rows `hit` on `document` lands on: the item it names when the
    /// search keeps it, else the document's own row, or every row of a
    /// document that has none. Empty for a document the grid does not have.
    fn rows_for_hit(&self, document: &str, hit: &SearchHit) -> Vec<GridRowRef> {
        let Some(file_rows) = self.by_document.get(document) else {
            return Vec::new();
        };
        if let Some(Place::Item { uuid: Some(uuid) }) = &hit.place {
            if let Some(row) = self.by_uuid.get(uuid.as_str()).filter(|r| self.keeps(r)) {
                return vec![row.clone()];
            }
        }
        let documents: Vec<GridRowRef> = file_rows
            .iter()
            .filter(|r| r.is_document)
            .cloned()
            .collect();
        if documents.is_empty() {
            file_rows.clone()
        } else {
            documents
        }
    }

    /// The row that *is* the document `hit` lands in, for an
    /// `is:document` search, whose filter would drop a message row. Empty
    /// when the document has no such row.
    fn document_for_hit(&self, document: &str, hit: &SearchHit) -> Vec<GridRowRef> {
        self.rows_for_hit(document, hit)
            .first()
            .and_then(|r| self.by_document.get(&r.markdown_uuid))
            .into_iter()
            .flatten()
            .filter(|r| r.is_document)
            .cloned()
            .collect()
    }

    /// The row a hit on `document` lands on, and the item to quote for it:
    /// the work a page does for each hit it shows, and only for those. The
    /// document's own row for an `is:document` search, quoting the message
    /// the hit matched. `None` for a document the grid does not have.
    pub fn land(&self, document: &str, hit: &SearchHit, documents_only: bool) -> Option<Landed> {
        let rows = if documents_only {
            self.document_for_hit(document, hit)
        } else {
            self.rows_for_hit(document, hit)
        };
        let row = rows.into_iter().next()?;
        let row = if self.keeps(&row) {
            row
        } else {
            self.first_kept(&row.markdown_uuid).unwrap_or(row)
        };
        let quote = match &hit.place {
            Some(Place::Item {
                uuid: Some(matched),
            }) if documents_only => Some(matched.clone()),
            Some(Place::Item { .. }) => Some(row.uuid.clone()),
            None => None,
        };
        Some(Landed { row, quote })
    }

    fn keeps(&self, row: &GridRowRef) -> bool {
        self.allowed.as_ref().is_none_or(|a| a.contains(&row.uuid))
    }

    /// The first row of `document` the search keeps, its messages in the
    /// order the grid lists them, then its own row: where a hit lands when
    /// none of the messages it matched is kept, rather than on one the
    /// search drops.
    fn first_kept(&self, document: &str) -> Option<GridRowRef> {
        let rows = self.by_document.get(document)?;
        let (own, messages): (Vec<_>, Vec<_>) = rows.iter().partition(|r| r.is_document);
        messages
            .into_iter()
            .chain(own)
            .find(|r| self.keeps(r))
            .cloned()
    }

    /// Twelve lines at most of `text`, what an item says, from the first
    /// that holds one of the search's words, else from its first.
    pub fn quote(&self, text: &str) -> String {
        let lines: Vec<&str> = text.lines().collect();
        let at = lines
            .iter()
            .position(|l| {
                let line = l.to_lowercase();
                self.terms.iter().any(|t| holds(&line, t))
            })
            .unwrap_or(0);
        lines[at..]
            .iter()
            .take(12)
            .copied()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const CHAT: &str = "00000000-0000-8000-aaaa-0000000000c1";
    const OTHER: &str = "00000000-0000-8000-bbbb-0000000000d2";

    fn row(uuid: &str, kind: &str, document: &str, provider: &str) -> GridRowRef {
        GridRowRef {
            uuid: uuid.to_string(),
            kind: kind.to_string(),
            markdown_uuid: document.to_string(),
            provider: provider.to_string(),
            is_document: false,
        }
    }

    fn document(uuid: &str, kind: &str, document: &str, provider: &str) -> GridRowRef {
        GridRowRef {
            is_document: true,
            ..row(uuid, kind, document, provider)
        }
    }

    fn hit(snippet: &str) -> SearchHit {
        SearchHit {
            score: 1.0,
            snippet: snippet.to_string(),
            place: None,
        }
    }

    /// What each message of the two-message chat says, by its id, as the
    /// caller reads it from the render store for the item a hit landed on.
    fn says(uuid: &str) -> Option<&'static str> {
        match uuid {
            FIRST => Some("## Human\nfirst message text\n"),
            SECOND => Some("## Assistant\nsecond message text\n"),
            _ => None,
        }
    }

    /// The message rows of `id`'s two-message chat.
    fn two_messages(id: &str) -> Vec<GridRowRef> {
        vec![
            row(FIRST, "User Input", id, "claude"),
            row(SECOND, "LLM Response", id, "claude"),
        ]
    }

    const FIRST: &str = "aaaaaaaa-0000-0000-0000-000000000001";
    const SECOND: &str = "bbbbbbbb-0000-0000-0000-000000000002";

    /// A hit naming item `uuid`, as the search index answers.
    fn on_item(uuid: Option<&str>) -> SearchHit {
        SearchHit {
            place: Some(Place::Item {
                uuid: uuid.map(String::from),
            }),
            ..hit("")
        }
    }

    /// A hit on `CHAT` landed, and quoted from what the item it names to
    /// quote says, as the search's caller does; the hit's own snippet
    /// where there is nothing to read.
    fn land_in_chat(
        idx: &GridIndex,
        uuid: Option<&str>,
        documents_only: bool,
    ) -> Option<(GridRowRef, String)> {
        let hit = on_item(uuid);
        let landed = idx.land(CHAT, &hit, documents_only)?;
        let quoted = landed.quote.as_deref().and_then(says).map(|t| idx.quote(t));
        Some((landed.row, quoted.unwrap_or(hit.snippet)))
    }

    /// The search index names the item it matched, and the hit lands on
    /// that item's row and quotes it, whatever the text holds where: the
    /// index's text is drawn for it, not cut from the document, so its
    /// offsets are no place in it. A hit naming no item, or one the grid
    /// does not have, lands on the document.
    #[test]
    fn a_hit_lands_on_the_item_the_index_names() {
        let mut rows = two_messages(CHAT);
        rows.push(document(CHAT, "Chat", CHAT, "claude"));
        let idx = GridIndex::new(rows);
        for (uuid, says) in [(FIRST, "first message"), (SECOND, "second message")] {
            let (row, quoted) = land_in_chat(&idx, Some(uuid), false).unwrap();
            assert_eq!(row.uuid, uuid);
            assert!(quoted.contains(says), "{quoted}");
        }
        for nowhere in [None, Some("not-a-row-of-the-grid")] {
            let (row, _) = land_in_chat(&idx, nowhere, false).unwrap();
            assert!(row.is_document, "{nowhere:?} landed on {}", row.uuid);
        }
    }

    /// The quote starts at the first line of the message holding one of
    /// the search's words, a stem of it included, since the keyword index
    /// stems; with none, at the message's first line.
    #[test]
    fn a_hit_is_quoted_from_the_first_line_holding_a_search_word() {
        let rows = two_messages(CHAT);
        for terms in ["Second message", "seconds"] {
            let idx = GridIndex::new(rows.clone()).with_terms(terms);
            let (row, quoted) = land_in_chat(&idx, Some(SECOND), false).unwrap();
            assert_eq!(row.uuid, SECOND);
            assert_eq!(quoted, "second message text", "{terms}");
        }
        let idx = GridIndex::new(rows).with_terms("tea");
        let (_, quoted) = land_in_chat(&idx, Some(SECOND), false).unwrap();
        assert_eq!(quoted, "## Assistant\nsecond message text");
    }

    /// A hit on a message the search's other terms drop lands on one they
    /// keep, so the document is not lost with the message.
    #[test]
    fn a_hit_on_a_dropped_message_lands_on_a_kept_one() {
        let idx = GridIndex::new(two_messages(CHAT)).within([SECOND.to_string()].into());
        let (row, quoted) = land_in_chat(&idx, Some(FIRST), false).unwrap();
        assert_eq!(row.uuid, SECOND);
        assert!(quoted.contains("second message text"), "{quoted}");
    }

    /// A hit on a message the search drops, in a document whose own row it
    /// keeps (`with:` finds a DM by who it is named after), lands on the
    /// document's row.
    #[test]
    fn a_hit_on_a_dropped_message_lands_on_its_kept_document() {
        let mut rows = two_messages(CHAT);
        rows.push(document(CHAT, "Chat", CHAT, "claude"));
        let idx = GridIndex::new(rows).within([CHAT.to_string()].into());
        let (row, _) = land_in_chat(&idx, Some(FIRST), false).unwrap();
        assert_eq!(row.uuid, CHAT);
    }

    /// A hit lands without reading any text: on the item it names, else on
    /// a row of its document, and says which item to quote, which the
    /// caller may find it cannot read.
    #[test]
    fn a_hit_whose_text_cannot_be_read_still_lands() {
        let idx = GridIndex::new(vec![
            row("u1", "User Input", CHAT, "claude"),
            row("u2", "LLM Response", CHAT, "claude"),
            row("u3", "Chat", OTHER, "claude"),
        ]);
        let landed = idx.land(CHAT, &on_item(Some("u2")), false).unwrap();
        assert_eq!(landed.row.uuid, "u2");
        assert_eq!(landed.quote.as_deref(), Some("u2"));
        let landed = idx.land(CHAT, &on_item(None), false).unwrap();
        assert_eq!(landed.row.uuid, "u1");
    }

    /// `is:document` with free text: a hit on a message answers with the
    /// message's document, and a document without its own row answers
    /// with nothing rather than a message the SQL filter would drop.
    #[test]
    fn a_document_search_lands_on_the_documents_own_row() {
        let mut rows = two_messages(CHAT);
        let without = GridIndex::new(rows.clone());
        assert_eq!(
            land_in_chat(&without, Some(SECOND), false).unwrap().0.uuid,
            SECOND
        );
        assert!(land_in_chat(&without, Some(SECOND), true).is_none());
        rows.push(document("conv", "Chat", CHAT, "claude"));
        let with = GridIndex::new(rows);
        assert_eq!(
            land_in_chat(&with, Some(SECOND), true).unwrap().0.uuid,
            "conv"
        );
    }

    /// `is:document`: the row is the document's, but the quote is the
    /// message the hit matched, so the list shows why the document was
    /// found rather than its opening words. A hit naming no message quotes
    /// as before.
    #[test]
    fn a_document_search_quotes_the_message_it_matched() {
        let mut rows = two_messages(CHAT);
        rows.push(document(CHAT, "Chat", CHAT, "claude"));
        let idx = GridIndex::new(rows)
            .with_terms("second")
            .within([CHAT.to_string()].into());
        let (row, quoted) = land_in_chat(&idx, Some(SECOND), true).unwrap();
        assert_eq!(row.uuid, CHAT);
        assert_eq!(quoted, "second message text");
        let (row, quoted) = land_in_chat(&idx, None, true).unwrap();
        assert_eq!((row.uuid.as_str(), quoted.as_str()), (CHAT, ""));
    }

    /// A hit lands nowhere when it names a document the grid does not have.
    #[test]
    fn a_hit_on_a_document_the_grid_lacks_lands_nowhere() {
        let idx = GridIndex::new(vec![row("u1", "Chat", CHAT, "claude")]);
        let h = SearchHit {
            place: Some(Place::Item { uuid: None }),
            ..hit("")
        };
        assert!(idx.land(OTHER, &h, false).is_none());
    }
}
