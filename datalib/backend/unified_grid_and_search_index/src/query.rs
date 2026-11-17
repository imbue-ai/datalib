//! A table's reading of the shared search-bar grammar (`datalib_query`):
//! which keys are its columns (declared on the table's schema, see
//! `datalib_query::table`), what `is:` and `before:`/`after:` mean, and,
//! for a table qmd indexes, `semantic:`, which says how free text is searched.

use datalib_query::table::{self, Column, FreeText, SearchKey, SearchTable};
use datalib_schema::grid_rows::GridRowColumn;

use crate::db::ANY_VALUE;
use crate::terms_keys::{self, TermsKey, TermsValue, TERMS_KEYS};

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
    let Some(candidate) = value.len().checked_sub(36).and_then(|at| value.get(at..)) else {
        return value;
    };
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
    /// The value was quoted: a key that matches in part matches it whole.
    pub quoted: bool,
    /// For a `contact:<id>` or `me` value, the handles it reaches, read
    /// when the search runs.
    pub handles: Option<Vec<String>>,
}

/// Whether free text is searched by meaning too, and how near a document
/// has to be to count. Without a cutoff every document is some distance
/// away, so every search would find them all. A word or a field that
/// matches counts however far off its meaning is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Semantic {
    /// `semantic:off`.
    Off,
    /// `semantic:0.3`: at least this cosine similarity, in thousandths so
    /// a query stays comparable.
    AtLeast(u16),
}

impl Semantic {
    /// Set for recall: a straggler at the bottom of a list costs little, and
    /// a missed document costs the search. One real library found as much at
    /// 0.30, with far shorter lists, but another may not.
    pub const DEFAULT: Self = Semantic::AtLeast(250);

    /// The similarity a document needs, or `None` when meaning is not asked.
    pub fn cutoff(self) -> Option<f64> {
        match self {
            Semantic::Off => None,
            Semantic::AtLeast(thousandths) => Some(f64::from(thousandths) / 1000.0),
        }
    }

