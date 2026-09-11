//! Schema introspection and DDL synthesis for the SQLite→doltlite mirror.

use anyhow::{Context, Result};
use sqlx::sqlite::SqliteConnection;
use sqlx::Row;

/// One column of a mirrored table, in the shape `PRAGMA table_xinfo`
/// reports it — which is also, verified against doltlite, the shape it
/// reads back as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSpec {
    pub name: String,
    /// Declared type, or empty for an untyped column. Untyped is
    /// preserved rather than defaulted to TEXT: SQLite (and doltlite)
    /// store the *value's* type in an untyped column, so a Lightroom
    /// column that holds an integer in one row and a blob in the next
    /// round-trips only if the mirror leaves it untyped too.
    pub decl_type: String,
    pub not_null: bool,
}

impl ColumnSpec {
    /// The `"name" TYPE [NOT NULL]` fragment, usable both inside
    /// `CREATE TABLE` and after `ALTER TABLE … ADD COLUMN`. No
    /// `DEFAULT`: see the module docs for why the mirror does not carry
    /// one.
    pub fn decl(&self) -> String {
        let mut s = quote_ident(&self.name);
        if !self.decl_type.is_empty() {
            s.push(' ');
            s.push_str(&quote_ident(&self.decl_type));
        }
        if self.not_null {
            s.push_str(" NOT NULL");
        }
        s
    }
}

/// A table as it will exist in the mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSpec {
    pub name: String,
    /// Mirrored columns, in source order, minus any the column filter
    /// dropped.
    pub columns: Vec<ColumnSpec>,
    /// The mirror's primary key. Empty means keyless — doltlite accepts
    /// keyless tables and still diffs them (by row multiset), which is
    /// the honest representation of a source table that has no key
    /// either.
    pub pk: Vec<String>,
    /// Source columns the filter dropped, for the run summary.
    pub dropped_columns: Vec<String>,
    /// Where [`Self::pk`] came from, for the run summary and for
    /// explaining a surprising diff.
    pub key_origin: KeyOrigin,
}

/// Provenance of a mirrored table's primary key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOrigin {
    /// The source table's own `PRIMARY KEY`.
    Declared,
    /// A single-column UNIQUE index named in `stable_key_columns`, chosen
    /// over the declared key. This is the Lightroom `id_global` case.
    StableUnique,
    /// A column named in `stable_key_columns` that the source did not
    /// declare UNIQUE, but which this run checked holds a distinct
    /// non-NULL value in every row. This is the Apple Photos `ZUUID`
    /// case.
    StableVerified,
    /// An explicit `primary_keys` config override.
    Override,
    /// No key: the source had none and no stable candidate matched.
    Keyless,
}

impl TableSpec {
    /// `CREATE TABLE …` for this table. No `IF NOT EXISTS`: the caller
    /// has just dropped it, and a silent no-op here would mean quietly
    /// keeping a stale schema.
    pub fn create_ddl(&self) -> String {
        let mut cols: Vec<String> = self.columns.iter().map(|c| c.decl()).collect();
        if !self.pk.is_empty() {
            let key: Vec<String> = self.pk.iter().map(|c| quote_ident(c)).collect();
            cols.push(format!("PRIMARY KEY ({})", key.join(", ")));
        }
        format!(
            "CREATE TABLE {} ({})",
            quote_ident(&self.name),
            cols.join(", ")
        )
    }

