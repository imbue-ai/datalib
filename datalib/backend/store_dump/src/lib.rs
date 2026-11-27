//! A store as the text of a golden: its tables read column by column as
//! they are declared, then written one row a line. The live bake and the
//! TNG fixture's index are both dumped with it.

use std::path::Path;

use datalib_sql::{sql, sql_id, Sql};
use serde_json::{Map, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::{Column, Row, TypeInfo, ValueRef};

/// TEXT columns holding JSON, dumped as the value it is.
const JSON_TEXT_COLUMNS: &[&str] = &[
    "payload",
    "config",
    "summary",
    "example_headers",
    "example_envelope_skeleton",
];

/// How much of a table a dump holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keep {
    Rows,
    /// Only how many rows there are.
    Count,
    Nothing,
}

/// Each table of the store at `path` by name, its rows in key order.
pub async fn dump(path: &Path, keep: impl Fn(&str) -> Keep) -> Value {
    let pool = open_readonly(path).await;
    let tables: Vec<String> = sql!(
        "store_dump.list_tables",
        "SELECT name FROM sqlite_master \
         WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
    )
    .query_scalar()
    .fetch_all(&pool)
    .await
    .expect("list tables");
    let mut out = Map::new();
    for t in tables {
        match keep(&t) {
            Keep::Nothing => {}
            Keep::Count => {
                // Audited: `t` is a table name just read out of the store.
                let sql = Sql::dynamic(
                    sql_id!("store_dump.count_table_rows"),
                    format!("SELECT COUNT(*) FROM \"{t}\""),
                );
                let n: i64 = sql
                    .query_scalar()
                    .fetch_one(&pool)
                    .await
                    .unwrap_or_else(|e| panic!("count {t}: {e}"));
                out.insert(t, Value::String(format!("{n} rows")));
            }
            Keep::Rows => {
                let rows = table_rows(&pool, &t).await;
                out.insert(t, Value::Array(rows));
            }
        }
    }
    pool.close().await;
    Value::Object(out)
}

async fn table_rows(pool: &SqlitePool, t: &str) -> Vec<Value> {
    // Audited, here and below: `t` is a table name just read out of the
    // store, and the columns are the ones it declares.
    let info = Sql::dynamic(
        sql_id!("store_dump.read_table_info"),
        format!("PRAGMA table_info(\"{t}\")"),
    )
    .query()
    .fetch_all(pool)
    .await
    .expect("table_info");
    let mut columns: Vec<String> = Vec::new();
    let mut key: Vec<(i64, String)> = Vec::new();
    for r in &info {
        let name: String = r.try_get("name").expect("table_info name");
        // What was split off the content because it churns.
        if name == "volatile_payload" {
            continue;
        }
        let place: i64 = r.try_get("pk").unwrap_or(0);
        if place > 0 {
            key.push((place, name.clone()));
        }
        columns.push(name);
    }
    key.sort();
    let select: Vec<String> = columns
        .iter()
        .map(|c| match c.as_str() {
            "payload" => "json(payload) AS payload".to_string(),
            c => format!("\"{c}\""),
        })
        .collect();
    // A table's key is its identity, so a row stays in place from one dump
    // to the next and a change to it reads as a change.
    let by: Vec<String> = match key.is_empty() {
        false => key.iter().map(|(_, c)| format!("\"{c}\"")).collect(),
        true => ["id", "run_id", "scope", "endpoint"]
            .iter()
            .find(|c| columns.iter().any(|have| have == *c))
            .map(|c| c.to_string())
            .into_iter()
            .collect(),
    };
    let order = match by.is_empty() {
        true => String::new(),
        false => format!("ORDER BY {}", by.join(", ")),
    };
    let sql = Sql::dynamic(
        sql_id!("store_dump.list_table_rows"),
        format!("SELECT {} FROM \"{t}\" {order}", select.join(", ")),
    );
    let rows = sql
        .query()
        .fetch_all(pool)
        .await
        .unwrap_or_else(|e| panic!("select {t}: {e}"));
    let mut out: Vec<Value> = rows.iter().map(row_value).collect();
    // No key to order by (lightroom's `Ag*` tables, mirrored from Adobe's
    // catalog, declare none): by the rendered row.
    if by.is_empty() {
        out.sort_by_cached_key(Value::to_string);
    }
    out
}

fn row_value(row: &SqliteRow) -> Value {
    let cells = row.columns().iter().map(|col| {
        let name = col.name();
        let is_json = JSON_TEXT_COLUMNS.contains(&name);
        (name.to_string(), cell_value(row, name, is_json))
    });
    Value::Object(cells.collect())
}

pub async fn open_readonly(path: &Path) -> SqlitePool {
    // A plain SQLite file has to say so to the doltlite-linked engine,
    // which would otherwise take it for a doltlite store.
    let opts = if path.extension().is_some_and(|e| e == "sqlite") {
        SqliteConnectOptions::new().filename(datalib_runtime::plain_sqlite::uri(path))
    } else {
        datalib_sqlite_open::file(path)
    }
    .create_if_missing(false)
    .read_only(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()))
}

