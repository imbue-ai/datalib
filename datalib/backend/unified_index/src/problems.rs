//! The search-bar grammar over the index's `problems` table: which key
//! names which column, which keys take a closed vocabulary, and the
//! `WHERE` clause a query becomes. Free text is a substring match on the
//! sample, the field and the rule — there is no qmd index of problems.

use datalib_query::{Term, Token};
use datalib_schema::problems::{Outcome, Reason, ScopeKind, Severity, Stage};
use strum::VariantArray;

/// The keys the grammar accepts, each naming one column. Spelled the way
/// a person would type them; the column names are the enum's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum Key {
    #[strum(serialize = "source_id", serialize = "source")]
    SourceId,
    Severity,
    Stage,
    Outcome,
    Reason,
    #[strum(serialize = "scope", serialize = "scope_kind")]
    ScopeKind,
    #[strum(
        serialize = "markdown_uuid",
        serialize = "doc",
        serialize = "scope_key"
    )]
    ScopeKey,
    #[strum(serialize = "item_uuid", serialize = "uuid", serialize = "item")]
    ItemUuid,
    #[strum(serialize = "problem_uuid", serialize = "problem")]
    ProblemUuid,
    Field,
    Rule,
}

impl Key {
    pub fn parse(s: &str) -> Option<Key> {
        s.parse().ok()
    }

    fn column(self) -> &'static str {
        match self {
            Key::SourceId => "source_id",
            Key::Severity => "severity",
            Key::Stage => "stage",
            Key::Outcome => "outcome",
            Key::Reason => "reason",
            Key::ScopeKind => "scope_kind",
            Key::ScopeKey => "scope_key",
            Key::ItemUuid => "item_uuid",
            Key::ProblemUuid => "problem_uuid",
            Key::Field => "field",
            Key::Rule => "rule",
        }
    }

    /// The spellings a closed-vocabulary key accepts, or `None` for a
    /// key that takes any text. A value outside the set is an error,
    /// not an empty result: `severity:eror` matching nothing would read
    /// as "no errors".
    fn vocabulary(self) -> Option<&'static [&'static str]> {
        fn words<T: VariantArray + Copy + Into<&'static str>>() -> Vec<&'static str> {
            T::VARIANTS.iter().map(|&v| v.into()).collect()
        }
        // Leaked once per process per key: the lists are tiny and the
        // grammar is queried on every request.
        macro_rules! leak {
            ($t:ty) => {{
                static CELL: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
                Some(CELL.get_or_init(words::<$t>).as_slice())
            }};
        }
        match self {
            Key::Severity => leak!(Severity),
            Key::Stage => leak!(Stage),
            Key::Outcome => leak!(Outcome),
            Key::Reason => leak!(Reason),
            Key::ScopeKind => leak!(ScopeKind),
            _ => None,
        }
    }
}

/// A query the repo can run: the clause, its bound values, and what in
/// the text it could not use.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProblemsQuery {
    /// With a leading ` WHERE`, or empty.
    pub where_sql: String,
    pub params: Vec<String>,
    /// One line per term the grammar refused, in the user's words.
    pub errors: Vec<String>,
}

pub fn parse(q: &str) -> ProblemsQuery {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut free: Vec<String> = Vec::new();
    for token in datalib_query::parse(q) {
        match token {
            Token::Free(text) => free.push(text),
            Token::Term(Term { key, value, negate }) => {
                let Some(k) = Key::parse(&key) else {
                    errors.push(format!("unknown filter `{key}:`"));
                    continue;
                };
                if let Some(words) = k.vocabulary() {
                    if !words.contains(&value.as_str()) {
                        errors.push(format!(
                            "`{key}:` takes one of {}, not `{value}`",
                            words.join(", ")
                        ));
                        continue;
                    }
                }
                let col = k.column();
                // Nullable columns: NULL would pass `col != ?` as unknown
                // and be dropped; a negation keeps them.
                clauses.push(if negate {
                    format!("({col} IS NULL OR {col} != ?)")
                } else {
                    format!("{col} = ?")
                });
                params.push(value);
            }
        }
    }
    if !free.is_empty() {
        let needle = format!("%{}%", free.join(" ").to_lowercase());
        clauses.push(
            "(LOWER(sample) LIKE ? OR LOWER(COALESCE(field, '')) LIKE ? \
             OR LOWER(COALESCE(rule, '')) LIKE ?)"
                .into(),
        );
        params.extend([needle.clone(), needle.clone(), needle]);
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    ProblemsQuery {
        where_sql,
        params,
        errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_columns_and_aliases_agree() {
        assert_eq!(Key::parse("source"), Some(Key::SourceId));
        assert_eq!(Key::parse("source_id"), Some(Key::SourceId));
        assert_eq!(Key::parse("doc"), Some(Key::ScopeKey));
        assert_eq!(Key::parse("nope"), None);
        let q = parse("source_id:slack -severity:info created");
        assert_eq!(
            q.where_sql,
            " WHERE source_id = ? AND (severity IS NULL OR severity != ?) AND \
             (LOWER(sample) LIKE ? OR LOWER(COALESCE(field, '')) LIKE ? \
             OR LOWER(COALESCE(rule, '')) LIKE ?)"
        );
        assert_eq!(
            q.params,
            vec!["slack", "info", "%created%", "%created%", "%created%"]
        );
        assert!(q.errors.is_empty());
    }

    /// A misspelled vocabulary word is refused with the valid words,
    /// never silently matched against nothing.
    #[test]
    fn a_word_outside_the_vocabulary_is_an_error_not_an_empty_result() {
        let q = parse("severity:eror stage:parse");
        assert_eq!(
            q.errors,
            vec!["`severity:` takes one of error, warning, info, not `eror`"]
        );
        assert_eq!(q.where_sql, " WHERE stage = ?");
        assert_eq!(q.params, vec!["parse"]);
        let unknown = parse("colour:red");
        assert_eq!(unknown.errors, vec!["unknown filter `colour:`"]);
        assert_eq!(unknown.where_sql, "");
    }
}
