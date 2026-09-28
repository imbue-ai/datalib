//! How a table is searched from the search bar: the keys that filter its
//! columns, the order its rows come in when nobody asks for one, and what
//! its free text matches. `#[derive(PortableTable)]` writes a
//! [`SearchTable`] from the table's own columns when the struct is marked
//! `search(...)` (`datalib/backend/etl/macros/README.md`); whoever serves
//! the rows turns a query into SQL through it.

use std::fmt::Debug;
use std::hash::Hash;

/// A column of one table, named by the enum the derive writes for it.
pub trait Column: Copy + Eq + Hash + Debug + Send + Sync + 'static {
    /// The table it is a column of, so a query over its columns knows
    /// which keys and order it reads by.
    type Table: SearchTable<Column = Self>;
    fn as_str(self) -> &'static str;
    /// `None` for a name the table does not have.
    fn parse(s: &str) -> Option<Self>;
}

pub trait SearchTable: 'static {
    type Column: Column;
    const TABLE: &'static str;
    /// Every key the search bar filters this table on, one column each.
    const KEYS: &'static [SearchKey<Self::Column>];
    /// The rows' order when nobody asks for one, most significant first.
    /// Its first column is a row's newest stamp: a group's sample is the
    /// row with the greatest.
    const ORDER: &'static [(Self::Column, Direction)];
    /// Unique per row: it breaks the last ties of any order.
    const PRIMARY_KEY: Self::Column;
    /// The stamp `before:` and `after:` compare, when the table has one.
    const RANGE: Option<Self::Column>;
    /// `is:<word>` keeps the rows where the column is true.
    const FLAGS: &'static [(&'static str, Self::Column)];
    const FREE_TEXT: FreeText<Self::Column>;
    /// The column a column sorts and groups by: itself, or a twin whose
    /// text order is the order a person means (`created_at`, as the
    /// source wrote it, by `created_at_utc`).
    fn sorts_by(column: Self::Column) -> Self::Column;
}

/// `author:picard` compares the `author` column.
#[derive(Debug)]
pub struct SearchKey<C: 'static> {
    pub key: &'static str,
    /// Older spellings, kept because people type them and saved searches
    /// hold them.
    pub aliases: &'static [&'static str],
    pub column: C,
    /// The value is a uuid, which a term may carry as `slug-uuid`, the
    /// slug only there to be read.
    pub uuid: bool,
    /// The words the column can hold, when it is a closed set: a word
    /// outside it is refused with these, never matched against nothing
    /// (`severity:eror` would read as "no errors").
    pub vocabulary: Option<fn() -> Vec<&'static str>>,
}

/// One key per name in a table, so the name is the identity.
impl<C> PartialEq for SearchKey<C> {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl<C> Eq for SearchKey<C> {}

/// What text outside any `key:` matches.
#[derive(Debug, PartialEq, Eq)]
pub enum FreeText<C: 'static> {
    /// The words go to the table's qmd index; SQL never sees them.
    Qmd,
    /// A case-insensitive substring of any of these columns.
    Like(&'static [C]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Asc,
    Desc,
}

impl Direction {
    pub fn sql(self) -> &'static str {
        match self {
            Direction::Asc => "ASC",
            Direction::Desc => "DESC",
        }
    }
}

/// The key a person typed, by its name or one of its aliases.
pub fn key<T: SearchTable>(typed: &str) -> Option<&'static SearchKey<T::Column>> {
    T::KEYS
        .iter()
        .find(|k| k.key == typed || k.aliases.contains(&typed))
}

/// The key that filters a column, when one does.
pub fn key_of<T: SearchTable>(column: T::Column) -> Option<&'static SearchKey<T::Column>> {
    T::KEYS.iter().find(|k| k.column == column)
}
