//! The order a search's rows come in when the grid asks for one: one of
//! its columns, ascending or descending. With none, rows come newest first
//! (`touched_at`), or in qmd's rank for free text.

use strum::{EnumString, IntoStaticStr, VariantArray};

/// A grid column a search can be ordered by, named by the grid's column id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
pub enum SortColumn {
    /// qmd's relevance. A search without free text has no score, and
    /// comes in its default order instead.
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

impl SortColumn {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    /// The `grid_rows` column behind it, or `None` for [`Score`], which
    /// no column holds. A label the grid shows through a lookup (a
    /// source's name, an account's) sorts by the stored value.
    ///
    /// [`Score`]: SortColumn::Score
    pub fn sql(self) -> Option<&'static str> {
        Some(match self {
            SortColumn::Score => return None,
            SortColumn::SourceRef => "source_id",
            SortColumn::Kind => "kind",
            SortColumn::ConversationName => "conversation_name",
            SortColumn::Project => "project",
            SortColumn::Channel => "channel",
            SortColumn::CreatedAt => "created_at_utc",
            SortColumn::ModifiedAt => "modified_at_utc",
            SortColumn::Snippet => "preview",
            SortColumn::Author => "author",
            SortColumn::Account => "account",
            SortColumn::OrgName => "org_name",
            SortColumn::ByteSize => "byte_size",
            SortColumn::ItemCount => "item_count",
            SortColumn::DiffStatus => "diff_status",
            SortColumn::DiffChangedColumns => "diff_changed_columns",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sort {
    pub column: SortColumn,
    pub descending: bool,
}

impl Sort {
    /// `created_at`, `created_at:asc` or `created_at:desc`. `None` for an
    /// unknown column or direction.
    pub fn parse(s: &str) -> Option<Sort> {
        let (column, direction) = s.split_once(':').unwrap_or((s, "asc"));
        let descending = match direction {
            "asc" => false,
            "desc" => true,
            _ => return None,
        };
        Some(Sort {
            column: SortColumn::parse(column)?,
            descending,
        })
    }

    /// The `ORDER BY` this sort means, with `uuid` breaking ties so the
    /// order is total, or `None` for one no column holds.
    pub fn order_by(self) -> Option<String> {
        let direction = if self.descending { "DESC" } else { "ASC" };
        Some(format!(
            "{} {direction}, uuid {direction}",
            self.column.sql()?
        ))
    }
}

/// The grid's own order: newest first, a document ahead of its rows at the
/// same moment. Every `grid_rows` index is built for it.
pub const DEFAULT_ORDER: &str = "touched_at_utc DESC, is_document DESC, uuid DESC";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_column_spells_as_it_parses() {
        for column in SortColumn::VARIANTS {
            assert_eq!(SortColumn::parse(column.as_str()), Some(*column));
        }
    }

    /// Each names a real `grid_rows` column, so a sort cannot fail at
    /// prepare time on a name the table lacks.
    #[test]
    fn every_column_but_score_is_a_grid_rows_column() {
        let (_, columns) = datalib_schema::grid_rows::COLUMNS[0];
        for column in SortColumn::VARIANTS {
            match column.sql() {
                None => assert_eq!(*column, SortColumn::Score),
                Some(sql) => assert!(columns.contains(&sql), "{sql} is not a grid_rows column"),
            }
        }
    }

    #[test]
    fn a_sort_reads_as_column_and_direction() {
        assert_eq!(
            Sort::parse("created_at:desc"),
            Some(Sort {
                column: SortColumn::CreatedAt,
                descending: true
            })
        );
        assert_eq!(Sort::parse("kind").map(|s| s.descending), Some(false));
        assert_eq!(Sort::parse("kind:sideways"), None);
        assert_eq!(Sort::parse("no_such_column"), None);
        assert_eq!(
            Sort::parse("author:desc")
                .and_then(Sort::order_by)
                .as_deref(),
            Some("author DESC, uuid DESC")
        );
        assert_eq!(Sort::parse("score").and_then(Sort::order_by), None);
    }
}
