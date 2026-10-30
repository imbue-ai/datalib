//! Map qmd search hits to grid rows and back. A hit's path is its
//! document's id (`grid_rows::markdown_uuid_of_path`); where in the
//! document it lands is read from the file.

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

/// One document a qmd ranking found: from its keyword index or its vectors.
#[derive(Debug, Clone)]
pub struct QmdHit {
    pub path: String,
    pub score: f64,
    pub snippet: String,
    /// How many lines from its matched line the hit covers: a Meaning
    /// chunk's. `None` for a hit on the whole document, as a keyword hit is.
    pub lines: Option<usize>,
    pub docid: String,
    pub title: String,
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

/// The rendered-markdown line a hit matched, in the file's own 1-based
/// numbering. qmd doesn't hand us a line field on every path, but both the
/// CLI and MCP snippets open with a unified-diff header
/// `@@ -N,M @@ (B before, A after)`: the match sits `B` context lines into the
/// hunk that starts at line `N`, so the matched line is `N + B`.
pub fn snippet_match_line(snippet: &str) -> Option<usize> {
    let n = take_leading_usize(&snippet[snippet.find("@@ -")? + 4..])?;
    let before = snippet
        .find(" before")
        .and_then(|end| {
            let head = &snippet[..end];
            let start = head
                .rfind(|c: char| !c.is_ascii_digit())
                .map_or(0, |i| i + 1);
            head[start..].parse::<usize>().ok()
        })
        .unwrap_or(0);
    Some(n + before)
}

fn take_leading_usize(s: &str) -> Option<usize> {
    s.chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
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

/// Which of `starts` (each section's first line, in order) a hit at 1-based
/// `line` covering `span` lines lands on, of the sections `allowed` takes
/// that overlap the span (every section when `None`, a keyword hit's): the
/// one holding the most of `terms`, the nearest to `own` of those that tie.
/// A keyword hit lands only where one of `terms` is, since its document
/// matched on words someone else may have written. `own` when it holds as
/// many and is allowed, or when no section qualifies, for the search's
/// other terms to drop.
fn best_section(
    lines: &[&str],
    starts: &[(usize, &str)],
    own: usize,
    line: usize,
    span: Option<usize>,
    terms: &[String],
    allowed: impl Fn(usize) -> bool,
) -> usize {
    if starts.is_empty() {
        return own;
    }
    let (lo, hi) = match span {
        Some(n) => (line.saturating_sub(1), line.saturating_sub(1) + n.max(1)),
        None => (0, usize::MAX),
    };
    let end_of = |i: usize| starts.get(i + 1).map_or(lines.len(), |(s, _)| *s);
    let score = |i: usize| {
        if terms.is_empty() {
            return 0;
        }
        let text = lines[starts[i].0..end_of(i)].join("\n").to_lowercase();
        terms.iter().filter(|t| holds(&text, t)).count()
    };
    if allowed(own) && score(own) == terms.len() {
        return own;
    }
    let needs_a_word = span.is_none() && !terms.is_empty();
    (0..starts.len())
        .filter(|&i| allowed(i) && starts[i].0 < hi && end_of(i) > lo)
        .map(|i| (i, score(i)))
        .filter(|&(_, n)| !needs_a_word || n > 0)
        .max_by_key(|&(i, n)| (n, std::cmp::Reverse(i.abs_diff(own))))
        .map_or(own, |(i, _)| i)
}

/// Indexes a set of `GridRowRef`s for fast hit→rows / row→hits lookup. Holds
/// the data root so a hit can be resolved to the exact message it landed in by
/// reading the rendered markdown and mapping the hit's line to a section.
pub struct GridIndex {
    root: PathBuf,
    by_uuid: HashMap<String, GridRowRef>,
    by_document: HashMap<String, Vec<GridRowRef>>,
    /// The search's words, lowercased: a hit lands on the message within
    /// its span that holds the most of them.
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

    /// Land each hit on the message holding the most of `free_text`'s
    /// words, rather than on the line the hit names.
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

    pub fn rows_for_hit(&self, hit: &QmdHit) -> Vec<GridRowRef> {
        let Some(file_rows) =
            markdown_uuid_of_path(&hit.path).and_then(|d| self.by_document.get(d))
        else {
            return Vec::new();
        };
        let kept = |r: &GridRowRef| self.allowed.as_ref().is_none_or(|a| a.contains(&r.uuid));
        if let Some(uuid) = self.section_for_line(hit) {
            if let Some(row) = self.by_uuid.get(uuid.as_str()).filter(|r| kept(r)) {
                return vec![row.clone()];
            }
        }
        // A hit that cannot be placed inside the document, or only on a
        // message the search drops, is a hit on the document: its row,
        // when the file has one, else every row.
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

    /// [`Self::rows_for_hit`] lifted to the document: whichever row a
    /// hit lands on, the row that *is* that document. What an
    /// `is:document` search wants from a hit inside a message — the
    /// message row would only be filtered out afterwards. Empty when the
    /// file has no document row, so an orphan stays an orphan.
    pub fn document_for_hit(&self, hit: &QmdHit) -> Vec<GridRowRef> {
        self.rows_for_hit(hit)
            .first()
            .and_then(|r| self.by_document.get(&r.markdown_uuid))
            .into_iter()
            .flatten()
            .filter(|r| r.is_document)
            .cloned()
            .collect()
    }

    /// Read the hit's file and return the `data-section-uuid` of the
    /// message the hit lands on: of the messages within the hit's span (the
    /// whole document, or a Meaning chunk's lines), the one holding the most
    /// of the search's words. Without words, or when its own message holds
    /// as many, the message covering the matched line: the last anchor at or
    /// before it, or the first when the hit is above every message (title,
    /// front matter). `None` if the line can't be derived or the file can't
    /// be read.
    fn section_for_line(&self, hit: &QmdHit) -> Option<String> {
        let line = snippet_match_line(&hit.snippet)?;
        let text = std::fs::read_to_string(self.root.join(&hit.path)).ok()?;
        let lines: Vec<&str> = text.lines().collect();
        let starts: Vec<(usize, &str)> = lines
            .iter()
            .enumerate()
            .filter_map(|(i, raw)| Some((i, parse_section_uuid(raw)?)))
            .collect();
        let own = starts.iter().rposition(|(i, _)| *i < line).unwrap_or(0);
        let allowed = |i: usize| {
            self.allowed
                .as_ref()
                .is_none_or(|rows| rows.contains(starts[i].1))
        };
        let chosen = best_section(&lines, &starts, own, line, hit.lines, &self.terms, allowed);
        starts.get(chosen).map(|(_, uuid)| uuid.to_string())
    }

    /// Aggregate over hits, preserving rank order, deduping by uuid.
    pub fn rows_for_hits<'a, I: IntoIterator<Item = &'a QmdHit>>(
        &self,
        hits: I,
    ) -> Vec<GridRowRef> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<GridRowRef> = Vec::new();
        for h in hits {
            for r in self.rows_for_hit(h) {
                if seen.insert(r.uuid.clone()) {
                    out.push(r);
                }
            }
        }
        out
    }

    /// Map ranked hits to grid rows for display, **one row per markdown
    /// document**: the highest-ranked hit that lands in a document wins, and
    /// lower-ranked hits from the same document are dropped to keep the result
    /// list concise. Returns `(row, score)` in rank order, each row carrying
    /// the score of the hit that produced it.
    pub fn ranked_rows_one_per_doc<'h>(
        &self,
        hits: &'h [QmdHit],
        documents_only: bool,
        mut on_orphan: impl FnMut(&QmdHit),
    ) -> Vec<(GridRowRef, &'h QmdHit)> {
        let mut seen_docs: HashSet<String> = HashSet::new();
        let mut out: Vec<(GridRowRef, &'h QmdHit)> = Vec::new();
        for h in hits {
            let rows = if documents_only {
                self.document_for_hit(h)
            } else {
                self.rows_for_hit(h)
            };
            if rows.is_empty() {
                on_orphan(h);
                continue;
            }
            // All rows from a single hit are of one document, so this keeps
            // the first — and, because we also skip documents seen via
            // earlier (higher-ranked) hits, the output holds at most one
            // row per document.
            for row in rows {
                if seen_docs.insert(row.markdown_uuid.clone()) {
                    out.push((row, h));
                }
            }
        }
        out
    }

    /// Reverse: which of `hits` resolve to `row`? Defined in terms of
    /// `rows_for_hit` so it always tracks the forward mapping.
    pub fn hits_for_row<'a>(&self, row: &GridRowRef, hits: &'a [QmdHit]) -> Vec<&'a QmdHit> {
        hits.iter()
            .filter(|h| self.rows_for_hit(h).iter().any(|r| r.uuid == row.uuid))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_schema::grid_rows::doc_file_rel;

    const CHAT: &str = "00000000-0000-8000-aaaa-0000000000c1";
    const OTHER: &str = "00000000-0000-8000-bbbb-0000000000d2";

    /// Where a source files document `id`: what qmd's hit names.
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
            lines: None,
            docid: String::new(),
            title: String::new(),
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

    /// Four messages, one line of header and one of text each.
    const AWAY_TEAM: &str = "<div data-section-uuid=\"m1\">\nRiker: the warp core\n\
        <div data-section-uuid=\"m2\">\nData: shields at forty percent\n\
        <div data-section-uuid=\"m3\">\nWorf: warp core breach imminent\n\
        <div data-section-uuid=\"m4\">\nPicard: make it so\n";

    fn landing(line: usize, span: Option<usize>, terms: &str) -> &'static str {
        let lines: Vec<&str> = AWAY_TEAM.lines().collect();
        let starts: Vec<(usize, &str)> = lines
            .iter()
            .enumerate()
            .filter_map(|(i, raw)| Some((i, parse_section_uuid(raw)?)))
            .collect();
        let own = starts.iter().rposition(|(i, _)| *i < line).unwrap_or(0);
        let mut terms = words(terms);
        terms.dedup();
        let at = best_section(&lines, &starts, own, line, span, &terms, |_| true);
        ["m1", "m2", "m3", "m4"][at]
    }

