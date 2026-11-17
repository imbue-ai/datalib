//! Land a search hit on a grid row. A hit's path is its document's file,
//! which names its id (`grid_rows::markdown_uuid_of_path`); the search
//! index names the item it matched, and the quote is read from the file.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use datalib_schema::grid_rows::markdown_uuid_of_path;

/// Which collections a search may draw from: every one, or exactly these.
/// There is one qmd collection per group, so these are group ids; an empty
/// list matches nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectionScope {
    All,
    Only(Vec<String>),
}

impl CollectionScope {
    /// The names to scope to, or `None` for an unscoped search.
    pub fn names(&self) -> Option<&[String]> {
        match self {
            CollectionScope::All => None,
            CollectionScope::Only(names) => Some(names),
        }
    }
}

/// One document a ranking found: by its words or by its meaning.
#[derive(Debug, Clone)]
pub struct QmdHit {
    /// The document's file, relative to the data root; empty until the
    /// page that shows the hit looks it up.
    pub path: String,
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

pub fn parse_section_uuid(line: &str) -> Option<&str> {
    const KEY: &str = "data-section-uuid=\"";
    let start = line.find(KEY)? + KEY.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
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

/// `lines`, the first of them line `line` of the file, as a hit's snippet:
/// under a `@@ -line,1 @@` header, twelve at most.
fn quoted_from(lines: &[&str], line: usize) -> String {
    let text: Vec<&str> = lines.iter().take(12).copied().collect();
    format!("@@ -{line},1 @@ (0 before, 0 after)\n{}", text.join("\n"))
}

/// A set of `GridRowRef`s, looked up by row and by document, and the data
/// root, whose files a hit's quote is read from.
pub struct GridIndex {
    root: PathBuf,
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
    pub fn new(root: impl Into<PathBuf>, rows: impl IntoIterator<Item = GridRowRef>) -> Self {
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
            root: root.into(),
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

    /// The rows `hit` lands on: the item it names when the search keeps
    /// it, else the document's own row, or every row of a document that
    /// has none. Empty for a document the grid does not have.
    fn rows_for_hit(&self, hit: &QmdHit) -> Vec<GridRowRef> {
        let Some(file_rows) =
            markdown_uuid_of_path(&hit.path).and_then(|d| self.by_document.get(d))
        else {
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
    fn document_for_hit(&self, hit: &QmdHit) -> Vec<GridRowRef> {
        self.rows_for_hit(hit)
            .first()
            .and_then(|r| self.by_document.get(&r.markdown_uuid))
            .into_iter()
            .flatten()
            .filter(|r| r.is_document)
            .cloned()
            .collect()
    }

    /// The row a hit lands on, with the lines quoted from its message,
    /// reading the document's file once: the work a page does for each hit
    /// it shows, and only for those. The document's own row for an
    /// `is:document` search, quoting the message the hit matched.
    /// `None` for a document the grid does not have.
    pub fn land(&self, hit: &QmdHit, documents_only: bool) -> Option<(GridRowRef, String)> {
        let text = std::fs::read_to_string(self.root.join(&hit.path)).ok();
        let rows = if documents_only {
            self.document_for_hit(hit)
        } else {
            self.rows_for_hit(hit)
        };
        let row = rows.into_iter().next()?;
        let row = if self.keeps(&row) {
            row
        } else {
            self.first_kept(text.as_deref(), &row.markdown_uuid)
                .unwrap_or(row)
        };
        let quoted_section = match &hit.place {
            Some(Place::Item {
                uuid: Some(matched),
            }) if documents_only => matched.as_str(),
            _ => row.uuid.as_str(),
        };
        let quoted = match (&text, &hit.place) {
            (Some(text), Some(Place::Item { .. })) => self.quote(text, quoted_section),
            _ => None,
        };
        Some((row, quoted.unwrap_or_else(|| hit.snippet.clone())))
    }

    fn keeps(&self, row: &GridRowRef) -> bool {
        self.allowed.as_ref().is_none_or(|a| a.contains(&row.uuid))
    }

    /// The first row of `document` the search keeps, its messages in the
    /// order the file has them, then its own row: where a hit lands when
    /// none of the messages it matched is kept, rather than on one the
    /// search drops.
    fn first_kept(&self, text: Option<&str>, document: &str) -> Option<GridRowRef> {
        let rows = self.by_document.get(document)?;
        let in_order = text
            .into_iter()
            .flat_map(str::lines)
            .filter_map(parse_section_uuid)
            .filter_map(|uuid| self.by_uuid.get(uuid));
        in_order.chain(rows).find(|r| self.keeps(r)).cloned()
    }

    /// The lines of message `uuid` from the first that holds one of the
    /// search's words.
    fn quote(&self, text: &str, uuid: &str) -> Option<String> {
        let lines: Vec<&str> = text.lines().collect();
        let start = lines
            .iter()
            .position(|l| parse_section_uuid(l) == Some(uuid))?;
        let end = lines[start + 1..]
            .iter()
            .position(|l| parse_section_uuid(l).is_some())
            .map_or(lines.len(), |i| start + 1 + i);
        let at = (start + 1..end)
            .find(|&i| {
                let line = lines[i].to_lowercase();
                self.terms.iter().any(|t| holds(&line, t))
            })
            .unwrap_or(start);
        Some(quoted_from(&lines[at..end], at + 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_schema::grid_rows::doc_file_rel;

    const CHAT: &str = "00000000-0000-8000-aaaa-0000000000c1";
    const OTHER: &str = "00000000-0000-8000-bbbb-0000000000d2";

    /// Where a source files document `id`: what a hit's path names.
    fn path_of(source: &str, id: &str) -> String {
        format!("{source}/render_markdown/{}", doc_file_rel(id))
    }

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

    fn hit(path: &str, snippet: &str) -> QmdHit {
        QmdHit {
            path: path.to_string(),
            score: 1.0,
            snippet: snippet.to_string(),
            place: None,
        }
    }

    #[test]
    fn parse_section_uuid_pulls_value() {
        let line = "<div id=\"m-11111111-2222-3333-4444-555555555555\" \
                    data-section-uuid=\"11111111-2222-3333-4444-555555555555\" class=\"msg\">";
        assert_eq!(
            parse_section_uuid(line),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(parse_section_uuid("## Human"), None);
        // `th-`/`tu-`/`tr-` block ids are returned verbatim.
        assert_eq!(
            parse_section_uuid("<div data-section-uuid=\"th-abc-1\">"),
            Some("th-abc-1")
        );
    }

    /// Writes `id`'s two-message chat where claude files it, and answers
    /// its message rows.
    fn write_two_message_doc(root: &std::path::Path, id: &str) -> Vec<GridRowRef> {
        let body = "---\n\
                    provider: claude\n\
                    ---\n\
                    \n\
                    <h1 class=\"page-title\">Reactive data pipeline composition in Rust</h1>\n\
                    \n\
                    <div id=\"m-aaaaaaaa-0000-0000-0000-000000000001\" data-section-uuid=\"aaaaaaaa-0000-0000-0000-000000000001\" class=\"msg\">\n\
                    \n\
                    ## Human\n\
                    first message text\n\
                    </div>\n\
                    \n\
                    <div id=\"m-bbbbbbbb-0000-0000-0000-000000000002\" data-section-uuid=\"bbbbbbbb-0000-0000-0000-000000000002\" class=\"msg\">\n\
                    \n\
                    ## Assistant\n\
                    second message text\n\
                    </div>\n";
        let path = root.join(path_of("claude", id));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        vec![
            row(
                "aaaaaaaa-0000-0000-0000-000000000001",
                "User Input",
                id,
                "claude",
            ),
            row(
                "bbbbbbbb-0000-0000-0000-000000000002",
                "LLM Response",
                id,
                "claude",
            ),
        ]
    }

    /// The search index names the item it matched, and the hit lands on
    /// that item's row and quotes it, whatever the file holds where: the
    /// index's text is drawn for it, not cut from the file, so its offsets
    /// are no place in the file. A hit naming no item, or one the grid
    /// does not have, lands on the document.
    #[test]
    fn a_hit_lands_on_the_item_the_index_names() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rows = write_two_message_doc(tmp.path(), CHAT);
        rows.push(document(CHAT, "Chat", CHAT, "claude"));
        let idx = GridIndex::new(tmp.path(), rows);
        let named = |uuid: Option<&str>| QmdHit {
            place: Some(Place::Item {
                uuid: uuid.map(String::from),
            }),
            ..hit(&path_of("claude", CHAT), "")
        };
        for (uuid, says) in [
            ("aaaaaaaa-0000-0000-0000-000000000001", "first message"),
            ("bbbbbbbb-0000-0000-0000-000000000002", "second message"),
        ] {
            let (row, quoted) = idx.land(&named(Some(uuid)), false).unwrap();
            assert_eq!(row.uuid, uuid);
            assert!(quoted.contains(says), "{quoted}");
        }
        for nowhere in [None, Some("not-a-row-of-the-grid")] {
            let (row, _) = idx.land(&named(nowhere), false).unwrap();
            assert!(row.is_document, "{nowhere:?} landed on {}", row.uuid);
        }
    }

    const FIRST: &str = "aaaaaaaa-0000-0000-0000-000000000001";
    const SECOND: &str = "bbbbbbbb-0000-0000-0000-000000000002";

    /// A hit on `CHAT` naming item `uuid`, as the search index answers.
    fn on_item(uuid: Option<&str>) -> QmdHit {
        QmdHit {
            place: Some(Place::Item {
                uuid: uuid.map(String::from),
            }),
            ..hit(&path_of("claude", CHAT), "")
        }
    }

    /// The quote starts at the first line of the message holding one of
    /// the search's words, a stem of it included, since the keyword index
    /// stems; with none, at the message's first line.
    #[test]
    fn a_hit_is_quoted_from_the_first_line_holding_a_search_word() {
        let tmp = tempfile::tempdir().unwrap();
        let rows = write_two_message_doc(tmp.path(), CHAT);
        for terms in ["Second message", "seconds"] {
            let idx = GridIndex::new(tmp.path(), rows.clone()).with_terms(terms);
            let (row, quoted) = idx.land(&on_item(Some(SECOND)), false).unwrap();
            assert_eq!(row.uuid, SECOND);
            assert!(quoted.starts_with("@@ -16,1 @@"), "{terms}: {quoted}");
            assert!(quoted.contains("second message text"), "{quoted}");
        }
        let idx = GridIndex::new(tmp.path(), rows).with_terms("tea");
        let (_, quoted) = idx.land(&on_item(Some(SECOND)), false).unwrap();
        assert!(quoted.starts_with("@@ -13,1 @@"), "{quoted}");
        assert!(quoted.contains("## Assistant"), "{quoted}");
    }

    /// A hit on a message the search's other terms drop lands on one they
    /// keep, so the document is not lost with the message.
    #[test]
    fn a_hit_on_a_dropped_message_lands_on_a_kept_one() {
        let tmp = tempfile::tempdir().unwrap();
        let idx = GridIndex::new(tmp.path(), write_two_message_doc(tmp.path(), CHAT))
            .within([SECOND.to_string()].into());
        let (row, quoted) = idx.land(&on_item(Some(FIRST)), false).unwrap();
        assert_eq!(row.uuid, SECOND);
        assert!(quoted.contains("second message text"), "{quoted}");
    }

    /// A hit on a message the search drops, in a document whose own row it
    /// keeps (`with:` finds a DM by who it is named after), lands on the
    /// document's row.
    #[test]
    fn a_hit_on_a_dropped_message_lands_on_its_kept_document() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rows = write_two_message_doc(tmp.path(), CHAT);
        rows.push(document(CHAT, "Chat", CHAT, "claude"));
        let idx = GridIndex::new(tmp.path(), rows).within([CHAT.to_string()].into());
        let (row, _) = idx.land(&on_item(Some(FIRST)), false).unwrap();
        assert_eq!(row.uuid, CHAT);
    }

    /// The file is gone (a render since the index was written): the hit
    /// still lands, on the item it names, else on a row of its document,
    /// quoting nothing.
    #[test]
    fn a_hit_whose_file_cannot_be_read_still_lands() {
        let idx = GridIndex::new(
            "/nonexistent-root",
            vec![
                row("u1", "User Input", CHAT, "claude"),
                row("u2", "LLM Response", CHAT, "claude"),
                row("u3", "Chat", OTHER, "claude"),
            ],
        );
        let (row, quoted) = idx.land(&on_item(Some("u2")), false).unwrap();
        assert_eq!((row.uuid.as_str(), quoted.as_str()), ("u2", ""));
        let (row, quoted) = idx.land(&on_item(None), false).unwrap();
        assert_eq!((row.uuid.as_str(), quoted.as_str()), ("u1", ""));
    }

    /// `is:document` with free text: a hit on a message answers with the
    /// message's document, and a document without its own row answers
    /// with nothing rather than a message the SQL filter would drop.
    #[test]
    fn a_document_search_lands_on_the_documents_own_row() {
        let td = tempfile::tempdir().unwrap();
        let mut rows = write_two_message_doc(td.path(), CHAT);
        let without = GridIndex::new(td.path(), rows.clone());
        assert_eq!(
            without.land(&on_item(Some(SECOND)), false).unwrap().0.uuid,
            SECOND
        );
        assert!(without.land(&on_item(Some(SECOND)), true).is_none());
        rows.push(document("conv", "Chat", CHAT, "claude"));
        let with = GridIndex::new(td.path(), rows);
        assert_eq!(
            with.land(&on_item(Some(SECOND)), true).unwrap().0.uuid,
            "conv"
        );
    }

    /// `is:document`: the row is the document's, but the quote is the
    /// message the hit matched, so the list shows why the document was
    /// found rather than its opening words. A hit naming no message quotes
    /// as before.
    #[test]
    fn a_document_search_quotes_the_message_it_matched() {
        let td = tempfile::tempdir().unwrap();
        let mut rows = write_two_message_doc(td.path(), CHAT);
        rows.push(document(CHAT, "Chat", CHAT, "claude"));
        let idx = GridIndex::new(td.path(), rows)
            .with_terms("second")
            .within([CHAT.to_string()].into());
        let (row, quoted) = idx.land(&on_item(Some(SECOND)), true).unwrap();
        assert_eq!(row.uuid, CHAT);
        assert!(quoted.starts_with("@@ -16,1 @@"), "{quoted}");
        assert!(quoted.contains("second message text"), "{quoted}");
        let (row, quoted) = idx.land(&on_item(None), true).unwrap();
        assert_eq!((row.uuid.as_str(), quoted.as_str()), (CHAT, ""));
    }

    /// A hit lands nowhere when its path names a document the grid does
    /// not have, or is not filed by an id at all.
    #[test]
    fn a_hit_on_a_document_the_grid_lacks_lands_nowhere() {
        let idx = GridIndex::new("/nonexistent-root", vec![row("u1", "Chat", CHAT, "claude")]);
        for path in [
            path_of("claude", OTHER),
            format!("claude/render_markdown/{CHAT}/all.md"),
        ] {
            let h = QmdHit {
                place: Some(Place::Item { uuid: None }),
                ..hit(&path, "")
            };
            assert!(idx.land(&h, false).is_none(), "{path}");
        }
    }
}
