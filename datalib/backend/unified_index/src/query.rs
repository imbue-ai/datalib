//! A table's reading of the shared search-bar grammar (`datalib_query`):
//! which keys are its columns (declared on the table's schema, see
//! `datalib_query::table`), what `is:` and `before:`/`after:` mean, and,
//! for a table qmd indexes, the `qmd:` / `qmd_vsearch:` predicates that
//! route free text to it.

use datalib_query::table::{self, Column, FreeText, SearchKey, SearchTable};
use datalib_schema::grid_rows::GridRowColumn;

use crate::terms_keys::{TermsKey, TERMS_KEYS};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field<C: 'static> {
    Before,
    After,
    /// `is:<word>`: the rows where this column is true, or with `-`,
    /// false. The grid's is `is:document`.
    Is(C),
    /// A key that compares one column.
    Column(&'static SearchKey<C>),
    /// A key that reads the search terms (`crate::terms_keys`).
    Terms(&'static TermsKey),
}

/// Notion-style slug+UUID parser. If `value` ends with a `-`-prefixed
/// UUID-shaped suffix (8-4-4-4-12 lowercase hex), return that suffix; else
/// return the input unchanged. The leading slug is non-load-bearing — it
/// exists only to make URLs/tokens self-describing.
pub fn extract_uuid_suffix(value: &str) -> &str {
    if value.len() < 36 {
        return value;
    }
    let candidate = &value[value.len() - 36..];
    if is_uuid_shape(candidate) && (value.len() == 36 || value.as_bytes()[value.len() - 37] == b'-')
    {
        candidate
    } else {
        value
    }
}

pub fn is_uuid_shape(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    let b = s.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        let is_dash_pos = matches!(i, 8 | 13 | 18 | 23);
        if is_dash_pos {
            if c != b'-' {
                return false;
            }
        } else if !c.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

/// One filter occurrence from the query string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterTerm<C: 'static> {
    pub field: Field<C>,
    pub value: String,
    pub negate: bool,
}

/// How free-text should be evaluated. Bare search-bar text defaults to
/// `Hybrid`; the explicit `qmd:"..."` predicate forces `Hybrid` and
/// `qmd_vsearch:"..."` forces `Vsearch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreeTextMode {
    /// qmd hybrid (BM25 + vector + reranker).
    Hybrid,
    /// qmd vector-only search.
    Vsearch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQuery<C: 'static = GridRowColumn> {
    /// Each occurrence preserved in order. Same-field repetitions are
    /// AND-ed downstream (tree-zoom).
    pub terms: Vec<FilterTerm<C>>,
    /// Free-text portion of the query (after structured filters and the
    /// optional `qmd:` / `qmd_vsearch:` predicate are peeled off). Empty
    /// when the user typed only structured filters.
    pub free_text: String,
    /// How `free_text` should be evaluated against the qmd index.
    pub free_text_mode: FreeTextMode,
    refusal: Option<String>,
}

impl<C: Column> ParsedQuery<C> {
    pub fn parse(s: &str) -> Self {
        Self::parse_with(s, &[])
    }

    /// `parse`, reading `terms` as keys too: the grid's search, whose
    /// reader has the search terms attached.
    pub fn parse_with(s: &str, terms_keys: &'static [TermsKey]) -> Self {
        let qmd = matches!(<C::Table as SearchTable>::FREE_TEXT, FreeText::Qmd);
        let mut terms: Vec<FilterTerm<C>> = Vec::new();
        let mut free_terms: Vec<String> = Vec::new();
        let mut refusal: Option<String> = None;
        // `qmd:"..."` / `qmd_vsearch:"..."` set the mode for the free-text
        // portion. Multiple occurrences: last one wins.
        let mut free_text_mode: FreeTextMode = FreeTextMode::Hybrid;
        for tok in datalib_query::parse(s) {
            match tok {
                // `qmd:` and `qmd_vsearch:` are not column filters — they
                // route the embedded free text through qmd. Treat the value
                // as free text and switch modes.
                datalib_query::Token::Term(t)
                    if qmd && !t.negate && (t.key == "qmd" || t.key == "qmd_vsearch") =>
                {
                    free_text_mode = if t.key == "qmd_vsearch" {
                        FreeTextMode::Vsearch
                    } else {
                        FreeTextMode::Hybrid
                    };
                    // Re-quote multi-word values so the daemon can route
                    // them as a single lex phrase. `qmd:"earl grey"` and
                    // `qmd:earl` both work; quotes survive into free_text
                    // only when the user actually typed a phrase.
                    let term = if t.value.contains(char::is_whitespace) {
                        format!("\"{}\"", t.value)
                    } else {
                        t.value
                    };
                    free_terms.push(term);
                }
                datalib_query::Token::Term(t) => match field::<C>(&t.key, &t.value, terms_keys) {
                    Ok(field) => terms.push(FilterTerm {
                        field,
                        value: t.value,
                        negate: t.negate,
                    }),
                    Err(why) => {
                        refusal.get_or_insert(why);
                    }
                },
                // Bare term: surrounding quotes and a leading `-` stay
                // verbatim so the daemon can forward lex-meaningful syntax to
                // qmd. `"earl grey"` → qmd exact-phrase match; `-foo` → qmd
                // term exclusion; `-"earl grey"` → qmd phrase exclusion. See
                // `qmd::lex`. Plain words pass through too.
                datalib_query::Token::Free(raw) => free_terms.push(raw),
            }
        }
        ParsedQuery {
            terms,
            free_text: free_terms.join(" "),
            free_text_mode,
            refusal,
        }
    }

