//! Every SQL statement Datalib runs, named where it is written:
//!
//! ```ignore
//! sql!("runs.latest_metric", "SELECT value FROM metrics WHERE name = ?")
//!     .query()
//!     .bind(name)
//!     .fetch_all(&pool)
//! ```
//!
//! A statement is a [`Sql`], and the only ways to make one are [`sql!`]
//! for text written in the code and [`Sql::dynamic`] for text built while
//! running, which takes a name from [`sql_id!`]. sqlx's own entry points
//! (`sqlx::query`, `raw_sql`, an executor handed a `&str`) are refused by
//! clippy (`datalib/backend/clippy.toml`), so a statement without a name
//! does not build, and `//:lint_repo` refuses two call sites that share
//! one. The name travels in the statement's text ([`tag`]), where
//! `datalib_sql_work` reads it to file the statement's work under it.

pub mod tag;

use std::fmt;
use std::sync::Arc;

use sqlx::query::{Query, QueryAs, QueryScalar};
use sqlx::sqlite::{SqliteArguments, SqliteRow};
use sqlx::{AssertSqlSafe, FromRow, RawSql, SqlSafeStr, SqlStr, Sqlite};

/// One statement, its name at the front of its text. Cheap to clone.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Sql(Text);

#[derive(Clone, PartialEq, Eq, Hash)]
enum Text {
    Written(&'static str),
    Built(Arc<str>),
}

/// The name of a statement whose text is built while running, for
/// [`Sql::dynamic`]. Made by [`sql_id!`], which checks it as it compiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SqlId(&'static str);

/// A statement written in the code: its name, then its text, both string
/// literals.
#[macro_export]
macro_rules! sql {
    ($id:literal, $text:expr $(,)?) => {{
        const SQL: $crate::Sql =
            $crate::Sql::__written($id, ::core::concat!("/* sql:", $id, " */ ", $text));
        SQL
    }};
}

/// The name of a statement built while running.
#[macro_export]
macro_rules! sql_id {
    ($id:literal $(,)?) => {{
        const ID: $crate::SqlId = $crate::SqlId::__new($id);
        ID
    }};
}

/// [`sql!`] in a test, named for the module and line it is written on:
/// `test[datalib_runs::store::tests:812]`. Nothing reads a test's numbers,
/// so it needs no name a person chose, and the place is unique as it is.
#[macro_export]
macro_rules! test_sql {
    ($text:expr $(,)?) => {{
        const SQL: $crate::Sql = $crate::Sql::__written_by_test(::core::concat!(
            "/* sql:test[",
            ::core::module_path!(),
            ":",
            ::core::line!(),
            "] */ ",
            $text
        ));
        SQL
    }};
}

/// [`sql_id!`] in a test, named as [`test_sql!`] names one.
#[macro_export]
macro_rules! test_sql_id {
    () => {{
        const ID: $crate::SqlId = $crate::SqlId::__test(::core::concat!(
            "test[",
            ::core::module_path!(),
            ":",
            ::core::line!(),
            "]"
        ));
        ID
    }};
}

impl SqlId {
    #[doc(hidden)]
    pub const fn __new(id: &'static str) -> Self {
        assert!(
            tag::is_id(id),
            "a statement's name is lowercase words joined by `_` and `.`"
        );
        SqlId(id)
    }

    #[doc(hidden)]
    pub const fn __test(name: &'static str) -> Self {
        SqlId(name)
    }

