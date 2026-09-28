//! The order a search's rows come in when the grid asks for one: its
//! columns, most significant first, each ascending or descending. With
//! none, rows come in the table's own order (`SearchTable::ORDER`), or in
//! qmd's rank for free text.

use datalib_query::table::{Column, SearchTable};
use datalib_schema::grid_rows::GridRowColumn;

/// What one step of an order sorts by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortBy<C> {
    /// qmd's relevance, which no column holds. A search without free
    /// text has none, and comes in its default order instead.
    Rank,
    Column(C),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sort<C = GridRowColumn> {
    pub by: SortBy<C>,
    pub descending: bool,
}

impl<C: Column> Sort<C> {
    /// `created_at:desc,author` — each column in turn breaking the ties
    /// of the one before, each named as the caller's `resolve` reads a
    /// grid column's id. Rank sorts alone.
    pub fn parse_order(
        s: &str,
        resolve: impl Fn(&str) -> Option<SortBy<C>>,
    ) -> Result<Vec<Self>, String> {
        let order: Vec<Self> = s
            .split(',')
            .map(|one| {
                let (column, direction) = one.split_once(':').unwrap_or((one, "asc"));
                let descending = match direction {
                    "asc" => false,
                    "desc" => true,
                    _ => return Err(format!("unknown sort {one:?}")),
                };
                let by = resolve(column).ok_or_else(|| format!("unknown sort {one:?}"))?;
                Ok(Sort { by, descending })
            })
            .collect::<Result<_, _>>()?;
        if order.len() > 1 && order.iter().any(|s| s.by == SortBy::Rank) {
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

/// The `ORDER BY` an order means, with the primary key breaking the last
/// ties so the order is total, or `None` for none, or for qmd's rank,
/// which no column holds.
pub fn order_by<C: Column>(order: &[Sort<C>]) -> Option<String> {
    let first = order.first()?;
    let keys: Vec<String> = order
        .iter()
        .map(|s| match s.by {
            SortBy::Rank => None,
            SortBy::Column(c) => Some(format!(
                "{} {}",
                <C::Table as SearchTable>::sorts_by(c).as_str(),
                s.direction()
            )),
        })
        .collect::<Option<_>>()?;
    Some(format!(
        "{}, {} {}",
        keys.join(", "),
        <C::Table as SearchTable>::PRIMARY_KEY.as_str(),
        first.direction()
    ))
}

/// The table's own order, which its indexes are built for.
pub fn default_order<T: SearchTable>() -> String {
    T::ORDER
        .iter()
        .map(|(c, d)| format!("{} {}", c.as_str(), d.sql()))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid_columns::grid_order;
    use datalib_schema::grid_rows::GridRow;

    #[test]
    fn a_sort_reads_as_column_and_direction() {
        assert_eq!(
            grid_order("created_at:desc"),
            Ok(vec![Sort {
                by: SortBy::Column(GridRowColumn::CreatedAt),
                descending: true
            }])
        );
        assert_eq!(grid_order("kind").map(|o| o[0].descending), Ok(false));
        assert!(grid_order("kind:sideways").is_err());
        assert!(grid_order("no_such_column").is_err());
    }

    /// Each column breaks the ties of the one before, and the primary key
    /// the last. A stamp sorts by its UTC twin.
    #[test]
    fn an_order_reads_most_significant_first() {
        let order = grid_order("author:desc,created_at").unwrap();
        assert_eq!(
            order_by(&order).as_deref(),
            Some("author DESC, created_at_utc ASC, uuid DESC")
        );
        assert_eq!(order_by::<GridRowColumn>(&[]), None);
        assert_eq!(order_by(&grid_order("score").unwrap()), None);
    }

    /// Score is qmd's rank: nothing to break its ties with in SQL, and
    /// nothing it could break the ties of.
    #[test]
    fn an_order_that_does_not_read_says_why() {
        assert!(grid_order("kind,score:desc").unwrap_err().contains("score"));
        assert!(grid_order("kind,nope").unwrap_err().contains("\"nope\""));
    }

    #[test]
    fn the_grids_own_order_is_newest_first() {
        assert_eq!(
            default_order::<GridRow>(),
            "touched_at_utc DESC, is_document DESC, uuid DESC"
        );
    }
}
