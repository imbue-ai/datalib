//! The `gpx` raw store: one file's rows written as the fewest row
//! changes, a file's rows deleted, points no file names any more swept,
//! and a file read back out.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use futures::TryStreamExt;
use sqlx::sqlite::{SqliteArguments, SqliteRow};
use sqlx::{Row as _, Sqlite, SqliteConnection};

use super::model::{self, Fidelity, GpxFile};
use super::rows::{self, FileRows, Row, Value};
use super::schema_raw::{full_ddl, Table, Ty, FILES, MEMBERSHIP, PER_FILE};

pub fn db_path_for(raw_dir: &Path) -> PathBuf {
    datalib_etl::raw_layout::entities_db(raw_dir)
}

datalib_etl::raw_db!(pub RawDb: EntityStore, full_ddl());

/// Point ids a run's writes stopped naming, per shared point table.
pub type Dropped = HashMap<&'static str, HashSet<String>>;

#[derive(Debug, Default, Clone, Copy)]
pub struct Written {
    /// Point rows this write added, across the three point tables.
    pub points_added: u64,
}

pub async fn write_file(
    conn: &mut SqliteConnection,
    new: &FileRows,
    dropped: &mut Dropped,
) -> Result<Written> {
    let mut written = Written::default();
    for (table, rows) in &new.points {
        written.points_added += insert_new(conn, table, rows).await?;
    }
    let (Value::Text(path), Value::Text(key)) = (&new.file[0], &new.file[1]) else {
        anyhow::bail!("gpx_files.path and file_key must be text");
    };
    // Diffed against what the key holds, whichever path it was under: a
    // renamed file's rows are compared with its old self's.
    let old = load_key_rows(conn, key).await?;
    for (table, rows) in &new.per_file {
        let before = old.get(table.name).map(Vec::as_slice).unwrap_or(&[]);
        let d = rows::diff(table, before, rows);
        delete_keys(conn, table, &d.deletes).await?;
        upsert(conn, table, &d.upserts).await?;
        note_dropped(dropped, table, rows::dropped_ids(before, rows));
    }
    sqlx::query("DELETE FROM gpx_files WHERE file_key = ? AND path <> ?")
        .bind(key)
        .bind(path)
        .execute(&mut *conn)
        .await
        .context("drop the path a renamed file left")?;
    upsert(conn, &FILES, std::slice::from_ref(&new.file)).await?;
    Ok(written)
}

/// Every stored path and the key its rows are under.
pub async fn stored_keys(pool: &sqlx::SqlitePool) -> Result<HashMap<String, String>> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT path, file_key FROM gpx_files")
        .fetch_all(pool)
        .await
        .context("read gpx_files keys")?;
    Ok(rows.into_iter().collect())
}

/// The point ids a stored file's member rows name, of all three kinds.
pub async fn point_ids(pool: &sqlx::SqlitePool, file_key: &str) -> Result<HashSet<String>> {
    let mut ids = HashSet::new();
    for (_, members, column) in MEMBERSHIP {
        // Audited: table and column are `&'static str`s from `schema_raw`;
        // the key is bound.
        let sql = format!("SELECT {column} FROM {} WHERE file_key = ?", members.name);
        let found: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(file_key)
            .fetch_all(pool)
            .await
            .with_context(|| format!("read {}", members.name))?;
        ids.extend(found);
    }
    Ok(ids)
}

/// Delete everything a path's file was, but the points: those may be
/// another file's too, so they are left for [`sweep`].
pub async fn delete_file(
    conn: &mut SqliteConnection,
    path: &str,
    dropped: &mut Dropped,
) -> Result<bool> {
    let Some((file, per_file)) = load_per_file(conn, path).await? else {
        return Ok(false);
    };
    for table in PER_FILE {
        let before = per_file.get(table.name).map(Vec::as_slice).unwrap_or(&[]);
        note_dropped(dropped, table, rows::dropped_ids(before, &[]));
        // Audited: the table name is a `&'static str` from `schema_raw`;
        // the key is bound.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {} WHERE file_key = ?",
            table.name
        )))
        .bind(file_key_of(&file))
        .execute(&mut *conn)
        .await
        .with_context(|| format!("delete {path} from {}", table.name))?;
    }
    sqlx::query("DELETE FROM gpx_files WHERE path = ?")
        .bind(path)
        .execute(&mut *conn)
        .await
        .with_context(|| format!("delete gpx_files {path}"))?;
    Ok(true)
}

