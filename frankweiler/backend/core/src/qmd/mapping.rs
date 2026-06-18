//! Map qmd search hits to grid rows and back.
//!
//! Hit→row finds the hit's document by normalized `qmd_path`, then reads that
//! rendered markdown and maps the hit's matched line (parsed from the snippet's
//! `@@ -N,M @@` diff header) to the enclosing `data-section-uuid` — i.e. the
//! exact message. When the line can't be pinned (no header, file unreadable,
//! or the section isn't a grid row) it falls back to every row of the
//! document; when the document matches no rows at all it returns nothing.
//! qmd lowercases paths and collapses runs of `_`/`-` to a single `-` in its
//! internal docid URI, so the same normalization applies on the grid side.
//!
//! `parse_query` recognizes `qmd:"…"` and `qmd_vsearch:"…"` as predicates
//! over the search-bar string; anything else is treated as bare hybrid
//! query text. The broader search-bar parser in `crate::query` calls
//! this after handling structured `field:value` filters.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    /// Hybrid BM25 + vector + reranker. Default for bare search-bar text.
    Hybrid,
    /// Vector-only. Faster, no LLM reranking.
    Vsearch,
}

/// One result from a `qmd query` / `qmd vsearch` run.
///
/// `path` is the file path qmd reports inside its `qmd://<collection>/…`
/// URI, already stripped of the URI prefix and normalized (lowercased,
/// `[_-]+` collapsed to `-`). Compare against `norm_path(row.qmd_path)`.
#[derive(Debug, Clone)]
pub struct QmdHit {
    pub path: String,
    pub score: f64,
    pub snippet: String,
    pub docid: String,
    pub title: String,
}

/// The bits of a `grid_rows` row that the hit↔row mapping needs.
#[derive(Debug, Clone)]
pub struct GridRowRef {
    pub uuid: String,
    pub kind: String,
    pub qmd_path: String,
    pub provider: String,
}

/// qmd's path normalization: lowercase + collapse runs of `_`/`-` to `-`.
pub fn norm_path(p: &str) -> String {
    let lower = p.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut prev_dash = false;
    for ch in lower.chars() {
        if ch == '_' || ch == '-' {
            if !prev_dash {
                out.push('-');
                prev_dash = true;
            }
        } else {
            out.push(ch);
            prev_dash = false;
        }
    }
    out
}

/// Pull the `data-section-uuid="…"` value out of one rendered-markdown line,
/// if present. Every message / thinking / tool block opens with such a div,
/// and the value is exactly the `grid_rows.uuid` for that section.
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

/// `qmd:"foo"` / `qmd_vsearch:"foo"` / bare text → (mode, inner).
///
/// Whitespace around the predicate keyword and value is tolerated.
/// Quotes are required for the predicate form (matches the Python
/// implementation). Anything that doesn't match the predicate shape is
/// treated as a bare hybrid query.
pub fn parse_qmd_predicate(raw: &str) -> (QueryMode, String) {
    let trimmed = raw.trim();
    for (prefix, mode) in [
        ("qmd_vsearch", QueryMode::Vsearch),
        ("qmd", QueryMode::Hybrid),
    ] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix(':') {
                let rest = rest.trim_start();
                if let Some(inner) = rest.strip_prefix('"') {
                    if let Some(inner) = inner.strip_suffix('"') {
                        return (mode, inner.to_string());
                    }
                }
            }
        }
    }
    (QueryMode::Hybrid, trimmed.to_string())
}

/// Indexes a set of `GridRowRef`s for fast hit→rows / row→hits lookup. Holds
/// the data root so a hit can be resolved to the exact message it landed in by
/// reading the rendered markdown and mapping the hit's line to a section.
pub struct GridIndex {
    root: PathBuf,
    by_uuid: HashMap<String, GridRowRef>,
    by_norm_path: HashMap<String, Vec<GridRowRef>>,
}

impl GridIndex {
    pub fn new(root: impl Into<PathBuf>, rows: impl IntoIterator<Item = GridRowRef>) -> Self {
        let mut by_uuid: HashMap<String, GridRowRef> = HashMap::new();
        let mut by_norm_path: HashMap<String, Vec<GridRowRef>> = HashMap::new();
        for r in rows {
            by_uuid.insert(r.uuid.clone(), r.clone());
            by_norm_path
                .entry(norm_path(&r.qmd_path))
                .or_default()
                .push(r);
        }
        Self {
            root: root.into(),
            by_uuid,
            by_norm_path,
        }
    }

