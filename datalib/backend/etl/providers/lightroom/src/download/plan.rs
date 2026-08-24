//! Schema introspection and DDL synthesis for the SQLite→doltlite mirror.
//!
//! Everything here is generic over "some SQLite database attached under
//! a schema alias" — nothing knows what Lightroom is. The one
//! Lightroom-shaped decision (prefer `id_global` over `id_local` as the
//! key) arrives as config, in [`super::mirror::MirrorOptions`].
//!
//! ## Why introspect instead of replaying `sqlite_master.sql`
//!
//! Copying each `CREATE TABLE` verbatim out of the source's
//! `sqlite_master` is tempting and does work — doltlite parses all 133
//! of a stock Lightroom catalog's table definitions unchanged. But
//! verbatim DDL forecloses the two things this ingester needs to do to
//! it: drop a column (the XMP filter) and choose a different primary key
//! (the stable-key rewrite). Both are textual surgery on arbitrary SQL
//! if you start from the source text, and neither is if you start from
//! `PRAGMA table_info`.
//!
//! What introspection loses is CHECK constraints, foreign keys, and
//! collations — none of which a mirror needs, because the mirror is
//! never the thing being written to by an application. Indexes and
//! triggers are dropped on purpose: doltlite stores each table as a
//! prolly tree keyed by its primary key, so a secondary index buys a
//! backup nothing and costs it space in every commit.
//!
//! There is deliberately no schema-reconciliation logic here. Every run
//! drops each mirror table and recreates it from the source, so the
//! mirror's schema is never *compared* to anything — it is simply
//! rebuilt. See [`super::mirror`] for why that is free.

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
    /// Default expression, verbatim SQL text (`''`, `-63113817600`,
    /// `'unset'`). Spliced back into the DDL as-is.
    pub default: Option<String>,
}

impl ColumnSpec {
    /// The `"name" TYPE [NOT NULL] [DEFAULT expr]` fragment, usable both
    /// inside `CREATE TABLE` and after `ALTER TABLE … ADD COLUMN`.
    pub fn decl(&self) -> String {
        let mut s = quote_ident(&self.name);
        if !self.decl_type.is_empty() {
            s.push(' ');
            s.push_str(&self.decl_type);
        }
        if self.not_null {
            s.push_str(" NOT NULL");
        }
        if let Some(d) = &self.default {
            s.push_str(" DEFAULT ");
            s.push_str(d);
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

    /// The mirrored column list, quoted: `"a", "b"`.
    fn column_list(&self) -> String {
        self.columns
            .iter()
            .map(|c| quote_ident(&c.name))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// `INSERT INTO main."t" ("a","b") SELECT "a","b" FROM <schema>."t"`.
    ///
    /// The column list is explicit on both sides so a dropped column (or
    /// a source that gained one we're not mirroring yet) can't shift the
    /// positional mapping.
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

/// Table names in an attached schema, excluding SQLite's internal ones.
pub async fn table_names(conn: &mut SqliteConnection, schema: &str) -> Result<Vec<String>> {
    let sql = format!(
        "SELECT name FROM {}.sqlite_master \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        quote_ident(schema)
    );
    let rows = sqlx::query(&sql)
        .fetch_all(&mut *conn)
        .await
        .with_context(|| format!("list tables in {schema}"))?;
    Ok(rows.iter().map(|r| r.get::<String, _>("name")).collect())
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

/// Introspect one table's columns.
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
    let rows = sqlx::query(&sql)
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
                default: r.try_get("dflt_value").ok().flatten(),
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
    let idx_rows = sqlx::query(&sql)
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
        let cols = sqlx::query(&info_sql)
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
            default: None,
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
            r#"CREATE TABLE "t" ("id_global", "id_local" INTEGER, PRIMARY KEY ("id_global"))"#
        );
    }

    #[test]
    fn keyless_ddl_has_no_primary_key_clause() {
        let s = spec(vec![col("a", "")], vec![]);
        assert_eq!(s.create_ddl(), r#"CREATE TABLE "t" ("a")"#);
    }

    #[test]
    fn decl_carries_not_null_and_default() {
        let c = ColumnSpec {
            name: "xmp".into(),
            decl_type: String::new(),
            not_null: true,
            default: Some("''".into()),
        };
        assert_eq!(c.decl(), r#""xmp" NOT NULL DEFAULT ''"#);
    }

    #[test]
    fn copy_sql_names_columns_on_both_sides() {
        let s = spec(vec![col("a", ""), col("b", "")], vec!["a"]);
        assert_eq!(
            s.copy_sql("src"),
            r#"INSERT INTO main."t" ("a", "b") SELECT "a", "b" FROM "src"."t""#
        );
    }

    #[test]
    fn quote_ident_escapes_embedded_quotes() {
        assert_eq!(quote_ident(r#"we"ird"#), r#""we""ird""#);
    }
}