    /// Why the search cannot read this query, worded for the search bar:
    /// the first key it does not have, or `is:` it does not know. A term
    /// dropped instead would quietly match everything the rest matches.
    pub fn refusal(&self) -> Option<String> {
        self.refusal.clone()
    }

    /// `is:<word>` on this column: true, false for `-is:<word>`, `None`
    /// for neither. The last one wins.
    pub fn flag(&self, column: C) -> Option<bool> {
        self.terms
            .iter()
            .filter(|t| t.field == Field::Is(column))
            .fold(None, |_, t| Some(!t.negate))
    }

    /// The first positive `before:` or `after:`, as typed.
    pub fn bound(&self, field: Field<C>) -> Option<&str> {
        self.terms
            .iter()
            .find(|t| t.field == field && !t.negate)
            .map(|t| t.value.as_str())
    }
}

impl ParsedQuery<GridRowColumn> {
    /// `is:document` keeps only the document rows, `-is:document` only
    /// the rows inside documents.
    pub fn documents(&self) -> Option<bool> {
        self.flag(GridRowColumn::IsDocument)
    }
}

fn field<C: Column>(
    key: &str,
    value: &str,
    terms: &'static [TermsKey],
) -> Result<Field<C>, String> {
    if let Some(k) = terms
        .iter()
        .find(|k| k.key == key || k.aliases.contains(&key))
    {
        return Ok(Field::Terms(k));
    }
    let t = || <C::Table as SearchTable>::FLAGS;
    let ranged = <C::Table as SearchTable>::RANGE.is_some();
    match key {
        "before" if ranged => Ok(Field::Before),
        "after" if ranged => Ok(Field::After),
        "is" if !t().is_empty() => t()
            .iter()
            .find(|(word, _)| *word == value)
            .map(|(_, c)| Field::Is(*c))
            .ok_or_else(|| {
                let words: Vec<String> = t().iter().map(|(w, _)| format!("`is:{w}`")).collect();
                format!(
                    "`is:{value}` is not something the search knows; try {}",
                    words.join(" or ")
                )
            }),
        _ => table::key::<C::Table>(key)
            .map(|k| within_vocabulary(key, k, value).map(|()| Field::Column(k)))
            .unwrap_or_else(|| {
                let mut known: Vec<&str> = <C::Table as SearchTable>::KEYS
                    .iter()
                    .map(|k| k.key)
                    .collect();
                if ranged {
                    known.extend(["before", "after"]);
                }
                if !t().is_empty() {
                    known.push("is");
                }
                known.extend(terms.iter().map(|k| k.key));
                Err(format!(
                    "`{key}:` is not something the search can filter on; try one of {}",
                    known.join(", ")
                ))
            }),
    }
}

/// A closed key takes one of its words, or `*`.
fn within_vocabulary<C>(typed: &str, key: &SearchKey<C>, value: &str) -> Result<(), String> {
    let Some(words) = key.vocabulary.map(|words| words()) else {
        return Ok(());
    };
    if value == crate::db::ANY_VALUE || words.contains(&value) {
        return Ok(());
    }
    Err(format!(
        "`{typed}:` takes one of {}, not `{value}`",
        words.join(", ")
    ))
}

