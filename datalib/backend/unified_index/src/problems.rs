//! The problems table's columns, by the ids its grid names them with, and
//! the `problems` column behind each (`crate::view`). Its keys, order and
//! free text are declared on `ProblemRow`: free text is a substring of
//! the sample, the field and the rule, since there is no qmd index of
//! problems.

use datalib_schema::problems::ProblemRowColumn;
use strum::{EnumString, IntoStaticStr, VariantArray};

use crate::query::ParsedQuery;
use crate::sort::SortBy;
use crate::view::View;

pub type ProblemsQuery = ParsedQuery<ProblemRowColumn>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
pub enum ProblemColumn {
    /// The severity drawn as a coloured chip.
    SeverityChip,
    SourceRef,
    Stage,
    Reason,
    Field,
    Sample,
    MarkdownUuid,
    Outcome,
    Rule,
    ItemUuid,
    FirstSeenAtUtc,
    LastSeenAtUtc,
    ScopeKind,
    ScopeKey,
    Path,
    RenderVersion,
    ProblemUuid,
}

impl ProblemColumn {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

impl View for ProblemColumn {
    type Column = ProblemRowColumn;

    fn parse(id: &str) -> Option<Self> {
        id.parse().ok()
    }

    fn backing(self) -> (SortBy<ProblemRowColumn>, Option<ProblemRowColumn>) {
        use ProblemRowColumn as P;
        let same = |c| (SortBy::Column(c), Some(c));
        // Sorted, never matched whole: free text reads these.
        let text = |c| (SortBy::Column(c), None);
        match self {
            ProblemColumn::SeverityChip => same(P::Severity),
            ProblemColumn::SourceRef => same(P::SourceId),
            ProblemColumn::Stage => same(P::Stage),
            ProblemColumn::Reason => same(P::Reason),
            ProblemColumn::Field => same(P::Field),
            ProblemColumn::Sample => text(P::Sample),
            // The document a markdown-scoped problem is about is its
            // scope key.
            ProblemColumn::MarkdownUuid => same(P::ScopeKey),
            ProblemColumn::Outcome => same(P::Outcome),
            ProblemColumn::Rule => same(P::Rule),
            ProblemColumn::ItemUuid => same(P::ItemUuid),
            ProblemColumn::FirstSeenAtUtc => text(P::FirstSeenAtUtc),
            ProblemColumn::LastSeenAtUtc => text(P::LastSeenAtUtc),
            ProblemColumn::ScopeKind => same(P::ScopeKind),
            ProblemColumn::ScopeKey => same(P::ScopeKey),
            ProblemColumn::Path => text(P::Path),
            ProblemColumn::RenderVersion => text(P::RenderVersion),
            ProblemColumn::ProblemUuid => same(P::ProblemUuid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::build_where;
    use crate::view::{for_column, unkeyed};

    fn parse(q: &str) -> ProblemsQuery {
        ParsedQuery::parse(q)
    }

    #[test]
    fn keys_map_to_columns_and_old_spellings_agree() {
        let q = parse("source:slack -severity:info created");
        assert_eq!(q.refusal(), None);
        let (sql, params) = build_where(&q);
        assert_eq!(
            sql,
            " WHERE source_id = ? AND (severity IS NULL OR severity != ?) AND \
             (LOWER(COALESCE(field, '')) LIKE ? OR LOWER(COALESCE(rule, '')) LIKE ? \
             OR LOWER(COALESCE(sample, '')) LIKE ?)"
        );
        assert_eq!(
            params,
            ["slack", "info", "%created%", "%created%", "%created%"]
        );
        for (old, new) in [
            ("markdown_uuid:d", "doc:d"),
            ("scope_key:d", "doc:d"),
            ("uuid:i", "item:i"),
            ("item_uuid:i", "item:i"),
            ("problem_uuid:p", "problem:p"),
            ("scope_kind:entity", "scope:entity"),
        ] {
            assert_eq!(build_where(&parse(old)), build_where(&parse(new)), "{old}");
        }
    }

    /// A misspelled vocabulary word is refused with the valid words,
    /// never silently matched against nothing.
    #[test]
    fn a_word_outside_the_vocabulary_is_refused() {
        assert_eq!(
            parse("severity:eror stage:parse").refusal().as_deref(),
            Some("`severity:` takes one of error, warning, info, not `eror`")
        );
        assert_eq!(
            parse("-stage:prase").refusal().map(|w| w.contains("parse")),
            Some(true)
        );
        assert_eq!(parse("severity:error stage:*").refusal(), None);
        assert!(parse("colour:red").refusal().unwrap().contains("`colour:`"));
    }

    /// `before:`/`after:` bound when a problem was last seen; there is no
    /// `is:` and no qmd here.
    #[test]
    fn a_range_is_the_last_seen_stamp() {
        let (sql, _) = build_where(&parse("after:2025-01-01T00:00:00Z"));
        assert_eq!(sql, " WHERE last_seen_at_utc > ?");
        assert!(parse("is:document").refusal().is_some());
        assert!(parse("qmd:tea").refusal().is_some());
    }

    #[test]
    fn every_column_spells_as_it_parses_and_every_filtered_one_has_a_key() {
        for column in ProblemColumn::VARIANTS {
            assert_eq!(ProblemColumn::parse(column.as_str()), Some(*column));
        }
        assert_eq!(unkeyed(ProblemColumn::VARIANTS), Vec::<String>::new());
        assert_eq!(
            for_column::<ProblemColumn>("markdown_uuid"),
            Some(("doc", "scope_key"))
        );
        assert_eq!(
            for_column::<ProblemColumn>("source_ref"),
            Some(("source_id", "source_id"))
        );
        assert_eq!(for_column::<ProblemColumn>("sample"), None);
    }
}