/// Delete each dropped point no member row names any more. One pass over
/// each member table that has candidates, rather than a lookup per id:
/// the member tables have no index on the point id, and an index would
/// be as large as the table.
pub async fn sweep(conn: &mut SqliteConnection, dropped: &Dropped) -> Result<u64> {
    let mut removed = 0;
    for (points, members, column) in MEMBERSHIP {
        let Some(candidates) = dropped.get(points.name).filter(|c| !c.is_empty()) else {
            continue;
        };
        let mut orphans = candidates.clone();
        {
            // Audited: table and column are `&'static str`s from `schema_raw`.
            let sql = format!("SELECT {column} FROM {}", members.name);
            let mut stream =
                sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql)).fetch(&mut *conn);
            while let Some(id) = stream.try_next().await.context("scan member ids")? {
                orphans.remove(&id);
                if orphans.is_empty() {
                    break;
                }
            }
        }
        let mut orphans: Vec<String> = orphans.into_iter().collect();
        orphans.sort();
        for chunk in orphans.chunks(datalib_etl::bulk::SQL_CHUNK) {
            let mut sql = format!("DELETE FROM {} WHERE id IN (", points.name);
            datalib_etl::bulk::push_placeholder_list(&mut sql, chunk.len());
            sql.push(')');
            // Audited: the table name is a `&'static str`; every id is bound.
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                q = q.bind(id);
            }
            removed += q
                .execute(&mut *conn)
                .await
                .with_context(|| format!("sweep {}", points.name))?
                .rows_affected();
        }
    }
    Ok(removed)
}

pub async fn rebuild(conn: &mut SqliteConnection, path: &str) -> Result<Option<String>> {
    let Some(file) = read_file(conn, path).await? else {
        return Ok(None);
    };
    model::rebuild(&file).map(Some)
}

pub async fn read_file(conn: &mut SqliteConnection, path: &str) -> Result<Option<GpxFile>> {
    let Some((file, per_file)) = load_per_file(conn, path).await? else {
        return Ok(None);
    };
    let mut points: HashMap<String, Row> = HashMap::new();
    for (pts, members, _) in MEMBERSHIP {
        let Some(rows) = per_file.get(members.name) else {
            continue;
        };
        let ids: Vec<&Value> = rows.iter().filter_map(|r| r.last()).collect();
        for chunk in ids.chunks(datalib_etl::bulk::SQL_CHUNK) {
            let mut sql = format!("SELECT {} FROM {} WHERE id IN (", columns(pts), pts.name);
            datalib_etl::bulk::push_placeholder_list(&mut sql, chunk.len());
            sql.push(')');
            // Audited: names are `&'static str`s from `schema_raw`; ids are bound.
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
            for v in chunk {
                q = bind(q, v);
            }
            for r in q.fetch_all(&mut *conn).await.context("read points")? {
                let row = decode(pts, &r)?;
                if let Value::Text(id) = &row[0] {
                    points.insert(id.clone(), row);
                }
            }
        }
    }
    let per_file_ref: HashMap<&str, Vec<Row>> = per_file.into_iter().collect();
    rows::from_rows(&file, &per_file_ref, &points).map(Some)
}

pub async fn lossy_paths(pool: &sqlx::SqlitePool) -> Result<Vec<String>> {
    sqlx::query_scalar("SELECT path FROM gpx_files WHERE fidelity = ? ORDER BY path")
        .bind(Fidelity::Lossy.as_str())
        .fetch_all(pool)
        .await
        .context("read lossy gpx_files")
}

async fn load_per_file(
    conn: &mut SqliteConnection,
    path: &str,
) -> Result<Option<(Row, HashMap<&'static str, Vec<Row>>)>> {
    // Audited: the column list is built from `schema_raw`'s names.
    let sql = format!("SELECT {} FROM gpx_files WHERE path = ?", columns(&FILES));
    let Some(r) = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(path)
        .fetch_optional(&mut *conn)
        .await
        .context("read gpx_files")?
    else {
        return Ok(None);
    };
    let file = decode(&FILES, &r)?;
    let per_file = load_key_rows(conn, &file_key_of(&file)).await?;
    Ok(Some((file, per_file)))
}

async fn load_key_rows(
    conn: &mut SqliteConnection,
    key: &str,
) -> Result<HashMap<&'static str, Vec<Row>>> {
    let mut per_file = HashMap::new();
    for table in PER_FILE {
        let order: Vec<&str> = table.columns[..table.key].iter().map(|(c, _)| *c).collect();
        let sql = format!(
            "SELECT {} FROM {} WHERE file_key = ? ORDER BY {}",
            columns(table),
            table.name,
            order.join(", ")
        );
        // Audited: every name is a `&'static str` from `schema_raw`; the key is bound.
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(key)
            .fetch_all(&mut *conn)
            .await
            .with_context(|| format!("read {}", table.name))?;
        per_file.insert(
            table.name,
            rows.iter()
                .map(|r| decode(table, r))
                .collect::<Result<_>>()?,
        );
    }
    Ok(per_file)
}