    fn column_list(&self) -> String {
        self.columns
            .iter()
            .map(|c| quote_ident(&c.name))
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn copy_sql(&self, from_schema: &str) -> String {
        let list = self.column_list();
        format!(
            "INSERT INTO main.{t} ({list}) SELECT {list} FROM {s}.{t}",
            t = quote_ident(&self.name),
            s = quote_ident(from_schema),
        )
    }
}

/// Double-quote an identifier, escaping embedded quotes. Table and
/// column names here come out of `sqlite_master` rather than from the
/// user, but they still get quoted: SQLite permits spaces, keywords, and
/// punctuation in identifiers, and a mirror that only worked on
/// well-behaved schemas would be a mirror with a footgun in it.
pub fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// How SQLite classifies an entry in `sqlite_master`. Only two kinds
/// hold rows we copy; the others exist so the mirror can say why it
/// left something out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    /// An ordinary table.
    Table,
    /// `CREATE VIRTUAL TABLE`: readable as rows only when its module is
    /// compiled into the engine doing the reading.
    Virtual,
    /// A virtual table's backing storage (`<name>_node`, `_content`,
    /// …). Opaque pages of an index, not data — the virtual table above
    /// it *is* the data.
    Shadow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTable {
    pub name: String,
    pub kind: TableKind,
}

/// Every table in `schema`, classified. Views are not tables and are
/// not listed. `PRAGMA table_list` is what tells a shadow table from a
/// real one; `sqlite_master` calls both `table`.
pub async fn source_tables(conn: &mut SqliteConnection, schema: &str) -> Result<Vec<SourceTable>> {
    let rows = sqlx::query(
        "SELECT name, type FROM pragma_table_list \
         WHERE schema = ? AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .bind(schema)
    .fetch_all(&mut *conn)
    .await
    .with_context(|| format!("list tables in {schema}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let kind = match r.get::<String, _>("type").as_str() {
            "table" => TableKind::Table,
            "virtual" => TableKind::Virtual,
            "shadow" => TableKind::Shadow,
            _ => continue,
        };
        out.push(SourceTable {
            name: r.get("name"),
            kind,
        });
    }
    Ok(out)
}

pub async fn table_names(conn: &mut SqliteConnection, schema: &str) -> Result<Vec<String>> {
    Ok(source_tables(conn, schema)
        .await?
        .into_iter()
        .map(|t| t.name)
        .collect())
}

/// One source column as `PRAGMA table_xinfo` reports it, plus the bits
/// [`ColumnSpec`] drops (pk position, generated-ness).
#[derive(Debug, Clone)]
pub struct SourceColumn {
    pub spec: ColumnSpec,
    /// 1-based position in the declared primary key; 0 if not part of it.
    pub pk_seq: i64,
    /// `hidden` 2/3 ⇒ GENERATED. A generated column has no stored value
    /// to copy, so it is not mirrored.
    pub generated: bool,
}

pub async fn table_columns(
    conn: &mut SqliteConnection,
    schema: &str,
    table: &str,
) -> Result<Vec<SourceColumn>> {
    let sql = format!(
        "PRAGMA {}.table_xinfo({})",
        quote_ident(schema),
        quote_ident(table)
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(&mut *conn)
        .await
        .with_context(|| format!("table_xinfo({schema}.{table})"))?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let name: String = r.try_get("name").unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let hidden: i64 = r.try_get("hidden").unwrap_or(0);
        let not_null: i64 = r.try_get("notnull").unwrap_or(0);
        out.push(SourceColumn {
            spec: ColumnSpec {
                name,
                decl_type: r.try_get("type").unwrap_or_default(),
                not_null: not_null != 0,
            },
            pk_seq: r.try_get("pk").unwrap_or(0),
            generated: hidden == 2 || hidden == 3,
        });
    }
    Ok(out)
}

/// Columns covered by a single-column UNIQUE index on `table` — the
/// candidate stable keys. Includes both `UNIQUE` constraints (`origin`
/// `u`) and standalone `CREATE UNIQUE INDEX` (`origin` `c`); excludes
/// partial indexes, which don't constrain every row.
pub async fn unique_single_columns(
    conn: &mut SqliteConnection,
    schema: &str,
    table: &str,
) -> Result<Vec<String>> {
    let sql = format!(
        "PRAGMA {}.index_list({})",
        quote_ident(schema),
        quote_ident(table)
    );
    let idx_rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(&mut *conn)
        .await
        .with_context(|| format!("index_list({schema}.{table})"))?;
    let mut out = Vec::new();
    for r in &idx_rows {
        let unique: i64 = r.try_get("unique").unwrap_or(0);
        let partial: i64 = r.try_get("partial").unwrap_or(0);
        if unique == 0 || partial != 0 {
            continue;
        }
        let idx_name: String = r.try_get("name").unwrap_or_default();
        if idx_name.is_empty() {
            continue;
        }
        let info_sql = format!(
            "PRAGMA {}.index_info({})",
            quote_ident(schema),
            quote_ident(&idx_name)
        );
        let cols = sqlx::query(sqlx::AssertSqlSafe(info_sql))
            .fetch_all(&mut *conn)
            .await
            .with_context(|| format!("index_info({schema}.{idx_name})"))?;
        if cols.len() != 1 {
            continue;
        }
        // A NULL name means an expression index — nothing to key on.
        if let Ok(Some(name)) = cols[0].try_get::<Option<String>, _>("name") {
            out.push(name);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ColumnSpec {
        ColumnSpec {
            name: name.into(),
            decl_type: ty.into(),
            not_null: false,
        }
    }

    fn spec(cols: Vec<ColumnSpec>, pk: Vec<&str>) -> TableSpec {
        TableSpec {
            name: "t".into(),
            columns: cols,
            pk: pk.into_iter().map(String::from).collect(),
            dropped_columns: Vec::new(),
            key_origin: KeyOrigin::Declared,
        }
    }

    #[test]
    fn ddl_quotes_and_keys() {
        let s = spec(
            vec![col("id_global", ""), col("id_local", "INTEGER")],
            vec!["id_global"],
        );
        assert_eq!(
            s.create_ddl(),
            r#"CREATE TABLE "t" ("id_global", "id_local" "INTEGER", PRIMARY KEY ("id_global"))"#
        );
    }

    #[test]
    fn keyless_ddl_has_no_primary_key_clause() {
        let s = spec(vec![col("a", "")], vec![]);
        assert_eq!(s.create_ddl(), r#"CREATE TABLE "t" ("a")"#);
    }

    #[test]
    fn decl_carries_not_null() {
        let c = ColumnSpec {
            name: "xmp".into(),
            decl_type: String::new(),
            not_null: true,
        };
        assert_eq!(c.decl(), r#""xmp" NOT NULL"#);
    }

    #[test]
    fn a_declared_type_carrying_sql_is_quoted_shut() {
        // A source catalog can declare this type — SQLite lets a type
        // name be a quoted name — and `table_xinfo` hands it back with
        // the quotes gone. Unquoted it would close our column
        // definition; quoted it is just a strange type.
        let s = spec(
            vec![col("id", r#"INTEGER, x TEXT); DROP TABLE t; --"#)],
            vec![],
        );
        assert_eq!(
            s.create_ddl(),
            r#"CREATE TABLE "t" ("id" "INTEGER, x TEXT); DROP TABLE t; --")"#
        );
    }

    #[test]
    fn quote_ident_escapes_embedded_quotes() {
        assert_eq!(quote_ident(r#"we"ird"#), r#""we""ird""#);
    }

    #[test]
    fn a_declared_type_with_a_quote_in_it_is_escaped_too() {
        let s = spec(vec![col("id", r#"IN"TEGER"#)], vec![]);
        assert_eq!(s.create_ddl(), r#"CREATE TABLE "t" ("id" "IN""TEGER")"#);
    }
}