    pub fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Display for SqlId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl Sql {
    #[doc(hidden)]
    pub const fn __written(id: &'static str, text: &'static str) -> Self {
        assert!(
            tag::is_id(id),
            "a statement's name is lowercase words joined by `_` and `.`"
        );
        Sql(Text::Written(text))
    }

    #[doc(hidden)]
    pub const fn __written_by_test(text: &'static str) -> Self {
        Sql(Text::Written(text))
    }

    /// A statement built while running. Everything in `text` that came
    /// from upstream must be bound or quoted (`doltlite_raw::quote_ident`),
    /// and the caller says why it is safe, as for sqlx's `AssertSqlSafe`.
    pub fn dynamic(id: SqlId, text: impl AsRef<str>) -> Self {
        Self::built(id.0, text.as_ref())
    }

    /// [`Sql::dynamic`] for one statement run over many tables: its work
    /// is filed under `id[table]`, one name per table.
    pub fn dynamic_on(id: SqlId, table: &str, text: impl AsRef<str>) -> Self {
        assert!(
            tag::is_qualifier(table),
            "{table:?} cannot go in a statement's name"
        );
        let name = match id.0.strip_suffix(']') {
            Some(test) => format!("{test}.{table}]"),
            None => format!("{}[{table}]", id.0),
        };
        Self::built(&name, text.as_ref())
    }

    /// [`Sql::dynamic_on`] for a schema statement, filed under the table,
    /// index or view it makes, drops or alters. Schema text is handed
    /// around as plain text, since a store's recorded shape is a hash over
    /// it, and is named where it runs.
    pub fn ddl(id: SqlId, text: impl AsRef<str>) -> Self {
        let text = text.as_ref();
        match tag::object_of(text) {
            Some(object) => Self::dynamic_on(id, object, text),
            None => Self::built(id.0, text),
        }
    }

    fn built(name: &str, text: &str) -> Self {
        assert!(
            tag::of(text).is_none(),
            "{text:?} is named already: run the `Sql` it came from"
        );
        let mut s =
            String::with_capacity(tag::OPEN.len() + name.len() + tag::CLOSE.len() + text.len());
        s.push_str(tag::OPEN);
        s.push_str(name);
        s.push_str(tag::CLOSE);
        s.push_str(text);
        Sql(Text::Built(s.into()))
    }

    /// The statement's text, its name in front.
    pub fn as_str(&self) -> &str {
        match &self.0 {
            Text::Written(s) => s,
            Text::Built(s) => s,
        }
    }

    /// The name the statement carries, brackets and all.
    pub fn name(&self) -> &str {
        tag::of(self.as_str()).expect("every Sql opens with its name")
    }

    pub fn query<'q>(&self) -> Query<'q, Sqlite, SqliteArguments> {
        #[allow(clippy::disallowed_methods)]
        sqlx::query(self.clone())
    }

    pub fn query_as<'q, O>(&self) -> QueryAs<'q, Sqlite, O, SqliteArguments>
    where
        O: for<'r> FromRow<'r, SqliteRow>,
    {
        #[allow(clippy::disallowed_methods)]
        sqlx::query_as(self.clone())
    }

    pub fn query_scalar<'q, O>(&self) -> QueryScalar<'q, Sqlite, O, SqliteArguments>
    where
        (O,): for<'r> FromRow<'r, SqliteRow>,
    {
        #[allow(clippy::disallowed_methods)]
        sqlx::query_scalar(self.clone())
    }

    /// The text run as it stands, without preparing it first: what runs a
    /// script of several statements, each after the first unnamed.
    pub fn raw(&self) -> RawSql {
        #[allow(clippy::disallowed_methods)]
        sqlx::raw_sql(self.clone())
    }
}

impl SqlSafeStr for Sql {
    fn into_sql_str(self) -> SqlStr {
        match self.0 {
            Text::Written(s) => s.into_sql_str(),
            Text::Built(s) => AssertSqlSafe(s).into_sql_str(),
        }
    }
}

impl fmt::Debug for Sql {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for Sql {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_statement_opens_with_its_name() {
        let s = sql!("sql.tests.latest_metric", "SELECT 1");
        assert_eq!(s.as_str(), "/* sql:sql.tests.latest_metric */ SELECT 1");
        assert_eq!(s.name(), "sql.tests.latest_metric");
    }

    #[test]
    fn a_written_statement_may_be_put_together_from_literals() {
        let s = sql!("sql.tests.joined", concat!("SELECT 1", " UNION SELECT 2"));
        assert_eq!(
            s.as_str(),
            "/* sql:sql.tests.joined */ SELECT 1 UNION SELECT 2"
        );
    }

    #[test]
    fn a_built_statement_opens_with_its_name_and_table() {
        let s = Sql::dynamic(
            sql_id!("sql.tests.count"),
            format!("SELECT count(*) FROM {}", "t"),
        );
        assert_eq!(
            s.as_str(),
            "/* sql:sql.tests.count */ SELECT count(*) FROM t"
        );
        let s = Sql::dynamic_on(
            sql_id!("sql.tests.count_on_table"),
            "slack_messages",
            "SELECT 1",
        );
        assert_eq!(s.name(), "sql.tests.count_on_table[slack_messages]");
    }

    #[test]
    fn a_tests_statement_is_named_for_its_place() {
        let s = test_sql!("SELECT 1");
        let line = line!() - 1;
        assert_eq!(
            s.name(),
            format!("test[datalib_sql::tests:{line}]"),
            "{}",
            s.as_str()
        );
        let s = Sql::dynamic_on(test_sql_id!(), "t", "SELECT 1");
        let line = line!() - 1;
        assert_eq!(s.name(), format!("test[datalib_sql::tests:{line}.t]"));
    }

    #[test]
    fn a_schema_statement_is_filed_under_what_it_makes() {
        let s = Sql::ddl(
            sql_id!("sql.tests.open_table"),
            "CREATE TABLE IF NOT EXISTS cas_objects (k TEXT)",
        );
        assert_eq!(s.name(), "sql.tests.open_table[cas_objects]");
        let s = Sql::ddl(
            sql_id!("sql.tests.open_index"),
            "CREATE INDEX t_by_x ON t (x)",
        );
        assert_eq!(s.name(), "sql.tests.open_index[t_by_x]");
        let s = Sql::ddl(
            sql_id!("sql.tests.open_pragma"),
            "PRAGMA journal_mode = WAL",
        );
        assert_eq!(s.name(), "sql.tests.open_pragma");
    }

    #[test]
    #[should_panic(expected = "cannot go in a statement's name")]
    fn a_table_that_could_close_the_comment_is_refused() {
        Sql::dynamic_on(sql_id!("sql.tests.refused_table"), "t */ DROP", "SELECT 1");
    }
}
