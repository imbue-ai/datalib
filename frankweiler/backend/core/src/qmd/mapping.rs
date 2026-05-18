//! Map qmd search hits to grid rows and back.
//!
//! Hit→rows resolves by embedded `m-{uuid}` ids in the snippet first,
//! then falls back to every row whose normalized `qmd_path` matches
//! the hit's path. qmd lowercases paths and collapses runs of `_`/`-`
//! to a single `-` in its internal docid URI, so the same normalization
//! applies on the grid side.
//!
//! `parse_query` recognizes `qmd:"…"` and `qmd_vsearch:"…"` as predicates
//! over the search-bar string; anything else is treated as bare hybrid
//! query text. The broader search-bar parser in `crate::query` calls
//! this after handling structured `field:value` filters.

use std::collections::{HashMap, HashSet};

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

/// Extract every `m-{uuid}` token from a qmd snippet.
pub fn extract_m_uuids(snippet: &str) -> Vec<String> {
    // Walk byte-by-byte looking for `m-` followed by a UUID-shaped token.
    // A UUID-shape is 8-4-4-4-12 lowercase hex with `-` separators (36 chars).
    let bytes = snippet.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 2 + 36 <= bytes.len() {
        if bytes[i] == b'm' && bytes[i + 1] == b'-' {
            let start = i + 2;
            if is_uuid_shape(&bytes[start..start + 36]) {
                // Reject if preceded by another hex char so we don't
                // grab the tail of a longer identifier.
                let preceded_by_hex =
                    i > 0 && (bytes[i - 1] as char).is_ascii_alphanumeric() && bytes[i - 1] != b' ';
                if !preceded_by_hex {
                    out.push(
                        std::str::from_utf8(&bytes[start..start + 36])
                            .unwrap()
                            .to_string(),
                    );
                    i = start + 36;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

fn is_uuid_shape(b: &[u8]) -> bool {
    if b.len() != 36 {
        return false;
    }
    for (i, &c) in b.iter().enumerate() {
        let dash_pos = matches!(i, 8 | 13 | 18 | 23);
        if dash_pos {
            if c != b'-' {
                return false;
            }
        } else if !(c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
            return false;
        }
    }
    true
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

/// Indexes a set of `GridRowRef`s for fast hit→rows / row→hits lookup.
pub struct GridIndex {
    by_uuid: HashMap<String, GridRowRef>,
    by_norm_path: HashMap<String, Vec<GridRowRef>>,
}

impl GridIndex {
    pub fn new(rows: impl IntoIterator<Item = GridRowRef>) -> Self {
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
            by_uuid,
            by_norm_path,
        }
    }

    /// Resolve a single hit to grid rows using strict semantics.
    ///
    /// Returns rows in the order they appear in the snippet (uuid-match
    /// case) or in arbitrary stable order (path-fallback case). Dedupes.
    pub fn rows_for_hit(&self, hit: &QmdHit) -> Vec<GridRowRef> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out: Vec<GridRowRef> = Vec::new();
        for u in extract_m_uuids(&hit.snippet) {
            if let Some(row) = self.by_uuid.get(&u) {
                if seen.insert(row.uuid.clone()) {
                    out.push(row.clone());
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
        if let Some(rows) = self.by_norm_path.get(&norm_path(&hit.path)) {
            return rows.clone();
        }
        Vec::new()
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

    /// Reverse: which of `hits` mention `row`? A hit mentions the row when
    /// the hit's path matches `row.qmd_path` (after normalization) AND
    /// either the snippet has no parseable `m-{uuid}` ids (file-level
    /// fallback) or the row's uuid is among them.
    pub fn hits_for_row<'a>(&self, row: &GridRowRef, hits: &'a [QmdHit]) -> Vec<&'a QmdHit> {
        let target = norm_path(&row.qmd_path);
        let mut out: Vec<&QmdHit> = Vec::new();
        for h in hits {
            if norm_path(&h.path) != target {
                continue;
            }
            let uuids: HashSet<String> = extract_m_uuids(&h.snippet).into_iter().collect();
            if uuids.is_empty() || uuids.contains(&row.uuid) {
                out.push(h);
            }
        }
        out
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
    fn extract_m_uuids_finds_embedded_ids() {
        let snip = "<div id=\"m-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\">x</div> \
                    and <div id=\"m-11111111-2222-3333-4444-555555555555\">y</div>";
        let got = extract_m_uuids(snip);
        assert_eq!(
            got,
            vec![
                "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
                "11111111-2222-3333-4444-555555555555".to_string(),
            ]
        );
    }

    #[test]
    fn extract_m_uuids_ignores_non_uuid_shapes() {
        assert!(extract_m_uuids("m-not-a-uuid").is_empty());
        assert!(extract_m_uuids("xm-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").is_empty());
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

    #[test]
    fn rows_for_hit_prefers_embedded_uuids() {
        let idx = GridIndex::new(vec![
            row(
                "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                "User Input",
                "anthropic/acct/llm_chats/conv.qmd",
                "anthropic",
            ),
            row(
                "11111111-2222-3333-4444-555555555555",
                "LLM Response",
                "anthropic/acct/llm_chats/conv.qmd",
                "anthropic",
            ),
        ]);
        let h = hit(
            "anthropic/acct/llm-chats/conv.qmd",
            "<div id=\"m-11111111-2222-3333-4444-555555555555\">…</div>",
        );
        let got = idx.rows_for_hit(&h);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uuid, "11111111-2222-3333-4444-555555555555");
    }

    #[test]
    fn rows_for_hit_falls_back_to_path() {
        // No m-uuid in the snippet — should resolve every row whose
        // normalized qmd_path matches the hit's normalized path.
        let idx = GridIndex::new(vec![
            row(
                "u1",
                "Chat",
                "anthropic/acct/llm_chats/conv.qmd",
                "anthropic",
            ),
            row(
                "u2",
                "User Input",
                "anthropic/acct/llm_chats/conv.qmd",
                "anthropic",
            ),
            row("u3", "Chat", "other/path.qmd", "anthropic"),
        ]);
        // Hit path uses qmd's normalized form (hyphen in `llm-chats`).
        let h = hit("anthropic/acct/llm-chats/conv.qmd", "no anchors here");
        let got = idx.rows_for_hit(&h);
        let uuids: HashSet<&str> = got.iter().map(|r| r.uuid.as_str()).collect();
        assert_eq!(uuids, ["u1", "u2"].into_iter().collect());
    }

    #[test]
    fn rows_for_hits_dedupes_across_hits() {
        let idx = GridIndex::new(vec![row(
            "11111111-2222-3333-4444-555555555555",
            "LLM Response",
            "p.qmd",
            "anthropic",
        )]);
        let h1 = hit(
            "p.qmd",
            "<div id=\"m-11111111-2222-3333-4444-555555555555\"/>",
        );
        let h2 = hit(
            "p.qmd",
            "<div id=\"m-11111111-2222-3333-4444-555555555555\"/>",
        );
        let got = idx.rows_for_hits(&[h1, h2]);
        assert_eq!(got.len(), 1);
    }
}
