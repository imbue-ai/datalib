//! The search grid's columns, by the ids the grid names them with, and the
//! `grid_rows` column behind each (`crate::view`).

use datalib_schema::grid_rows::GridRowColumn;
use strum::{EnumString, IntoStaticStr, VariantArray};

use crate::sort::SortBy;
use crate::view::View;

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
}

impl View for GridColumn {
    type Column = GridRowColumn;

    fn parse(id: &str) -> Option<Self> {
        id.parse().ok()
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{for_column, unkeyed};
    use datalib_query::table;
    use datalib_schema::grid_rows::GridRow;

    #[test]
    fn every_column_spells_as_it_parses() {
        for column in GridColumn::VARIANTS {
            assert_eq!(GridColumn::parse(column.as_str()), Some(*column));
        }
    }

    #[test]
    fn every_filtered_column_has_a_key() {
        assert_eq!(unkeyed(GridColumn::VARIANTS), Vec::<String>::new());
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
        assert_eq!(
            for_column::<GridColumn>("source_ref"),
            Some(("source_id", "source_id"))
        );
        assert_eq!(
            for_column::<GridColumn>("conversation_name"),
            Some(("convo", "conversation_uuid"))
        );
        assert_eq!(for_column::<GridColumn>("score"), None);
        assert_eq!(for_column::<GridColumn>("snippet"), None);
    }
}
