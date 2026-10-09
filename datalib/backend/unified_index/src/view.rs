//! A grid's own columns, by the ids it names them with, over one table's
//! columns: what each sorts and groups by, and what its cells filter on.
//! That is how a sort, a grouping, a cell's right-click and a column
//! dropped on the search bar each reach a column. The keys themselves are
//! declared on the table (`datalib_query::table`).

use datalib_query::table::{self, Column, SearchTable};

use crate::sort::{Sort, SortBy};

pub trait View: Copy + 'static {
    type Column: Column;
    /// `None` for an id the grid does not have.
    fn parse(id: &str) -> Option<Self>;
    /// What the column sorts and groups by, and the column its cells
    /// filter on when a key filters them. A cell that shows a name
    /// filters on the id behind it.
    fn backing(self) -> (SortBy<Self::Column>, Option<Self::Column>);
    /// Where no column key filters the cells: a key that reads the search
    /// terms (`crate::terms_keys`), and the row field holding the value.
    fn terms_filter(self) -> Option<(&'static str, &'static str)> {
        None
    }
}

/// An order as the grid spells it: `created_at:desc,author`.
pub fn order<V: View>(s: &str) -> Result<Vec<Sort<V::Column>>, String> {
    Sort::parse_order(s, |id| V::parse(id).map(|v| v.backing().0))
}

/// The column a grid column groups by, as its rows' text order reads it.
pub fn group_column<V: View>(id: &str) -> Result<V::Column, String> {
    match V::parse(id).map(|v| v.backing().0) {
        Some(SortBy::Column(c)) => Ok(<<V::Column as Column>::Table as SearchTable>::sorts_by(c)),
        _ => Err(format!("rows cannot be grouped by {id:?}")),
    }
}

/// The key that filters a grid column's cells, and the row field holding
/// the value a term names (the uuid behind a name, the id behind a label).
pub fn for_column<V: View>(id: &str) -> Option<(&'static str, &'static str)> {
    let view = V::parse(id)?;
    let column = view.backing().1?;
    match table::key_of::<<V::Column as Column>::Table>(column) {
        Some(key) => Some((key.key, column.as_str())),
        None => view.terms_filter(),
    }
}

/// Every column of a view that filters its cells has a key, or Keep only
/// and Exclude would be offered and write nothing.
#[cfg(test)]
pub fn unkeyed<V: View + std::fmt::Debug>(all: &[V]) -> Vec<String> {
    all.iter()
        .filter_map(|v| {
            let c = v.backing().1?;
            (table::key_of::<<V::Column as Column>::Table>(c).is_none()
                && v.terms_filter().is_none())
            .then(|| format!("{v:?} filters on {c:?}, which no key compares"))
        })
        .collect()
}
