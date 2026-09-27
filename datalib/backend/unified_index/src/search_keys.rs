//! The keys the grid's search bar filters on: what a person types, the
//! `grid_rows` column it compares, and the grid column whose cells it
//! names, which is how a cell's right-click and a column dropped on the
//! search bar know what to write. The search bar is the grid's one filter,
//! so every column a person might narrow by has a key here.

/// One key: `author:picard` compares `grid_rows.author`.
#[derive(Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SearchKey {
    pub key: &'static str,
    pub column: &'static str,
    /// The value is a uuid, which a term may carry as `slug-uuid`, the
    /// slug only there to be read.
    pub uuid: bool,
    /// The grid column whose cells this key filters, and the row field
    /// holding the value a term names (the uuid behind a name, the id
    /// behind a label). None for a key no grid column shows.
    pub cells: Option<(&'static str, &'static str)>,
}

const fn key(
    key: &'static str,
    column: &'static str,
    uuid: bool,
    cells: Option<(&'static str, &'static str)>,
) -> SearchKey {
    SearchKey {
        key,
        column,
        uuid,
        cells,
    }
}

/// Every column the grid shows has a key but Score (qmd's rank, no
/// column) and Contents (the text, which free text searches, and whose
/// cell shows qmd's matched words rather than a value to match).
pub const SEARCH_KEYS: &[SearchKey] = &[
    // The provider's label ("Slack"): one per source *type*.
    key("source", "source_label", false, None),
    // The configured source's id: the grid's Source cell shows its name.
    key(
        "source_id",
        "source_id",
        false,
        Some(("source_ref", "source_id")),
    ),
    key("kind", "kind", false, Some(("kind", "kind"))),
    key("channel", "channel", false, Some(("channel", "channel"))),
    key(
        "convo",
        "conversation_uuid",
        true,
        Some(("conversation_name", "conversation_uuid")),
    ),
    key("author", "author", true, Some(("author", "author"))),
    key("account", "account", true, Some(("account", "account"))),
    key("project", "project", true, Some(("project", "project"))),
    // A Notion page, which a row names apart from its conversation.
    key("notion_page", "notion_page_uuid", true, None),
    // How a diff group's row differs between its two commits.
    key(
        "change",
        "diff_status",
        false,
        Some(("diff_status", "diff_status")),
    ),
    key(
        "created_at",
        "created_at",
        false,
        Some(("created_at", "created_at")),
    ),
    key(
        "modified_at",
        "modified_at",
        false,
        Some(("modified_at", "modified_at")),
    ),
    key(
        "org_name",
        "org_name",
        false,
        Some(("org_name", "org_name")),
    ),
    key(
        "byte_size",
        "byte_size",
        false,
        Some(("byte_size", "byte_size")),
    ),
    key(
        "item_count",
        "item_count",
        false,
        Some(("item_count", "item_count")),
    ),
    key(
        "diff_changed_columns",
        "diff_changed_columns",
        false,
        Some(("diff_changed_columns", "diff_changed_columns")),
    ),
];

/// Older spellings of a key, kept because people type them and saved
/// searches hold them.
const ALIASES: &[(&str, &str)] = &[("source_name", "source_id")];

pub fn lookup(typed: &str) -> Option<&'static SearchKey> {
    let key = ALIASES
        .iter()
        .find(|(alias, _)| *alias == typed)
        .map_or(typed, |(_, key)| *key);
    SEARCH_KEYS.iter().find(|k| k.key == key)
}

/// The key that filters a grid column's cells, and the row field holding
/// the value a term names.
pub fn for_column(id: &str) -> Option<(&'static str, &'static str)> {
    SEARCH_KEYS.iter().find_map(|k| match k.cells {
        Some((column, field)) if column == id => Some((k.key, field)),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each names a real `grid_rows` column, so a filter cannot fail at
    /// prepare time on a name the table lacks.
    #[test]
    fn every_key_compares_a_grid_rows_column() {
        let (_, columns) = datalib_schema::grid_rows::COLUMNS[0];
        for k in SEARCH_KEYS {
            assert!(columns.contains(&k.column), "{}: {}", k.key, k.column);
        }
    }

    #[test]
    fn a_key_is_found_by_its_name_an_alias_or_its_column() {
        assert_eq!(lookup("author").map(|k| k.column), Some("author"));
        assert_eq!(lookup("source_name").map(|k| k.key), Some("source_id"));
        assert_eq!(lookup("subj"), None);
        assert_eq!(for_column("source_ref"), Some(("source_id", "source_id")));
        assert_eq!(
            for_column("conversation_name"),
            Some(("convo", "conversation_uuid"))
        );
        assert_eq!(for_column("score"), None);
    }
}