/// One cell as JSON, typed by the cell's own storage class. The
/// *column*'s declared type cannot be trusted: an aliased function
/// result (`json(payload) AS payload`) has none and would fall through
/// to NULL. `parse_json` turns a JSON-bearing TEXT into a Value.
pub fn cell_value(row: &SqliteRow, name: &str, parse_json: bool) -> Value {
    let raw = row.try_get_raw(name).expect("try_get_raw");
    if raw.is_null() {
        return Value::Null;
    }
    let type_info = raw.type_info().into_owned();
    match type_info.name() {
        "TEXT" => {
            let s: String = row.try_get(name).expect("TEXT decodes as String");
            if parse_json {
                serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s))
            } else {
                Value::String(s)
            }
        }
        "INTEGER" => Value::from(row.try_get::<i64, _>(name).expect("INTEGER decodes as i64")),
        "REAL" => serde_json::Number::from_f64(row.try_get(name).expect("REAL decodes as f64"))
            .map_or(Value::Null, Value::Number),
        "BLOB" => {
            let b: Vec<u8> = row.try_get(name).expect("BLOB decodes as bytes");
            Value::String(format!("<bytes {}>", b.len()))
        }
        _ => Value::Null,
    }
}

/// A row whose one-line form is longer than this is indented instead: a
/// whole conversation in one row is a line of megabytes, which no diff
/// viewer will render.
const SPILL_WIDTH: usize = 64 * 1024;

/// A store dump as text: a `# <table>: N rows` header, then one row a line.
///
/// Not JSON indented one line per column, which makes the line count the
/// product of rows and columns: four columns arriving on `grid_rows` once
/// added 13,044 lines to one source's snapshot. A row a line also makes a
/// changed row read as one changed line.
///
/// Read a changed row with `git diff --word-diff`, which is why the
/// separators carry a space: a compact object is a single word to git's
/// tokenizer, so a one-field change would read as a whole-line rewrite.
pub fn store_as_jsonl(dump: &Value) -> String {
    let Value::Object(tables) = dump else {
        return format!("{dump}\n");
    };
    let mut out = String::new();
    for (table, value) in tables {
        match value {
            Value::Array(rows) => {
                out.push_str(&format!("# {table}: {} rows\n", rows.len()));
                for row in rows {
                    let line = spaced_json(row);
                    if line.len() <= SPILL_WIDTH {
                        out.push_str(&line);
                    } else {
                        out.push_str(&serde_json::to_string_pretty(row).expect("pretty row"));
                    }
                    out.push('\n');
                }
            }
            // A table its caller keeps to a count, already `"<n> rows"`.
            other => {
                let said = other
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| other.to_string());
                out.push_str(&format!("# {table}: {said}\n"));
            }
        }
    }
    out
}

/// Compact JSON with a space after every `,` and `:` — see
/// [`store_as_jsonl`] on why the spaces are load-bearing.
fn spaced_json(v: &Value) -> String {
    use serde::Serialize;
    struct Spaced;
    impl serde_json::ser::Formatter for Spaced {
        fn begin_array_value<W: ?Sized + std::io::Write>(
            &mut self,
            w: &mut W,
            first: bool,
        ) -> std::io::Result<()> {
            if first {
                Ok(())
            } else {
                w.write_all(b", ")
            }
        }
        fn begin_object_key<W: ?Sized + std::io::Write>(
            &mut self,
            w: &mut W,
            first: bool,
        ) -> std::io::Result<()> {
            if first {
                Ok(())
            } else {
                w.write_all(b", ")
            }
        }
        fn begin_object_value<W: ?Sized + std::io::Write>(
            &mut self,
            w: &mut W,
        ) -> std::io::Result<()> {
            w.write_all(b": ")
        }
    }
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, Spaced);
    v.serialize(&mut ser).expect("serialize row");
    String::from_utf8(buf).expect("row is utf-8")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table's rows go one a line, with a header naming the count, and
    /// the separators carry a space so `git diff --word-diff` can tell a
    /// one-field change from a rewritten row.
    #[test]
    fn a_table_dumps_one_row_a_line_with_spaced_separators() {
        let dump = serde_json::json!({
            "grid_rows": [
                {"uuid": "a", "h3": null, "kind": "Photo"},
                {"uuid": "b", "h3": 644, "kind": "Photo"},
            ],
            "tree": "53988 rows",
        });
        assert_eq!(
            store_as_jsonl(&dump),
            "# grid_rows: 2 rows\n\
             {\"h3\": null, \"kind\": \"Photo\", \"uuid\": \"a\"}\n\
             {\"h3\": 644, \"kind\": \"Photo\", \"uuid\": \"b\"}\n\
             # tree: 53988 rows\n"
        );
    }

    /// A row too wide for one line is indented instead, so a store holding
    /// whole conversations cannot put a multi-megabyte line in the golden.
    #[test]
    fn a_row_wider_than_the_spill_width_is_indented() {
        let wide = "x".repeat(SPILL_WIDTH + 1);
        let dump = serde_json::json!({"conversations": [{"payload": wide}]});
        let out = store_as_jsonl(&dump);
        assert!(
            out.starts_with("# conversations: 1 rows\n{\n  \"payload\": \""),
            "{}",
            &out[..40]
        );
        assert!(
            out.lines().all(|l| l.len() <= SPILL_WIDTH + 16),
            "a spilled row still produced a line of {}",
            out.lines().map(str::len).max().unwrap_or(0)
        );
    }

    /// An empty table still says so: a table that lost its rows has to read
    /// as a change, not as an absence.
    #[test]
    fn an_empty_table_keeps_its_header() {
        let dump = serde_json::json!({"edges": []});
        assert_eq!(store_as_jsonl(&dump), "# edges: 0 rows\n");
    }
}