/// Insert the rows whose key is not there yet; a point already stored is
/// the same point, since its key is its content.
async fn insert_new(conn: &mut SqliteConnection, table: &Table, rows: &[Row]) -> Result<u64> {
    let mut added = 0;
    for chunk in rows.chunks(datalib_etl::bulk::SQL_CHUNK) {
        let mut sql = format!("INSERT INTO {} ({}) VALUES ", table.name, columns(table));
        datalib_etl::bulk::push_placeholders(&mut sql, chunk.len(), table.columns.len());
        sql.push_str(" ON CONFLICT DO NOTHING");
        added += execute(conn, sql, chunk)
            .await
            .with_context(|| format!("insert {}", table.name))?;
    }
    Ok(added)
}

async fn upsert(conn: &mut SqliteConnection, table: &Table, rows: &[Row]) -> Result<()> {
    let key: Vec<&str> = table.columns[..table.key].iter().map(|(c, _)| *c).collect();
    let set: Vec<String> = table.columns[table.key..]
        .iter()
        .map(|(c, _)| format!("{c} = excluded.{c}"))
        .collect();
    for chunk in rows.chunks(datalib_etl::bulk::SQL_CHUNK) {
        let mut sql = format!("INSERT INTO {} ({}) VALUES ", table.name, columns(table));
        datalib_etl::bulk::push_placeholders(&mut sql, chunk.len(), table.columns.len());
        sql.push_str(&format!(
            " ON CONFLICT({}) DO UPDATE SET {}",
            key.join(", "),
            set.join(", ")
        ));
        execute(conn, sql, chunk)
            .await
            .with_context(|| format!("upsert {}", table.name))?;
    }
    Ok(())
}

async fn delete_keys(conn: &mut SqliteConnection, table: &Table, keys: &[Row]) -> Result<()> {
    let clause: Vec<String> = table.columns[..table.key]
        .iter()
        .map(|(c, _)| format!("{c} = ?"))
        .collect();
    let sql = format!("DELETE FROM {} WHERE {}", table.name, clause.join(" AND "));
    for key in keys {
        // Audited: names are `&'static str`s from `schema_raw`; the key is bound.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.clone()));
        for v in key {
            q = bind(q, v);
        }
        q.execute(&mut *conn)
            .await
            .with_context(|| format!("delete from {}", table.name))?;
    }
    Ok(())
}

/// Audited for every caller: `sql` names only `schema_raw`'s tables and
/// columns, and every value in `rows` is bound.
async fn execute(conn: &mut SqliteConnection, sql: String, rows: &[Row]) -> Result<u64> {
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for row in rows {
        for v in row {
            q = bind(q, v);
        }
    }
    Ok(q.execute(&mut *conn).await?.rows_affected())
}

fn bind<'q>(
    q: sqlx::query::Query<'q, Sqlite, SqliteArguments>,
    v: &'q Value,
) -> sqlx::query::Query<'q, Sqlite, SqliteArguments> {
    match v {
        Value::Null => q.bind(None::<String>),
        Value::Int(i) => q.bind(*i),
        Value::Text(s) => q.bind(s.as_str()),
    }
}

fn decode(table: &Table, r: &SqliteRow) -> Result<Row> {
    table
        .columns
        .iter()
        .enumerate()
        .map(|(i, (c, ty))| {
            Ok(match ty {
                Ty::Int => r
                    .try_get::<Option<i64>, _>(i)
                    .with_context(|| format!("{}.{c}", table.name))?
                    .map_or(Value::Null, Value::Int),
                Ty::Text => r
                    .try_get::<Option<String>, _>(i)
                    .with_context(|| format!("{}.{c}", table.name))?
                    .map_or(Value::Null, Value::Text),
            })
        })
        .collect()
}

fn columns(table: &Table) -> String {
    table
        .columns
        .iter()
        .map(|(c, _)| *c)
        .collect::<Vec<_>>()
        .join(", ")
}

fn file_key_of(file: &Row) -> String {
    match &file[1] {
        Value::Text(k) => k.clone(),
        _ => String::new(),
    }
}

fn note_dropped(dropped: &mut Dropped, member_table: &Table, ids: Vec<String>) {
    if ids.is_empty() {
        return;
    }
    if let Some((points, _, _)) = MEMBERSHIP
        .iter()
        .find(|(_, m, _)| m.name == member_table.name)
    {
        dropped.entry(points.name).or_default().extend(ids);
    }
}
