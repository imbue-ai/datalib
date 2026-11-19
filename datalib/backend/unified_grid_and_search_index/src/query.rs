//! A table's reading of the shared search-bar grammar (`datalib_query`):
//! which keys are its columns (declared on the table's schema, see
//! `datalib_query::table`), what `is:` and `before:`/`after:` mean, and,
//! for a table search indexes, `semantic:`, which says how free text is searched.

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
    /// The row's text holds these words in this order: a quoted phrase or a
    /// `+word`, or with `-`, does not. A message's text is its own; a
    /// document's own row holds its whole document.
    Text,
    /// The row names this id or handle in any of its terms: an id, a link
    /// ending in one, or an address, typed as free text. The value is as
    /// typed; [`identifier`] reads it.
    Names,
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
    /// The words to rank by, space-separated, with no quotes: every bare,
    /// quoted and `+` word (`qmd:` and `qmd_vsearch:` being other
    /// spellings). Empty when nothing is to be ranked: the filters are
    /// then listed in order.
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
        let ranked = matches!(<C::Table as SearchTable>::FREE_TEXT, FreeText::Ranked);
        let mut terms: Vec<FilterTerm<C>> = Vec::new();
        let mut free_terms: Vec<String> = Vec::new();
        let mut refusal: Option<String> = None;
        // Multiple occurrences: the last one wins.
        let mut semantic = Semantic::DEFAULT;
        for tok in datalib_query::parse(s) {
            match tok {
                datalib_query::Token::Term(t) if ranked && t.key == "semantic" => {
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
                    if ranked && !t.negate && (t.key == "qmd" || t.key == "qmd_vsearch") =>
                {
                    // `qmd:"earl grey"` was a phrase.
                    let typed = if t.value.contains(char::is_whitespace) {
                        datalib_query::quote(&t.value)
                    } else {
                        t.value
                    };
                    free_token(&typed, &mut terms, &mut free_terms);
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
                datalib_query::Token::Free(raw) if ranked => {
                    free_token(&raw, &mut terms, &mut free_terms)
                }
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

/// One free token of a ranked table's query. An id or an address is a
/// filter on the row's terms; a quoted, `+` or `-` one a filter on its
/// text; every word but an excluded one is ranked by.
fn free_token<C>(raw: &str, terms: &mut Vec<FilterTerm<C>>, words: &mut Vec<String>) {
    let (text, negate) = datalib_query::free_text(raw);
    let body = raw.strip_prefix('-').filter(|_| negate).unwrap_or(raw);
    let quoted = body.len() >= 2 && body.starts_with('"') && body.ends_with('"');
    let filter = |field, value: String| FilterTerm {
        field,
        value,
        negate,
        quoted,
        handles: None,
    };
    if !quoted && identifier(&text).is_some() {
        terms.push(filter(Field::Names, text));
        return;
    }
    let (required, text) = match text.strip_prefix('+') {
        Some(rest) if !quoted && !negate && !rest.is_empty() => (true, rest.to_string()),
        _ => (false, text),
    };
    // Punctuation alone (`warp - core`) is no word: the index holds none.
    if !text.chars().any(char::is_alphanumeric) {
        return;
    }
    if quoted || required || negate {
        terms.push(filter(Field::Text, text.clone()));
    }
    if !negate {
        words.push(text);
    }
}

/// `word` as the id or handle it names: a uuid, dashed or not, alone or at
/// the end of a slug or a link, canonical; or a handle (`terms_keys::handle_of`).
pub fn identifier(word: &str) -> Option<String> {
    datalib_uuid_text::at_end(word).or_else(|| terms_keys::handle_of(word))
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
/// the search index for free text.
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
        let q = parse_query(r#"datalib_document_id:"a\"b\\c""#);
        assert_eq!(q.terms[0].value, "a\"b\\c");
    }

    #[test]
    fn negated_quoted_value() {
        let q = parse_query(r#"-datalib_document_id:"hello world""#);
        assert!(q.terms[0].negate);
        assert_eq!(q.terms[0].field, col("datalib_document_id"));
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

    /// A word whose 36th byte from the end falls inside a character
    /// panicked on the byte slice; every free-text search reaches this.
    /// The words to rank by are every word but an excluded one, without its
    /// quotes; a quoted, `+` or `-` one is a filter on the row's text too.
    #[test]
    fn free_text_is_words_to_rank_and_filters_on_the_text() {
        let q = parse_query(r#""warp core" +deck -breach -"red alert" twelve channel:bridge"#);
        assert_eq!(q.free_text, "warp core deck twelve");
        let text: Vec<(&str, bool)> = q
            .terms
            .iter()
            .filter(|t| t.field == Field::Text)
            .map(|t| (t.value.as_str(), t.negate))
            .collect();
        assert_eq!(
            text,
            [
                ("warp core", false),
                ("deck", false),
                ("breach", true),
                ("red alert", true)
            ]
        );
        assert_eq!(kept(&q, col("channel")), ["bridge"]);
        assert_eq!(parse_query("-breach").free_text, "", "nothing to rank");
    }

    /// Punctuation alone is no word; a lone `+` or `-` is punctuation.
    #[test]
    fn punctuation_alone_is_skipped() {
        let q = parse_query("warp - core + \"--\"");
        assert_eq!(q.free_text, "warp core");
        assert!(q.terms.is_empty(), "{:?}", q.terms);
    }

    /// `qmd:` and `qmd_vsearch:` are free text: a value of several words a
    /// phrase.
    #[test]
    fn qmd_values_are_free_text() {
        let q = parse_query("qmd:\"earl grey\" source:slack");
        assert_eq!(q.free_text, "earl grey");
        assert_eq!(q.terms[0].field, Field::Text);
        assert_eq!(parse_query("qmd_vsearch:tea").free_text, "tea");
    }

    /// An id or an address typed as free text is a filter on what a row
    /// names, as typed, and nothing to rank.
    #[test]
    fn an_id_or_an_address_is_a_filter() {
        let q = parse_query("Ann@Example.com -1701d000abcd4e00a0000000000000ef budget");
        assert_eq!(q.free_text, "budget");
        let names: Vec<(&str, bool)> = q
            .terms
            .iter()
            .filter(|t| t.field == Field::Names)
            .map(|t| (t.value.as_str(), t.negate))
            .collect();
        assert_eq!(
            names,
            [
                ("Ann@Example.com", false),
                ("1701d000abcd4e00a0000000000000ef", true)
            ]
        );
        assert_eq!(
            parse_query("\"ann@example.com\"").terms[0].field,
            Field::Text,
            "quoted, it is words"
        );
    }

    #[test]
    fn identifiers_are_uuids_and_handles_and_nothing_else() {
        let id = |w: &str| identifier(w);
        assert_eq!(
            id("00000000-0000-8BaD-8eeF-0000bead0002").as_deref(),
            Some("00000000-0000-8bad-8eef-0000bead0002")
        );
        assert_eq!(
            id("away-team-00000000-0000-4000-8000-000000000001").as_deref(),
            Some("00000000-0000-4000-8000-000000000001")
        );
        assert_eq!(
            id("Ann@Example.com").as_deref(),
            Some("email:ann@example.com")
        );
        assert_eq!(id("slack:T1/U2").as_deref(), Some("slack:T1/U2"));
        assert_eq!(id("budget"), None);
        assert_eq!(
            id("xxx00000000-0000-4000-8000-000000000001"),
            None,
            "a uuid run into the word before it is not one"
        );
    }

    /// Notion's id for a page, and the link a person copies from Notion,
    /// carry no dashes.
    #[test]
    fn an_id_without_dashes_or_inside_a_link_is_an_identifier() {
        for word in [
            "1701d00ab0a14e5f8c3a9d2e4f6b7c81",
            "1701D00AB0A14E5F8C3A9D2E4F6B7C81",
            "https://app.notion.com/p/bridge/Project-Away-Team-1701d00ab0a14e5f8c3a9d2e4f6b7c81",
            "https://www.notion.so/1701d00ab0a14e5f8c3a9d2e4f6b7c81?pvs=4",
            "https://claude.test/chat/1701d00a-b0a1-4e5f-8c3a-9d2e4f6b7c81/",
        ] {
            assert_eq!(
                identifier(word).as_deref(),
                Some("1701d00a-b0a1-4e5f-8c3a-9d2e4f6b7c81"),
                "{word}"
            );
        }
        assert_eq!(identifier("x1701d00ab0a14e5f8c3a9d2e4f6b7c81"), None);
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