    fn parse(value: &str, negate: bool) -> Result<Self, String> {
        let unread = || "`semantic:` takes `on`, `off` or a similarity from 0 to 1".to_string();
        if negate {
            return Err(unread());
        }
        match value {
            "on" => Ok(Self::DEFAULT),
            "off" => Ok(Semantic::Off),
            number => match number.parse::<f64>() {
                Ok(x) if (0.0..=1.0).contains(&x) => {
                    Ok(Semantic::AtLeast((x * 1000.0).round() as u16))
                }
                _ => Err(unread()),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQuery<C: 'static = GridRowColumn> {
    /// Each occurrence preserved in order. Same-field repetitions are
    /// AND-ed downstream (tree-zoom).
    pub terms: Vec<FilterTerm<C>>,
    /// Free-text portion of the query (after structured filters are peeled
    /// off, `qmd:` and `qmd_vsearch:` being other spellings of free text).
    /// Empty when the user typed only structured filters.
    pub free_text: String,
    pub semantic: Semantic,
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
        // Multiple occurrences: the last one wins.
        let mut semantic = Semantic::DEFAULT;
        for tok in datalib_query::parse(s) {
            match tok {
                datalib_query::Token::Term(t) if qmd && t.key == "semantic" => {
                    match Semantic::parse(&t.value, t.negate) {
                        Ok(asked) => semantic = asked,
                        Err(why) => {
                            refusal.get_or_insert(why);
                        }
                    }
                }
                // `qmd:` and `qmd_vsearch:` are what free text once had to
                // be spelled as, kept for the saved searches that do. A
                // search now has one answer, so both are plain free text.
                datalib_query::Token::Term(t)
                    if qmd && !t.negate && (t.key == "qmd" || t.key == "qmd_vsearch") =>
                {
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
                        quoted: t.quoted,
                        handles: None,
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
            semantic,
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

    /// The contacts the query names (`from:contact:<id>`), each once, in
    /// the order they appear.
    pub fn contact_ids(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for t in &self.terms {
            if let Some(id) = contact_of(t) {
                if !out.iter().any(|seen| seen == id) {
                    out.push(id.to_string());
                }
            }
        }
        out
    }

    /// Each `contact:` term given the handles its contact reaches, read
    /// from the contacts store; a contact `reached` lacks reaches none.
    pub fn with_contact_handles(
        mut self,
        reached: &std::collections::HashMap<String, Vec<String>>,
    ) -> Self {
        for t in &mut self.terms {
            if let Some(id) = contact_of(t).map(str::to_string) {
                t.handles = Some(reached.get(&id).cloned().unwrap_or_default());
            }
        }
        self
    }

    /// Whether a term names the owner (`from:me`).
    pub fn names_me(&self) -> bool {
        self.terms.iter().any(is_me)
    }

    /// Each `me` term given the owner's handles.
    pub fn with_me_handles(mut self, mine: &[String]) -> Self {
        for t in &mut self.terms {
            if is_me(t) {
                t.handles = Some(mine.to_vec());
            }
        }
        self
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

fn is_me<C>(t: &FilterTerm<C>) -> bool {
    match t.field {
        Field::Terms(key) => {
            terms_keys::value_of(key, &t.value, t.quoted, ANY_VALUE) == TermsValue::Me
        }
        _ => false,
    }
}

/// The contact a term names, when it is a person key's `contact:<id>`.
fn contact_of<C>(t: &FilterTerm<C>) -> Option<&str> {
    match t.field {
        Field::Terms(key) => match terms_keys::value_of(key, &t.value, t.quoted, ANY_VALUE) {
            TermsValue::Contact(_) => t.value.strip_prefix(terms_keys::CONTACT),
            _ => None,
        },
        _ => None,
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
        // `since:` is `after:`, in the word a source's window uses.
        "before" | "after" | "since" if ranged => {
            if !value.is_empty() && datalib_time::normalize_user_time_to_utc(value).is_none() {
                return Err(format!(
                    "`{key}:{value}` is not a time the search can read; write a day \
                     (`2026-11-01`, `2026/11/01`, `\"April 3, 2026\"`), a time \
                     (`2026-11-01T09:00`), or an offset from now (`-30d`, `-12h`, `-2w`)"
                ));
            }
            Ok(if key == "before" {
                Field::Before
            } else {
                Field::After
            })
        }
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

    /// A contact named twice is read once, and each of its terms takes
    /// the handles read for it; one the store lacks reaches nothing.
    #[test]
    fn a_contact_value_takes_the_handles_read_for_it() {
        let q = parse_query("from:contact:c-1 -with:contact:c-1 to:contact:c-2 label:contact:x");
        assert_eq!(q.contact_ids(), ["c-1", "c-2"]);
        let reached =
            std::collections::HashMap::from([("c-1".to_string(), vec!["email:a@b.c".to_string()])]);
        let q = q.with_contact_handles(&reached);
        let handles: Vec<Option<Vec<String>>> = q.terms.iter().map(|t| t.handles.clone()).collect();
        assert_eq!(
            handles,
            [
                Some(vec!["email:a@b.c".to_string()]),
                Some(vec!["email:a@b.c".to_string()]),
                Some(vec![]),
                None,
            ],
            "a label is never a contact"
        );
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

    /// `after:2026/11/01`, Gmail's spelling, read as no date and the filter
    /// was dropped: the search answered for every day.
    /// `since:` is `after:`, and a bound may be an offset from now or a
    /// day in words, quoted.
    #[test]
    fn since_offsets_and_days_in_words_are_bounds() {
        for typed in [
            "since:-30d",
            "after:-2w",
            "before:-12h",
            r#"before:"Tuesday, April 3, 2029""#,
            r#"since:"Tue Apr 3rd, 2029 at 08:30""#,
            r#"after:"3 April 2029""#,
        ] {
            let q = parse_query(&format!("{typed} hello"));
            assert_eq!(q.refusal(), None, "{typed}");
            assert_eq!(q.free_text, "hello", "{typed}");
        }
        let q = parse_query(r#"since:"April 3, 2029""#);
        assert_eq!(q.bound(Field::After), Some("April 3, 2029"));
        assert!(
            parse_query(r#"before:"Monday, April 3, 2029""#)
                .refusal()
                .is_some(),
            "April 3, 2029 is a Tuesday"
        );
    }

    #[test]
    fn a_bound_is_a_gmail_day_or_refused() {
        let q = parse_query("after:2026/11/01 hello");
        assert_eq!(q.refusal(), None);
        assert_eq!(q.bound(Field::After), Some("2026/11/01"));
        let why = parse_query("before:yesterday")
            .refusal()
            .expect("an unreadable bound is refused, not dropped");
        assert!(why.contains("`before:yesterday`"), "{why}");
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
    fn source_and_kind_keys_recognized() {
        let q = parse_query("source:slack kind:Chat");
        assert_eq!(kept(&q, col("source_id")), ["slack"]);
        assert_eq!(kept(&q, col("kind")), ["Chat"]);
    }

    /// `source_id:` was the key's name before `source:`; a saved query or
    /// a link that still says it finds the same rows.
    #[test]
    fn source_id_is_still_the_source_key() {
        let q = parse_query("source_id:slack");
        assert_eq!(q.refusal(), None);
        assert_eq!(kept(&q, col("source_id")), ["slack"]);
    }

    /// `source_name:` (an older spelling of `source:`) is gone. A saved
    /// query that still names it is refused by name, not searched as
    /// free text.
    #[test]
    fn the_retired_source_key_is_refused() {
        let parsed = parse_query("source_name:slack");
        assert!(parsed.refusal().is_some(), "{parsed:?}");
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

    /// A word whose 36th byte from the end falls inside a character
    /// panicked on the byte slice; every free-text search reaches this.
    #[test]
    fn extract_uuid_suffix_leaves_a_word_split_mid_character_alone() {
        let word = format!("é{}", "a".repeat(35));
        assert_eq!(extract_uuid_suffix(&word), word);
    }

    #[test]
    fn bare_text_defaults_to_hybrid_mode() {
        let q = parse_query("earl grey");
        assert_eq!(q.free_text, "earl grey");
    }

    #[test]
    fn qmd_predicate_recognized_as_hybrid() {
        let q = parse_query("qmd:\"earl grey\"");
        // Multi-word qmd: value is re-quoted so the daemon can route
        // it as a single lex phrase.
        assert_eq!(q.free_text, "\"earl grey\"");
        // qmd: is NOT a Field — it doesn't show up as a structured filter.
        assert!(q.terms.is_empty());
    }

    #[test]
    fn qmd_vsearch_predicate_is_plain_free_text() {
        let q = parse_query("qmd_vsearch:\"hello world\"");
        assert_eq!(q.free_text, "\"hello world\"");
    }

    #[test]
    fn qmd_predicate_single_word_unquoted() {
        // Single-word qmd: value doesn't need re-quoting.
        let q = parse_query("qmd:\"foo\" source:slack");
        assert_eq!(q.free_text, "foo");
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

    /// The search bar's one say over meaning: on by default at a cutoff, off,
    /// or at a cutoff of the person's. Anything else is refused, so a typo
    /// cannot quietly turn it on.
    #[test]
    fn semantic_is_on_by_default_and_refuses_what_it_cannot_read() {
        let semantic = |q: &str| parse_query(q).semantic;
        assert_eq!(semantic("warp core"), Semantic::DEFAULT);
        assert_eq!(semantic("warp core semantic:on"), Semantic::DEFAULT);
        assert_eq!(semantic("warp core semantic:off"), Semantic::Off);
        assert_eq!(semantic("semantic:0.4 warp"), Semantic::AtLeast(400));
        assert_eq!(semantic("semantic:0"), Semantic::AtLeast(0));
        assert_eq!(semantic("semantic:1"), Semantic::AtLeast(1000));
        assert_eq!(
            semantic("semantic:off semantic:0.3"),
            Semantic::AtLeast(300)
        );
        assert_eq!(Semantic::AtLeast(300).cutoff(), Some(0.3));
        assert_eq!(Semantic::Off.cutoff(), None);

        let q = parse_query("warp semantic:0.3");
        assert_eq!(q.free_text, "warp", "it is no part of the words");
        assert!(q.terms.is_empty(), "and no filter on a column");

        for q in [
            "semantic:maybe",
            "semantic:1.5",
            "semantic:-1",
            "-semantic:off",
        ] {
            assert!(
                parse_query(q)
                    .refusal()
                    .is_some_and(|why| why.contains("semantic")),
                "{q}"
            );
        }
    }
}