    /// Resolve a single hit to the grid row for the message it matched.
    ///
    /// The hit's document is found via `by_norm_path` (which carries the real,
    /// un-normalized `qmd_path`); that rendered markdown is then read so the
    /// hit's matched line can be mapped to the enclosing `data-section-uuid`.
    /// Degrades in steps:
    ///   * line pinned to a section that is a grid row → just that one row;
    ///   * line can't be pinned (no diff header, file unreadable, or the
    ///     section isn't a grid row) → every row of the document, so it still
    ///     surfaces — just without the precise message;
    ///   * the hit's file matches no grid rows at all → empty. Every indexed
    ///     doc should have rows, so callers treat this as an error.
    pub fn rows_for_hit(&self, hit: &QmdHit) -> Vec<GridRowRef> {
        let Some(file_rows) = self.by_norm_path.get(&norm_path(&hit.path)) else {
            return Vec::new();
        };
        if let Some(uuid) = self.section_for_line(&file_rows[0].qmd_path, hit) {
            if let Some(row) = self.by_uuid.get(uuid.as_str()) {
                return vec![row.clone()];
            }
        }
        file_rows.clone()
    }

    /// Read `<root>/<qmd_path>` and return the `data-section-uuid` of the
    /// message covering the hit's matched line: the last anchor at or before
    /// that line, or — when the hit lands above the first message (title /
    /// front matter) — the first anchor. `None` if the line can't be derived
    /// or the file can't be read.
    fn section_for_line(&self, qmd_path: &str, hit: &QmdHit) -> Option<String> {
        let line = snippet_match_line(&hit.snippet)?;
        let text = std::fs::read_to_string(self.root.join(qmd_path)).ok()?;
        let mut first: Option<String> = None;
        let mut chosen: Option<String> = None;
        for (i, raw) in text.lines().enumerate() {
            let Some(uuid) = parse_section_uuid(raw) else {
                continue;
            };
            if first.is_none() {
                first = Some(uuid.to_string());
            }
            if i < line {
                chosen = Some(uuid.to_string());
            } else {
                // Anchors appear in file order; once we're past the matched
                // line the enclosing section is settled.
                break;
            }
        }
        chosen.or(first)
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

    fn row(uuid: &str, kind: &str, qmd_path: &str, provider: &str) -> GridRowRef {
        GridRowRef {
            uuid: uuid.to_string(),
            kind: kind.to_string(),
            qmd_path: qmd_path.to_string(),
            provider: provider.to_string(),
        }
    }

    fn hit(path: &str, snippet: &str) -> QmdHit {
        QmdHit {
            path: path.to_string(),
            score: 1.0,
            snippet: snippet.to_string(),
            docid: String::new(),
            title: String::new(),
        }
    }

    #[test]
    fn norm_path_lowercases_and_collapses_dashes_underscores() {
        assert_eq!(norm_path("PR-42__Recalibrate"), "pr-42-recalibrate");
        assert_eq!(norm_path("foo_-_bar"), "foo-bar");
        assert_eq!(norm_path("already-normal"), "already-normal");
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
        // No diff header at all → unknown.
        assert_eq!(snippet_match_line("just a snippet"), None);
    }

    #[test]
    fn parse_qmd_predicate_recognizes_predicates() {
        assert_eq!(
            parse_qmd_predicate("qmd:\"foo bar\""),
            (QueryMode::Hybrid, "foo bar".to_string())
        );
        assert_eq!(
            parse_qmd_predicate("qmd_vsearch:\"x\""),
            (QueryMode::Vsearch, "x".to_string())
        );
        // Whitespace around the colon is tolerated.
        assert_eq!(
            parse_qmd_predicate("qmd : \"hi\""),
            (QueryMode::Hybrid, "hi".to_string())
        );
        // Bare text → hybrid mode, trimmed.
        assert_eq!(
            parse_qmd_predicate("  earl grey  "),
            (QueryMode::Hybrid, "earl grey".to_string())
        );
        // Missing quotes → not a predicate.
        assert_eq!(
            parse_qmd_predicate("qmd:foo"),
            (QueryMode::Hybrid, "qmd:foo".to_string())
        );
    }

    /// Write a two-message rendered chat under `<root>/<rel>` and return the
    /// two grid rows (User Input, LLM Response) keyed to its section anchors.
    /// Anchor lines: `m-AAA…` at line 7, `m-BBB…` at line 13.
    fn write_two_message_doc(root: &std::path::Path, rel: &str) -> Vec<GridRowRef> {
        let body = "---\n\
                    provider: anthropic\n\
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
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        vec![
            row(
                "aaaaaaaa-0000-0000-0000-000000000001",
                "User Input",
                rel,
                "anthropic",
            ),
            row(
                "bbbbbbbb-0000-0000-0000-000000000002",
                "LLM Response",
                rel,
                "anthropic",
            ),
        ]
    }

    #[test]
    fn rows_for_hit_pins_message_by_line() {
        // A hit whose diff header lands in the *second* message resolves to
        // exactly that one row — no snippet anchor required.
        let tmp = tempfile::tempdir().unwrap();
        let rel = "rendered_md/anthropic/acct/org/llm_chats/conv/index.md";
        let idx = GridIndex::new(tmp.path(), write_two_message_doc(tmp.path(), rel));

        // `@@ -13,4 @@ (2 before, …)` → matched line 15, inside the 2nd message.
        let h = hit(
            "rendered-md/anthropic/acct/org/llm-chats/conv/index.md",
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
        let rel = "rendered_md/anthropic/acct/org/llm_chats/conv/index.md";
        let idx = GridIndex::new(tmp.path(), write_two_message_doc(tmp.path(), rel));

        // `@@ -4,4 @@ (1 before, …)` → matched line 5 (the <h1>), before any anchor.
        let h = hit(
            "rendered-md/anthropic/acct/org/llm-chats/conv/index.md",
            "@@ -4,4 @@ (1 before, 10 after)\n<h1>Reactive data pipeline composition in Rust</h1>",
        );
        let got = idx.rows_for_hit(&h);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uuid, "aaaaaaaa-0000-0000-0000-000000000001");
    }

    #[test]
    fn rows_for_hit_unreadable_file_falls_back_to_whole_doc() {
        // File missing on disk → can't pin a section, but the document must
        // still surface: every row whose qmd_path matches comes back.
        let idx = GridIndex::new(
            "/nonexistent-root",
            vec![
                row(
                    "u1",
                    "User Input",
                    "anthropic/acct/llm_chats/conv/index.md",
                    "anthropic",
                ),
                row(
                    "u2",
                    "LLM Response",
                    "anthropic/acct/llm_chats/conv/index.md",
                    "anthropic",
                ),
                row("u3", "Chat", "other/path/index.md", "anthropic"),
            ],
        );
        let h = hit(
            "anthropic/acct/llm-chats/conv/index.md",
            "@@ -9,4 @@ (2 before, 5 after)\nx",
        );
        let got = idx.rows_for_hit(&h);
        let uuids: HashSet<&str> = got.iter().map(|r| r.uuid.as_str()).collect();
        assert_eq!(uuids, ["u1", "u2"].into_iter().collect());
    }

    #[test]
    fn rows_for_hit_unknown_path_is_empty() {
        // A hit whose file matches no grid rows resolves to nothing — the
        // signal callers log as an error.
        let idx = GridIndex::new(
            "/nonexistent-root",
            vec![row("u1", "Chat", "a/b.md", "anthropic")],
        );
        let h = hit("totally/different.md", "@@ -1,4 @@ (0 before, 1 after)\nx");
        assert!(idx.rows_for_hit(&h).is_empty());
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
                "p/index.md",
                "anthropic",
            )],
        );
        let h1 = hit("p/index.md", "@@ -1,4 @@ (0 before, 1 after)\nx");
        let h2 = hit("p/index.md", "@@ -1,4 @@ (0 before, 1 after)\nx");
        let got = idx.rows_for_hits(&[h1, h2]);
        assert_eq!(got.len(), 1);
    }

    /// Replay of `http::run_qmd_search`'s fanout: walk hits in rank order,
    /// stamp each hit's score onto every row it resolves to, first-score-wins.
    /// Returns `(uuid, score)` in the order rows are discovered. Kept in the
    /// test so this regression is self-contained (the real loop is inline in
    /// the http crate).
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
            docid: String::new(),
            title: String::new(),
        }
    }

    /// Regression: a poorly-matched result (a Slack post that merely mentions
    /// "rust") outranks the genuinely-relevant Claude chat in the grid, even
    /// though qmd ranked the chat #1 (score 1.0) and the Slack post #2 (0.5).
    ///
    /// Reproduces the live failure for `q="claude chat about rust dag runner"`
    /// (msg 6d10c99d shown above msg 019e2d00…). Two compounding bugs:
    ///
    ///   1. Title-chunk snippets carry NO `m-{uuid}` anchor, so EVERY hit
    ///      falls through to the path-fallback branch and fans out to the
    ///      whole file with one shared score — per-message ranking is lost.
    ///   2. The same conversation is indexed under two qmd paths (the chat
    ///      renders both at `…/llm_chats/<id>` and nested under another
    ///      conversation at `…/<other>/llm_chats/<id>`). grid_rows.qmd_path
    ///      only matches the nested variant, so qmd's #1 hit (the canonical
    ///      path, score 1.0) resolves to ZERO rows and its score is dropped.
    ///      The chat re-enters only via the duplicate hit at 0.33 — below the
    ///      Slack post's 0.5 — so a score-desc sort floats the spam to the top.
    ///
    /// This test encodes the CURRENT (buggy) behavior so it runs green and
    /// pins the defect; the `WANT` comments mark the intended outcome. When
    /// either bug is fixed, flip the assertions to the `WANT` values.
    #[test]
    fn title_match_dup_path_floats_spam_above_relevant_chat() {
        // qmd_path the grid stored points at the NESTED (duplicate) render.
        let chat_dup_path = "anthropic/acct/ef7dacc5/llm_chats/b0c2f022/index.md";
        let chat_user = "019e2d00-aad9-7ecd-9cfd-b1cd1648b98f"; // the great result
        let chat_llm = "019e2d00-aad9-7933-8639-51c8474c1b11";
        let spam_path = "slack/team/chan/threads/dba0820c/index.md";
        let spam_msg = "6d10c99d-1219-50e0-9d00-84979fd99d1d"; // the terrible result

        // No real files on disk, so every hit resolves via the whole-doc
        // fallback — which is enough to reproduce the dup-path defect (the
        // line-based resolver doesn't change which *document* a hit maps to).
        let idx = GridIndex::new(
            "/nonexistent-root",
            vec![
                row(spam_msg, "Slack Message", spam_path, "slack"),
                row(chat_user, "User Input", chat_dup_path, "anthropic"),
                row(chat_llm, "LLM Response", chat_dup_path, "anthropic"),
            ],
        );

        // qmd's ranking, in order. None of the snippets contain an m-uuid
        // (they're the page-title chunk), so all resolve via path fallback.
        let no_anchor = "@@ -10,4 @@\n<h1 data-page-title-uuid=\"b0c2f022\">Reactive data pipeline composition in Rust</h1>";
        let hits = vec![
            // #1: qmd's best hit — canonical path, NOT the path the grid stored.
            scored_hit("anthropic/acct/llm_chats/b0c2f022/index.md", 1.0, no_anchor),
            // #2: the spam, matched only on a stray "rust cli" vector hit.
            scored_hit(
                spam_path,
                0.5,
                "@@ -23,4 @@\nthe rust cli that sped up your build",
            ),
            // #3: the duplicate render of the same chat — this is the one the
            //     grid path matches.
            scored_hit(chat_dup_path, 0.33, no_anchor),
        ];

        let ranked = fanout(&idx, &hits);

        // qmd's #1 hit (score 1.0) resolved to nothing: its path has no grid
        // rows. WANT: the chat's User Input row should carry 1.0.
        let user_score = ranked.iter().find(|(u, _)| u == chat_user).map(|(_, s)| *s);
        assert_eq!(user_score, Some(0.33)); // WANT: Some(1.0)

        // The frontend sorts by score desc (stable). Emulate it.
        let mut shown = ranked.clone();
        shown.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let order: Vec<&str> = shown.iter().map(|(u, _)| u.as_str()).collect();

        let spam_pos = order.iter().position(|u| *u == spam_msg).unwrap();
        let chat_pos = order.iter().position(|u| *u == chat_user).unwrap();

        // BUG: the spam (0.5) sits above the relevant chat message (0.33).
        assert!(
            spam_pos < chat_pos,
            "spam at {spam_pos}, chat at {chat_pos}: {order:?}"
        );
        // WANT (currently fails): assert!(chat_pos < spam_pos);
    }
}