/// The grid's query: `grid_rows`, its keys, the search terms' keys, and
/// qmd for free text.
pub fn parse_query(s: &str) -> ParsedQuery {
    ParsedQuery::parse_with(s, TERMS_KEYS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(key: &str) -> Field<GridRowColumn> {
        Field::Column(table::key::<datalib_schema::grid_rows::GridRow>(key).unwrap())
    }

    /// The values the query keeps for a field: its positive terms.
    fn kept(q: &ParsedQuery, field: Field<GridRowColumn>) -> Vec<&str> {
        q.terms
            .iter()
            .filter(|t| t.field == field && !t.negate)
            .map(|t| t.value.as_str())
            .collect()
    }

    #[test]
    fn empty_query_keeps_every_row() {
        let q = parse_query("");
        assert_eq!(q.documents(), None);
        assert_eq!(q.free_text, "");
        assert!(q.terms.is_empty());
    }

    #[test]
    fn free_text_alone_keeps_every_row() {
        let q = parse_query("treemap layout");
        assert_eq!(q.documents(), None);
        assert_eq!(q.free_text, "treemap layout");
    }

    #[test]
    fn is_document_sets_the_documents_filter() {
        assert_eq!(parse_query("treemap is:document").documents(), Some(true));
        assert_eq!(parse_query("-is:document").documents(), Some(false));
        assert_eq!(parse_query("is:unread").documents(), None);
        assert!(parse_query("is:unread")
            .refusal()
            .unwrap()
            .contains("is:unread"));
        // Last one wins, and the free text is untouched.
        let q = parse_query("treemap is:document -is:document");
        assert_eq!(q.documents(), Some(false));
        assert_eq!(q.free_text, "treemap");
    }

    #[test]
    fn structured_filters_collected() {
        let q = parse_query("before:2025-01-01 channel:bridge hello");
        assert_eq!(q.bound(Field::Before), Some("2025-01-01"));
        assert_eq!(kept(&q, col("channel")), ["bridge"]);
        assert_eq!(q.free_text, "hello");
    }

    #[test]
    fn an_unknown_key_is_refused_by_name() {
        let q = parse_query("custom:foo");
        assert!(q.terms.is_empty());
        let why = q.refusal().expect("an unknown key is refused");
        assert!(why.contains("`custom:`"), "{why}");
        assert!(
            why.contains("channel"),
            "it names the keys there are: {why}"
        );
        assert!(why.contains("from"), "the search terms' keys too: {why}");
        assert!(parse_query("-custom:foo").refusal().is_some());
        assert_eq!(parse_query("author:picard is:document").refusal(), None);
        assert!(
            ParsedQuery::<GridRowColumn>::parse("from:picard")
                .refusal()
                .is_some(),
            "a table read without the search terms has no `from:`"
        );
    }

    #[test]
    fn quoted_free_text_preserves_quotes() {
        // Quotes survive into free_text so the daemon can send qmd a
        // lex sub-query for exact-phrase matching.
        let q = parse_query("\"hello world\" channel:bridge");
        assert_eq!(q.free_text, "\"hello world\"");
        assert_eq!(kept(&q, col("channel")), ["bridge"]);
    }

    #[test]
    fn negated_quoted_free_text_preserves_quotes_and_dash() {
        let q = parse_query("-\"earl grey\"");
        assert_eq!(q.free_text, "-\"earl grey\"");
    }

    #[test]
    fn phrase_plus_word_concatenated_in_order() {
        let q = parse_query("\"earl grey\" tea");
        assert_eq!(q.free_text, "\"earl grey\" tea");
    }

    #[test]
    fn duplicate_filters_accumulate() {
        let q = parse_query("channel:a channel:b");
        assert_eq!(kept(&q, col("channel")), ["a", "b"]);
        assert_eq!(q.terms.len(), 2);
        assert!(q.terms.iter().all(|t| !t.negate));
    }

    #[test]
    fn negation_recorded_in_terms_only() {
        let q = parse_query("-channel:announce");
        assert_eq!(q.terms.len(), 1);
        assert!(q.terms[0].negate);
        assert_eq!(q.terms[0].field, col("channel"));
        assert_eq!(q.terms[0].value, "announce");
        assert!(kept(&q, col("channel")).is_empty());
    }

    #[test]
    fn quoted_value_with_special_chars() {
        let q = parse_query("channel:\"#dev:ops\"");
        let chan = &q.terms[0];
        assert_eq!(chan.field, col("channel"));
        assert_eq!(chan.value, "#dev:ops");
        assert!(!chan.negate);
    }

    #[test]
    fn quoted_value_with_escapes() {
        let q = parse_query(r#"convo:"a\"b\\c""#);
        assert_eq!(q.terms[0].value, "a\"b\\c");
    }

    #[test]
    fn negated_quoted_value() {
        let q = parse_query(r#"-convo:"hello world""#);
        assert!(q.terms[0].negate);
        assert_eq!(q.terms[0].field, col("convo"));
        assert_eq!(q.terms[0].value, "hello world");
    }

    #[test]
    fn source_id_and_kind_keys_recognized() {
        let q = parse_query("source_id:slack kind:Chat");
        assert_eq!(kept(&q, col("source_id")), ["slack"]);
        assert_eq!(kept(&q, col("kind")), ["Chat"]);
    }

    /// `source:` (the provider's label) and `source_name:` (the old
    /// spelling of `source_id:`) are gone. A saved query that still names
    /// one is refused by name, not searched as free text.
    #[test]
    fn the_retired_source_keys_are_refused() {
        for q in ["source:Slack", "source_name:slack"] {
            let parsed = parse_query(q);
            assert!(parsed.refusal().is_some(), "{q}: {parsed:?}");
        }
    }

    #[test]
    fn extract_uuid_suffix_strips_slug() {
        // Notion-style: slug-uuid.
        assert_eq!(
            extract_uuid_suffix("picard-jean-luc-00000001-1701-4d00-8000-000000000001"),
            "00000001-1701-4d00-8000-000000000001"
        );
        // Bare UUID passes through.
        assert_eq!(
            extract_uuid_suffix("00000001-1701-4d00-8000-000000000001"),
            "00000001-1701-4d00-8000-000000000001"
        );
        // No UUID-shaped suffix → unchanged.
        assert_eq!(extract_uuid_suffix("plain-name"), "plain-name");
        // Too-short string → unchanged.
        assert_eq!(extract_uuid_suffix("short"), "short");
        // Suffix is hex-shaped but missing the boundary dash before slug.
        assert_eq!(
            extract_uuid_suffix("xxx00000001-1701-4d00-8000-000000000001"),
            "xxx00000001-1701-4d00-8000-000000000001"
        );
    }

    #[test]
    fn bare_text_defaults_to_hybrid_mode() {
        let q = parse_query("earl grey");
        assert_eq!(q.free_text, "earl grey");
        assert_eq!(q.free_text_mode, FreeTextMode::Hybrid);
    }

    #[test]
    fn qmd_predicate_recognized_as_hybrid() {
        let q = parse_query("qmd:\"earl grey\"");
        // Multi-word qmd: value is re-quoted so the daemon can route
        // it as a single lex phrase.
        assert_eq!(q.free_text, "\"earl grey\"");
        assert_eq!(q.free_text_mode, FreeTextMode::Hybrid);
        // qmd: is NOT a Field — it doesn't show up as a structured filter.
        assert!(q.terms.is_empty());
    }

    #[test]
    fn qmd_vsearch_predicate_switches_mode() {
        let q = parse_query("qmd_vsearch:\"hello world\"");
        assert_eq!(q.free_text, "\"hello world\"");
        assert_eq!(q.free_text_mode, FreeTextMode::Vsearch);
    }

    #[test]
    fn qmd_predicate_single_word_unquoted() {
        // Single-word qmd: value doesn't need re-quoting.
        let q = parse_query("qmd:\"foo\" source_id:slack");
        assert_eq!(q.free_text, "foo");
        assert_eq!(q.free_text_mode, FreeTextMode::Hybrid);
        assert_eq!(kept(&q, col("source_id")), ["slack"]);
    }

    #[test]
    fn lone_dash_term_stays_free_text() {
        // `-foo` (no colon) is just a free-text token (we don't yet
        // implement free-text negation); the leading `-` is preserved so
        // round-tripping isn't lossy.
        let q = parse_query("-foo bar");
        assert_eq!(q.free_text, "-foo bar");
        assert!(q.terms.is_empty());
    }
}
