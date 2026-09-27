//! The order a search's rows come in when the grid asks for one: its
//! columns, most significant first, each ascending or descending. With
//! none, rows come newest first (`touched_at`), or in qmd's rank for free
//! text.

use strum::{EnumString, IntoStaticStr, VariantArray};

/// A grid column a search can be ordered or grouped by, named by the
/// grid's column id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "snake_case")]
pub enum GridColumn {
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

impl GridColumn {
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
    /// [`Score`]: GridColumn::Score
    pub fn sql(self) -> Option<&'static str> {
        Some(match self {
            GridColumn::Score => return None,
            GridColumn::SourceRef => "source_id",
            GridColumn::Kind => "kind",
            GridColumn::ConversationName => "conversation_name",
            GridColumn::Project => "project",
            GridColumn::Channel => "channel",
            GridColumn::CreatedAt => "created_at_utc",
            GridColumn::ModifiedAt => "modified_at_utc",
            GridColumn::Snippet => "preview",
            GridColumn::Author => "author",
            GridColumn::Account => "account",
            GridColumn::OrgName => "org_name",
            GridColumn::ByteSize => "byte_size",
            GridColumn::ItemCount => "item_count",
            GridColumn::DiffStatus => "diff_status",
            GridColumn::DiffChangedColumns => "diff_changed_columns",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sort {
    pub column: GridColumn,
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
            column: GridColumn::parse(column)?,
            descending,
        })
    }

    /// `created_at:desc,author` — each column in turn breaking the ties
    /// of the one before. Score is qmd's rank, which no column holds, so
    /// it sorts alone.
    pub fn parse_order(s: &str) -> Result<Vec<Sort>, String> {
        let order: Vec<Sort> = s
            .split(',')
            .map(|one| Sort::parse(one).ok_or_else(|| format!("unknown sort {one:?}")))
            .collect::<Result<_, _>>()?;
        if order.len() > 1 && order.iter().any(|s| s.column == GridColumn::Score) {
            return Err("score sorts alone: it is qmd's rank, not a column".to_string());
        }
        Ok(order)
    }

    fn direction(self) -> &'static str {
        if self.descending {
            "DESC"
        } else {
            "ASC"
        }
    }
}

/// The `ORDER BY` an order means, with `uuid` breaking the last ties so
/// the order is total, or `None` for none, or for qmd's rank, which no
/// column holds.
pub fn order_by(order: &[Sort]) -> Option<String> {
    let first = order.first()?;
    let keys: Vec<String> = order
        .iter()
        .map(|s| Some(format!("{} {}", s.column.sql()?, s.direction())))
        .collect::<Option<_>>()?;
    Some(format!("{}, uuid {}", keys.join(", "), first.direction()))
}

/// The grid's own order: newest first, a document ahead of its rows at the
/// same moment. Every `grid_rows` index is built for it.
pub const DEFAULT_ORDER: &str = "touched_at_utc DESC, is_document DESC, uuid DESC";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_column_spells_as_it_parses() {
        for column in GridColumn::VARIANTS {
            assert_eq!(GridColumn::parse(column.as_str()), Some(*column));
        }
    }

    /// Each names a real `grid_rows` column, so a sort cannot fail at
    /// prepare time on a name the table lacks.
    #[test]
    fn every_column_but_score_is_a_grid_rows_column() {
        let (_, columns) = datalib_schema::grid_rows::COLUMNS[0];
        for column in GridColumn::VARIANTS {
            match column.sql() {
                None => assert_eq!(*column, GridColumn::Score),
                Some(sql) => assert!(columns.contains(&sql), "{sql} is not a grid_rows column"),
            }
        }
    }

    #[test]
    fn a_sort_reads_as_column_and_direction() {
        assert_eq!(
            Sort::parse("created_at:desc"),
            Some(Sort {
                column: GridColumn::CreatedAt,
                descending: true
            })
        );
        assert_eq!(Sort::parse("kind").map(|s| s.descending), Some(false));
        assert_eq!(Sort::parse("kind:sideways"), None);
        assert_eq!(Sort::parse("no_such_column"), None);
    }

    /// Each column breaks the ties of the one before, and `uuid` the last.
    #[test]
    fn an_order_reads_most_significant_first() {
        let order = Sort::parse_order("author:desc,created_at").unwrap();
        assert_eq!(
            order_by(&order).as_deref(),
            Some("author DESC, created_at_utc ASC, uuid DESC")
        );
        assert_eq!(order_by(&[]), None);
        assert_eq!(order_by(&Sort::parse_order("score").unwrap()), None);
    }

    /// Score is qmd's rank: nothing to break its ties with in SQL, and
    /// nothing it could break the ties of.
    #[test]
    fn an_order_that_does_not_read_says_why() {
        assert!(Sort::parse_order("kind,score:desc")
            .unwrap_err()
            .contains("score"));
        assert!(Sort::parse_order("kind,nope")
            .unwrap_err()
            .contains("\"nope\""));
    }
}
