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
//! ## Why column DEFAULTs are not mirrored either
//!
//! A `DEFAULT` is a rule for writes, and the mirror is not written to.
//! It only ever applies to a row inserted without a value for that
//! column, and no such insert happens here: [`TableSpec::copy_sql`]
//! names every column on both sides, so every mirrored value comes from
//! the source row it was copied from. A mirrored `DEFAULT` could
//! therefore never fire.
//!
//! Carrying one across is not free, either. `PRAGMA table_xinfo`
//! reports the default as SQL text — a literal like `'unset'`, but
//! equally an expression like `datetime('now')` — out of the `.lrcat`,
//! a SQLite file we did not write. Splicing that back into our own
//! `CREATE TABLE` means either trusting it or parsing arbitrary SQL to
//! decide whether to. That is a real cost, paid to reproduce a rule
//! nothing can trigger, so the defaults are simply dropped — same
//! reasoning as the constraints above.
//!
//! There is deliberately no schema-reconciliation logic here. Every run
//! drops each mirror table and recreates it from the source, so the
//! mirror's schema is never *compared* to anything — it is simply
//! rebuilt. See [`super::mirror`] for why that is free.

use anyhow::{bail, Context, Result};
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
    ///
    /// The name is quoted; the declared type is *checked* instead. A
    /// type is not an identifier, so quoting it would change its meaning
    /// rather than make it safe — `"VARCHAR(255)"` is a column named
    /// that, not a type. And it cannot be spliced in on trust: it
    /// arrives from `PRAGMA table_xinfo` on the attached source catalog,
    /// an arbitrary SQLite file we did not write.
    ///
    /// An unrecognised type fails the step rather than being dropped.
    /// Dropping it would leave the column untyped, which changes its
    /// affinity and so what the mirror stores — a quiet narrowing where
    /// a loud stop is cheap.
    pub fn decl(&self) -> Result<String> {
        let mut s = quote_ident(&self.name);
        if !self.decl_type.is_empty() {
            if !is_plain_type_name(&self.decl_type) {
                bail!(
                    "column {:?} declares type {:?}, which is not a plain SQLite type name; \
                     refusing to splice it into the mirror's CREATE TABLE. If the source \
                     really is shaped like this, drop the column with exclude_columns.",
                    self.name,
                    self.decl_type,
                );
            }
            s.push(' ');
            s.push_str(&self.decl_type);
        }
        if self.not_null {
            s.push_str(" NOT NULL");
        }
        Ok(s)
    }
}

/// Is `ty` a declared type we are willing to splice into DDL verbatim?
///
/// Accepted: letters, digits, underscores and spaces, optionally
/// followed by a parenthesised list of one or two numbers. That covers
/// every type name SQLite itself documents — `INTEGER`, `VARCHAR(255)`,
/// `UNSIGNED BIG INT`, `NUMERIC(10,5)` — and admits nothing that could
/// end the column definition early: no quotes, no commas outside the
/// argument list, no parentheses beyond the one pair, no semicolons.
///
/// What it rejects is a type name SQLite would have accepted only
/// because it was quoted (`CREATE TABLE t(a "my type")`). Those are
/// legal and vanishingly rare, and the caller says how to skip such a
/// column, so refusing them is cheaper than reconstructing the quoting.
fn is_plain_type_name(ty: &str) -> bool {
    let (head, args) = match ty.split_once('(') {
        Some((head, rest)) => (head, Some(rest)),
        None => (ty, None),
    };
    let word_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == ' ';
    if !head.chars().any(|c| c.is_ascii_alphanumeric()) || !head.chars().all(word_char) {
        return false;
    }
    let Some(args) = args else { return true };
    let Some(inner) = args.trim_end().strip_suffix(')') else {
        return false;
    };
    let nums: Vec<&str> = inner.split(',').collect();
    // `VARCHAR()` is not a type, and SQLite takes at most two arguments.
    (1..=2).contains(&nums.len())
        && nums.iter().all(|n| {
            let n = n.trim().strip_prefix(['-', '+']).unwrap_or(n.trim());
            !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())
        })
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
    ///
    /// Fails if any column's declared type is not one
    /// [`ColumnSpec::decl`] will splice into DDL.
    pub fn create_ddl(&self) -> Result<String> {
        let mut cols: Vec<String> = Vec::with_capacity(self.columns.len() + 1);
        for c in &self.columns {
            cols.push(
                c.decl()
                    .with_context(|| format!("build CREATE TABLE for {:?}", self.name))?,
            );
        }
        if !self.pk.is_empty() {
            let key: Vec<String> = self.pk.iter().map(|c| quote_ident(c)).collect();
            cols.push(format!("PRIMARY KEY ({})", key.join(", ")));
        }
        Ok(format!(
            "CREATE TABLE {} ({})",
            quote_ident(&self.name),
            cols.join(", ")
        ))
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
    // Audited: every interpolated identifier goes through `plan::quote_ident`,
    // which double-quotes and escapes embedded quotes (covered by
    // `quote_ident_escapes_embedded_quotes`). Source catalog names are
    // attacker-shaped only in the sense that they come from the user's own
    // Lightroom file, and quoting is what makes that safe.
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
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
            s.create_ddl().unwrap(),
            r#"CREATE TABLE "t" ("id_global", "id_local" INTEGER, PRIMARY KEY ("id_global"))"#
        );
    }

    #[test]
    fn keyless_ddl_has_no_primary_key_clause() {
        let s = spec(vec![col("a", "")], vec![]);
        assert_eq!(s.create_ddl().unwrap(), r#"CREATE TABLE "t" ("a")"#);
    }

    #[test]
    fn decl_carries_not_null() {
        let c = ColumnSpec {
            name: "xmp".into(),
            decl_type: String::new(),
            not_null: true,
        };
        assert_eq!(c.decl().unwrap(), r#""xmp" NOT NULL"#);
    }

    #[test]
    fn a_type_that_is_not_a_plain_type_name_is_refused() {
        // The declared type is spliced into DDL, so a source catalog
        // that carries SQL there must not be able to close the column
        // definition and open something of its own.
        let c = col("id", "INTEGER, x TEXT); DROP TABLE t; --");
        assert!(c.decl().is_err());
        let s = spec(vec![c], vec![]);
        assert!(s.create_ddl().is_err());
    }

    #[test]
    fn real_sqlite_type_names_are_accepted() {
        for ty in [
            "INTEGER",
            "TEXT",
            "VARCHAR(255)",
            "NUMERIC(10,5)",
            "UNSIGNED BIG INT",
            "DOUBLE PRECISION",
            "NATIVE CHARACTER (70)",
        ] {
            assert!(is_plain_type_name(ty), "should accept {ty:?}");
        }
        for ty in [
            "",
            "TEXT'",
            r#""my type""#,
            "TEXT)",
            "VARCHAR(255",
            "VARCHAR()",
            "VARCHAR(255) CHECK (1)",
            "INT(1,2,3)",
            "INT(a)",
        ] {
            assert!(!is_plain_type_name(ty), "should reject {ty:?}");
        }
    }

    #[test]
    fn quote_ident_escapes_embedded_quotes() {
        assert_eq!(quote_ident(r#"we"ird"#), r#""we""ird""#);
    }
}
