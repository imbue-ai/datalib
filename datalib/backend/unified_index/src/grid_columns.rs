//! The search grid's columns, by the ids the grid names them with, and the
//! `grid_rows` column behind each: what it sorts and groups by, and what
//! its cells filter on — which is how a sort, a grouping, a cell's
//! right-click and a column dropped on the search bar each reach a column.
//! The keys themselves are declared on `GridRow`.

use datalib_query::table;
use datalib_schema::grid_rows::{GridRow, GridRowColumn};
use strum::{EnumString, IntoStaticStr, VariantArray};

use crate::sort::{Sort, SortBy};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
pub enum GridColumn {
    Score,
    SourceRef,
    Kind,
    ConversationName,
    Project,
    Channel,
    CreatedAt,
    ModifiedAt,
    Snippet,
    Author,
    Account,
    OrgName,
    ByteSize,
    ItemCount,
    DiffStatus,
    DiffChangedColumns,
}

impl GridColumn {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    /// What the column sorts and groups by, and the column its cells
    /// filter on when a key filters them. A cell that shows a name filters
    /// on the id behind it.
    fn backing(self) -> (SortBy<GridRowColumn>, Option<GridRowColumn>) {
        use GridRowColumn as G;
        let same = |c| (SortBy::Column(c), Some(c));
        match self {
            GridColumn::Score => (SortBy::Rank, None),
            GridColumn::SourceRef => same(G::SourceId),
            GridColumn::Kind => same(G::Kind),
            GridColumn::ConversationName => (
                SortBy::Column(G::ConversationName),
                Some(G::ConversationUuid),
            ),
            GridColumn::Project => same(G::Project),
            GridColumn::Channel => same(G::Channel),
            GridColumn::CreatedAt => same(G::CreatedAt),
            GridColumn::ModifiedAt => same(G::ModifiedAt),
            // The cell shows qmd's matched words, or the preview: no value
            // to match.
            GridColumn::Snippet => (SortBy::Column(G::Preview), None),
            GridColumn::Author => same(G::Author),
            GridColumn::Account => same(G::Account),
            GridColumn::OrgName => same(G::OrgName),
            GridColumn::ByteSize => same(G::ByteSize),
            GridColumn::ItemCount => same(G::ItemCount),
            GridColumn::DiffStatus => same(G::DiffStatus),
            GridColumn::DiffChangedColumns => same(G::DiffChangedColumns),
        }
    }

    pub fn sorts(self) -> SortBy<GridRowColumn> {
        self.backing().0
    }

    pub fn filters(self) -> Option<GridRowColumn> {
        self.backing().1
    }
}

/// An order as the grid spells it: `created_at:desc,author`.
pub fn grid_order(s: &str) -> Result<Vec<Sort>, String> {
    Sort::parse_order(s, |id| GridColumn::parse(id).map(GridColumn::sorts))
}

/// The key that filters a grid column's cells, and the row field holding
/// the value a term names (the uuid behind a name, the id behind a label).
pub fn for_column(id: &str) -> Option<(&'static str, &'static str)> {
    let column = GridColumn::parse(id)?.filters()?;
    let key = table::key_of::<GridRow>(column)?;
    Some((key.key, column.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_column_spells_as_it_parses() {
        for column in GridColumn::VARIANTS {
            assert_eq!(GridColumn::parse(column.as_str()), Some(*column));
        }
    }

    /// A column that filters its cells on a column no key compares would
    /// offer Keep only and Exclude, and they would write nothing.
    #[test]
    fn every_filtered_column_has_a_key() {
        for column in GridColumn::VARIANTS {
            if let Some(c) = column.filters() {
                assert!(
                    table::key_of::<GridRow>(c).is_some(),
                    "{column:?} filters on {c:?}, which no key compares"
                );
            }
        }
    }

    /// The keys people type, and how each reads its value. They are
    /// declared on `GridRow`'s columns; one lost there would turn every
    /// saved search that names it into a refusal.
    #[test]
    fn the_grid_keeps_the_keys_people_type() {
        use datalib_query::table::SearchTable;
        let keys: Vec<(&str, &[&str], &str, bool)> = GridRow::KEYS
            .iter()
            .map(|k| (k.key, k.aliases, k.column.as_str(), k.uuid))
            .collect();
        let none: &[&str] = &[];
        assert_eq!(
            keys,
            [
                ("kind", none, "kind", false),
                ("source", none, "source_label", false),
                ("created_at", none, "created_at", false),
                ("modified_at", none, "modified_at", false),
                ("author", none, "author", true),
                ("account", none, "account", true),
                ("project", none, "project", true),
                ("org_name", none, "org_name", false),
                ("channel", none, "channel", false),
                ("convo", none, "conversation_uuid", true),
                ("source_id", &["source_name"][..], "source_id", false),
                ("notion_page", none, "notion_page_uuid", true),
                ("byte_size", none, "byte_size", false),
                ("item_count", none, "item_count", false),
                ("change", none, "diff_status", false),
                ("diff_changed_columns", none, "diff_changed_columns", false),
            ]
        );
    }

    #[test]
    fn a_key_is_found_by_its_name_an_alias_or_its_column() {
        let key = |typed| table::key::<GridRow>(typed).map(|k| k.column);
        assert_eq!(key("author"), Some(GridRowColumn::Author));
        assert_eq!(key("source_name"), Some(GridRowColumn::SourceId));
        assert_eq!(key("subj"), None);
        assert_eq!(for_column("source_ref"), Some(("source_id", "source_id")));
        assert_eq!(
            for_column("conversation_name"),
            Some(("convo", "conversation_uuid"))
        );
        assert_eq!(for_column("score"), None);
        assert_eq!(for_column("snippet"), None);
    }
}