    fn landing_within(line: usize, terms: &str, kept: &[&str]) -> &'static str {
        let lines: Vec<&str> = AWAY_TEAM.lines().collect();
        let starts: Vec<(usize, &str)> = lines
            .iter()
            .enumerate()
            .filter_map(|(i, raw)| Some((i, parse_section_uuid(raw)?)))
            .collect();
        let own = starts.iter().rposition(|(i, _)| *i < line).unwrap_or(0);
        let terms = words(terms);
        let at = best_section(&lines, &starts, own, line, None, &terms, |i| {
            kept.contains(&starts[i].1)
        });
        ["m1", "m2", "m3", "m4"][at]
    }

    /// A hit lands only on a message the search's other terms keep and, for
    /// a keyword hit, one holding a word of the search: the one holding the
    /// most, the nearest of those that tie, and its own when none qualifies,
    /// for those terms to drop.
    #[test]
    fn a_hit_lands_on_a_message_the_other_terms_keep() {
        assert_eq!(landing_within(2, "warp core breach", &["m3", "m4"]), "m3");
        // Neither kept message holds a word, so the hit stays to be dropped.
        assert_eq!(landing_within(2, "warp core breach", &["m2", "m4"]), "m1");
        // A keyword hit on Picard's line may land above it, on Riker's.
        assert_eq!(landing_within(8, "warp core", &["m1", "m4"]), "m1");
        assert_eq!(landing_within(2, "", &["m4"]), "m4");
        assert_eq!(landing_within(2, "warp", &[]), "m1");
    }

    /// A hit lands on the message holding the most of the search's words,
    /// stays on its own when that one holds as many or none holds any, and
    /// a Meaning hit never leaves its chunk.
    #[test]
    fn a_hit_lands_on_the_message_with_the_most_of_the_words() {
        // The keyword index named Riker's line, where "warp" first appears.
        assert_eq!(landing(2, None, "warp core breach"), "m3");
        assert_eq!(landing(2, None, "warp core"), "m1", "a tie keeps its own");
        assert_eq!(landing(2, None, "tea earl grey"), "m1", "nothing holds any");
        assert_eq!(landing(2, None, ""), "m1");
        // A chunk of Data's and Picard's lines cannot land on Worf's.
        assert_eq!(landing(3, Some(2), "warp core breach"), "m2");
        assert_eq!(landing(5, Some(4), "warp core breach"), "m3");
        // Stems: the index found "imminently" for "imminent".
        assert_eq!(landing(2, None, "imminently"), "m3");
    }

    #[test]
    fn snippet_match_line_reads_diff_header() {
        // `@@ -N,M @@ (B before, …)` → N + B.
        assert_eq!(
            snippet_match_line("@@ -10,4 @@ (9 before, 51 after)\n<h1>…"),
            Some(19)
        );
        // MCP prefixes each line with `LABEL: ` — the header is still in there.
        assert_eq!(
            snippet_match_line("11: @@ -23,4 @@ (2 before, 5 after)\n12: x"),
            Some(25)
        );
        // No "before" annotation → fall back to the hunk start.
        assert_eq!(snippet_match_line("@@ -7,4 @@\nx"), Some(7));
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

    /// A hit on a message the search's other terms drop lands on one they
    /// keep, so the document is not lost with the message.
    #[test]
    fn rows_for_hit_lands_within_the_rows_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let second = "bbbbbbbb-0000-0000-0000-000000000002";
        let idx = GridIndex::new(tmp.path(), write_two_message_doc(tmp.path(), CHAT))
            .within([second.to_string()].into());
        // Line 7 is inside the first message.
        let h = hit(
            &path_of("claude", CHAT),
            "@@ -7,1 @@ (0 before, 0 after)\n## Human",
        );
        let got = idx.rows_for_hit(&h);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uuid, second);
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
        let h = hit(
            &path_of("claude", CHAT),
            "@@ -7,1 @@ (0 before, 0 after)\n## Human",
        );
        let got = idx.rows_for_hit(&h);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uuid, CHAT);
    }

    #[test]
    fn rows_for_hit_pins_message_by_line() {
        // A hit whose diff header lands in the *second* message resolves to
        // exactly that one row — no snippet anchor required.
        let tmp = tempfile::tempdir().unwrap();
        let idx = GridIndex::new(tmp.path(), write_two_message_doc(tmp.path(), CHAT));
        // `@@ -13,4 @@ (2 before, …)` → matched line 15, inside the 2nd message.
        let h = hit(
            &path_of("claude", CHAT),
            "@@ -13,4 @@ (2 before, 3 after)\n## Assistant",
        );
        let got = idx.rows_for_hit(&h);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uuid, "bbbbbbbb-0000-0000-0000-000000000002");
    }

    #[test]
    fn rows_for_hit_title_region_resolves_to_first_message() {
        // The motivating bug: a hit on the conversation *title* (above the
        // first message) must resolve to the first message, not fan out.
        let tmp = tempfile::tempdir().unwrap();
        let idx = GridIndex::new(tmp.path(), write_two_message_doc(tmp.path(), CHAT));
        // `@@ -4,4 @@ (1 before, …)` → matched line 5 (the <h1>), before any anchor.
        let h = hit(
            &path_of("claude", CHAT),
            "@@ -4,4 @@ (1 before, 10 after)\n<h1>Reactive data pipeline composition in Rust</h1>",
        );
        let got = idx.rows_for_hit(&h);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uuid, "aaaaaaaa-0000-0000-0000-000000000001");
    }

    #[test]
    fn rows_for_hit_unreadable_file_falls_back_to_whole_doc() {
        // File missing on disk → can't pin a section, but the document must
        // still surface: every row of the document comes back.
        let idx = GridIndex::new(
            "/nonexistent-root",
            vec![
                row("u1", "User Input", CHAT, "claude"),
                row("u2", "LLM Response", CHAT, "claude"),
                row("u3", "Chat", OTHER, "claude"),
            ],
        );
        let h = hit(
            &path_of("claude", CHAT),
            "@@ -9,4 @@ (2 before, 5 after)\nx",
        );
        let got = idx.rows_for_hit(&h);
        let uuids: HashSet<&str> = got.iter().map(|r| r.uuid.as_str()).collect();
        assert_eq!(uuids, ["u1", "u2"].into_iter().collect());
    }

    /// With a document row in the file, an unplaceable hit is a hit on
    /// the document rather than on every row.
    #[test]
    fn rows_for_hit_unplaceable_prefers_the_document_row() {
        let idx = GridIndex::new(
            "/nonexistent-root",
            vec![
                row("u1", "User Input", CHAT, "claude"),
                document("doc", "Chat", CHAT, "claude"),
                row("u2", "LLM Response", CHAT, "claude"),
            ],
        );
        let h = hit(
            &path_of("claude", CHAT),
            "@@ -9,4 @@ (2 before, 5 after)\nx",
        );
        let got = idx.rows_for_hit(&h);
        let uuids: Vec<&str> = got.iter().map(|r| r.uuid.as_str()).collect();
        assert_eq!(uuids, ["doc"]);
    }

    /// `is:document` with free text: a hit pinned to a message still
    /// answers with the message's document, and a file without a
    /// document row answers with nothing rather than a message that the
    /// SQL filter would then drop.
    #[test]
    fn document_for_hit_lifts_a_pinned_message_to_its_document() {
        let td = tempfile::tempdir().unwrap();
        let mut rows = write_two_message_doc(td.path(), CHAT);
        let h = hit(
            &path_of("claude", CHAT),
            "@@ -13,4 @@ (2 before, 3 after)\n## Assistant",
        );
        let pinned = GridIndex::new(td.path(), rows.clone()).rows_for_hit(&h);
        assert_eq!(
            pinned.iter().map(|r| r.uuid.as_str()).collect::<Vec<_>>(),
            ["bbbbbbbb-0000-0000-0000-000000000002"],
            "the hit itself lands on the second message"
        );
        assert!(
            GridIndex::new(td.path(), rows.clone())
                .document_for_hit(&h)
                .is_empty(),
            "no document row, no document"
        );
        rows.push(document("conv", "Chat", CHAT, "claude"));
        let lifted = GridIndex::new(td.path(), rows).document_for_hit(&h);
        assert_eq!(
            lifted.iter().map(|r| r.uuid.as_str()).collect::<Vec<_>>(),
            ["conv"]
        );
    }

    /// A hit names no rows when its path is not filed by an id, or names a
    /// document the grid does not have — the signal callers log as an
    /// error.
    #[test]
    fn rows_for_hit_unknown_document_is_empty() {
        let idx = GridIndex::new("/nonexistent-root", vec![row("u1", "Chat", CHAT, "claude")]);
        let snippet = "@@ -1,4 @@ (0 before, 1 after)\nx";
        assert!(idx
            .rows_for_hit(&hit(&path_of("claude", OTHER), snippet))
            .is_empty());
        assert!(idx
            .rows_for_hit(&hit(
                &format!("claude/render_markdown/{CHAT}/all.md"),
                snippet
            ))
            .is_empty());
    }

    #[test]
    fn rows_for_hits_dedupes_across_hits() {
        // Two hits into the same single-row doc → one row out. (No file on
        // disk, so both resolve via the whole-doc fallback.)
        let idx = GridIndex::new(
            "/nonexistent-root",
            vec![row(
                "11111111-2222-3333-4444-555555555555",
                "LLM Response",
                CHAT,
                "claude",
            )],
        );
        let h1 = hit(
            &path_of("claude", CHAT),
            "@@ -1,4 @@ (0 before, 1 after)\nx",
        );
        let h2 = hit(
            &path_of("claude", CHAT),
            "@@ -1,4 @@ (0 before, 1 after)\nx",
        );
        let got = idx.rows_for_hits(&[h1, h2]);
        assert_eq!(got.len(), 1);
    }

    /// Replay of the applet's `qmd_ranking` fanout: walk hits in rank order,
    /// stamp each hit's score onto every row it resolves to, first-score-wins.
    /// Returns `(uuid, score)` in the order rows are discovered. Kept in the
    /// test so this regression is self-contained (the real loop is inline in
    /// the applet crate).
    fn fanout(idx: &GridIndex, hits: &[QmdHit]) -> Vec<(String, f64)> {
        let mut seen: HashMap<String, f64> = HashMap::new();
        let mut out: Vec<(String, f64)> = Vec::new();
        for h in hits {
            for r in idx.rows_for_hit(h) {
                if !seen.contains_key(&r.uuid) {
                    seen.insert(r.uuid.clone(), h.score);
                    out.push((r.uuid, h.score));
                }
            }
        }
        out
    }

    fn scored_hit(path: &str, score: f64, snippet: &str) -> QmdHit {
        QmdHit {
            path: path.to_string(),
            score,
            snippet: snippet.to_string(),
            lines: None,
            docid: String::new(),
            title: String::new(),
        }
    }

    /// Regression: the genuinely-relevant Claude chat — qmd's #1 hit (score
    /// 1.0) — must outrank a poorly-matched Slack post that merely mentions
    /// "rust" (qmd's #2 hit, score 0.5) in the grid.
    #[test]
    fn relevant_chat_outranks_spam() {
        let chat_user = "019e2d00-aad9-7ecd-9cfd-b1cd1648b98f"; // the great result
        let chat_llm = "019e2d00-aad9-7933-8639-51c8474c1b11";
        let spam_msg = "6d10c99d-1219-50e0-9d00-84979fd99d1d"; // the terrible result

        // No real files on disk, so every hit resolves via the whole-doc
        // fallback — enough to exercise the hit→score mapping.
        let idx = GridIndex::new(
            "/nonexistent-root",
            vec![
                row(spam_msg, "Slack Message", OTHER, "slack"),
                row(chat_user, "User Input", CHAT, "claude"),
                row(chat_llm, "LLM Response", CHAT, "claude"),
            ],
        );
        let no_anchor = "@@ -10,4 @@\n<h1 data-page-title-uuid=\"b0c2f022\">Reactive data pipeline composition in Rust</h1>";
        let hits = vec![
            scored_hit(&path_of("claude", CHAT), 1.0, no_anchor),
            scored_hit(
                &path_of("slack", OTHER),
                0.5,
                "@@ -23,4 @@\nthe rust cli that sped up your build",
            ),
        ];

        let ranked = fanout(&idx, &hits);
        let user_score = ranked.iter().find(|(u, _)| u == chat_user).map(|(_, s)| *s);
        assert_eq!(user_score, Some(1.0));

        // The frontend sorts by score desc (stable). Emulate it.
        let mut shown = ranked.clone();
        shown.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let order: Vec<&str> = shown.iter().map(|(u, _)| u.as_str()).collect();
        let spam_pos = order.iter().position(|u| *u == spam_msg).unwrap();
        let chat_pos = order.iter().position(|u| *u == chat_user).unwrap();
        assert!(
            chat_pos < spam_pos,
            "chat at {chat_pos}, spam at {spam_pos}: {order:?}"
        );
    }

    /// A single document that matches in several places collapses to one row —
    /// its top-ranked hit — so the result list stays concise. Lower-ranked
    /// hits into the same document are dropped; orphan hits fire the callback
    /// and contribute nothing.
    #[test]
    fn ranked_rows_one_per_doc_keeps_top_hit_per_document() {
        let tmp = tempfile::tempdir().unwrap();
        // A two-message chat (anchors at lines 7 and 13) ...
        let mut rows = write_two_message_doc(tmp.path(), CHAT);
        // ... plus a second, single-message document (anchor at line 5).
        let other_path = tmp.path().join(path_of("slack", OTHER));
        std::fs::create_dir_all(other_path.parent().unwrap()).unwrap();
        std::fs::write(
            &other_path,
            "---\nprovider: slack\n---\n\n<div id=\"m-cccccccc-0000-0000-0000-000000000003\" \
             data-section-uuid=\"cccccccc-0000-0000-0000-000000000003\" class=\"msg\">\n\n\
             slack body\n</div>\n",
        )
        .unwrap();
        rows.push(row(
            "cccccccc-0000-0000-0000-000000000003",
            "Slack Message",
            OTHER,
            "slack",
        ));
        let idx = GridIndex::new(tmp.path(), rows);

        // Hits in rank order:
        //   #1 (1.0)  pins the chat's 2nd message (matched line 15).
        //   #2 (0.5)  lands in the other document.
        //   #3 (0.25) pins the chat's 1st message — same doc as #1, dropped.
        //   #4 (0.1)  a document the grid doesn't know — orphan.
        let chat = path_of("claude", CHAT);
        let hits = vec![
            scored_hit(&chat, 1.0, "@@ -13,4 @@ (2 before, 3 after)\n## Assistant"),
            scored_hit(
                &path_of("slack", OTHER),
                0.5,
                "@@ -7,4 @@ (0 before, 1 after)\nslack body",
            ),
            scored_hit(&chat, 0.25, "@@ -7,4 @@ (0 before, 1 after)\n## Human"),
            scored_hit(
                &path_of("slack", "00000000-0000-8000-cccc-0000000000e3"),
                0.1,
                "@@ -1,4 @@ (0 before, 1 after)\nx",
            ),
        ];

        let mut orphans = 0;
        let ranked = idx.ranked_rows_one_per_doc(&hits, false, |_| orphans += 1);

        assert_eq!(orphans, 1, "the unknown document's hit is an orphan");
        let got: Vec<(&str, f64)> = ranked
            .iter()
            .map(|(r, h)| (r.uuid.as_str(), h.score))
            .collect();
        assert_eq!(
            got,
            vec![
                // The chat once, at its best rank (1.0), pinned to the 2nd msg.
                ("bbbbbbbb-0000-0000-0000-000000000002", 1.0),
                // The other doc once. The lower-ranked #3 hit is absent.
                ("cccccccc-0000-0000-0000-000000000003", 0.5),
            ]
        );
    }
}
