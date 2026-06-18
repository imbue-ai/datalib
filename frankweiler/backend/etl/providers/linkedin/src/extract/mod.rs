//! LinkedIn data-export ("takeout") ingester — dead simple, one table
//! per file.
//!
//! We walk the export directory, and for every `*.csv` we find we make a
//! `(id, payload)` raw table named after the file, drop its rows, and
//! re-insert one row per CSV record with the entire record captured as a
//! JSON `payload`. A LinkedIn export is a complete snapshot, so
//! "drop-all-and-reinsert every present file" is the whole incremental
//! story — no cursors, no diffing. The user's published articles
//! (`Articles/**/*.html`) are the one non-CSV feed; they land in the
//! [`schema_raw::ARTICLES_TABLE`] table one row per file.
//!
//! ## What we ingest
//!
//! [`schema_raw::KNOWN_FILES`] enumerates every file a *complete* export
//! can contain. That list is documentation, not a gate: any CSV we find
//! is ingested whether listed or not (an unlisted one earns a WARN so we
//! notice new export shapes), and every listed file is optional — a
//! missing file simply yields no table. So running on a partial export
//! (deleted, never-exported, or privacy-excluded files) is always safe.
//!
//! ## Identity
//!
//! Most LinkedIn CSVs carry no per-row id, so the default PK is a
//! uuidv5 over the table name + the row's contents: stable across
//! re-exports and self-deduping. [`schema_raw::KNOWN_FILES`] names the
//! natural-key column(s) for the handful of files that have one (e.g.
//! Connections' profile `URL`); when those columns are all empty for a
//! row we fall back to the row hash. Table names come from
//! [`schema_raw::canonical_table`], which strips LinkedIn's per-member
//! numeric filename suffix (`Comments_17529409.csv` → `comments`).
//!
//! ## Quirks handled
//!
//!   * A leading `Notes:` preamble block (Connections.csv) is stripped
//!     before parsing.
//!   * Duplicate header names (Ad_Targeting.csv has `Company Names` ×3)
//!     are disambiguated with a ` (2)`, ` (3)` suffix so no cell is
//!     lost.
//!   * Multi-line quoted fields (Learning.csv course descriptions) parse
//!     correctly because we hand the byte stream to the `csv` crate.

pub mod schema_raw;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::doltlite_raw::{self as dr};
use frankweiler_etl::progress::Progress;
use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::sqlite::SqlitePool;
use tracing::warn;
use uuid::Uuid;

use schema_raw::{canonical_table, known_file, linkedin_ns, ARTICLES_TABLE};

pub use frankweiler_etl::doltlite_raw::db_path_for;

/// Rows per multi-VALUES INSERT statement. 2 binds/row keeps us well
/// under SQLite's 32k-param ceiling.
const INSERT_CHUNK: usize = 400;

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
}

impl RawDb {
    /// Open the raw store. CSV tables are created lazily during
    /// [`fetch`] (their names aren't known until we walk the export), so
    /// we open with just the shared bookkeeping DDL.
    pub async fn open(db_path: &Path) -> Result<Self> {
        let pool = dr::open(db_path, &[]).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Load every row's `payload` JSON from one table (used by the
    /// translate/render side).
    pub async fn load_payloads(&self, table: &str) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, table).await
    }
}

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Doltlite database path. Ignored for opening when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB (the orchestrator opens it so the post-extract
    /// commit hits the same pool).
    pub db: Option<RawDb>,
    /// Root of the user's LinkedIn export (the directory full of CSVs).
    pub input_path: PathBuf,
    pub progress: Progress,
    pub control: ExtractControl,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct FetchSummary {
    pub files: usize,
    pub rows: usize,
    pub parse_errors: usize,
}

/// Run one extract pass: ingest every `*.csv` (plus `Articles/**/*.html`)
/// under `input_path`. Files absent from [`schema_raw::KNOWN_FILES`] are
/// still ingested — they just log a WARN so new export shapes surface.
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&db_path_for(&opts.db_path)).await?,
    };
    // Every run is a full snapshot replace, so `--reset-and-redownload`
    // is implicit: we DELETE+reinsert each present table below.
    let _ = opts.control.reset_and_redownload;

    let mut summary = FetchSummary::default();
    let mut tx = db.pool().begin().await.context("begin linkedin tx")?;

    for path in discover_csvs(&opts.input_path) {
        let table = table_name(&opts.input_path, &path);
        if known_file(&table).is_none() {
            warn!(
                event = "linkedin_unknown_file",
                file = %path.display(),
                table,
                "CSV not in KNOWN_FILES manifest; ingesting generically",
            );
        }
        match ingest_one(&mut tx, &table, &path).await {
            Ok(n) => {
                summary.files += 1;
                summary.rows += n;
                opts.progress
                    .set_message(&format!("{table}: {n} rows ({} files)", summary.files));
            }
            Err(e) => {
                warn!(event = "linkedin_csv_failed", file = %path.display(), table, error = %e);
                summary.parse_errors += 1;
            }
        }
    }

    // Articles are the one non-CSV feed: each `*.html` becomes a row in
    // the shared `articles` table. No-op when the export has none.
    let articles = discover_articles(&opts.input_path);
    if !articles.is_empty() {
        match ingest_articles(&mut tx, &opts.input_path, &articles).await {
            Ok(n) => {
                summary.files += 1;
                summary.rows += n;
                opts.progress.set_message(&format!(
                    "{ARTICLES_TABLE}: {n} rows ({} files)",
                    summary.files
                ));
            }
            Err(e) => {
                warn!(event = "linkedin_articles_failed", error = %e);
                summary.parse_errors += 1;
            }
        }
    }

    tx.commit().await.context("commit linkedin tx")?;
    Ok(summary)
}

/// Parse one CSV file and replace its table's contents.
async fn ingest_one(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    path: &Path,
) -> Result<usize> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let body = strip_notes_preamble(&raw);

    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(body.as_bytes());
    let headers = dedup_headers(rdr.headers().context("read CSV header")?);
    let id_cols = known_file(table)
        .map(|f| f.id_cols)
        .filter(|c| !c.is_empty());

    let mut rows: Vec<(String, String)> = Vec::new();
    for rec in rdr.records() {
        let rec = rec.context("read CSV record")?;
        let mut obj = Map::new();
        for (i, col) in headers.iter().enumerate() {
            let cell = rec.get(i).unwrap_or("").trim();
            obj.insert(col.clone(), Value::String(cell.to_string()));
        }
        let payload = Value::Object(obj);
        let id = row_id(table, &payload, id_cols);
        rows.push((id, payload.to_string()));
    }

    replace_table(tx, table, &rows).await?;
    Ok(rows.len())
}

/// Discover every `*.html` under an `Articles/` directory in the export
/// and ingest each as one row of the shared [`ARTICLES_TABLE`]. The
/// payload is `{ "file": <export-relative path>, "html": <contents> }`;
/// the row id is the relative path (stable, one row per article file).
async fn ingest_articles(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    root: &Path,
    paths: &[PathBuf],
) -> Result<usize> {
    let mut rows: Vec<(String, String)> = Vec::with_capacity(paths.len());
    for path in paths {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let html =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let payload = serde_json::json!({ "file": rel, "html": html });
        rows.push((rel, payload.to_string()));
    }
    replace_table(tx, ARTICLES_TABLE, &rows).await?;
    Ok(rows.len())
}

/// CREATE the `(id, payload)` table if needed, clear it, and bulk-insert
/// `rows` (id, payload-JSON). A LinkedIn export is a full snapshot, so
/// every table is replaced wholesale.
async fn replace_table(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    rows: &[(String, String)],
) -> Result<()> {
    let ddl = dr::wire_payload_table_ddl(table, &[]);
    sqlx::query(&ddl)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("create table {table}"))?;
    sqlx::query(&format!("DELETE FROM {table}"))
        .execute(&mut **tx)
        .await
        .with_context(|| format!("clear table {table}"))?;

    for chunk in rows.chunks(INSERT_CHUNK) {
        let mut sql = format!("INSERT OR REPLACE INTO {table} (id, payload) VALUES ");
        for i in 0..chunk.len() {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str("(?, jsonb(?))");
        }
        let mut q = sqlx::query(&sql);
        for (id, payload) in chunk {
            q = q.bind(id.clone()).bind(payload.clone());
        }
        q.execute(&mut **tx)
            .await
            .with_context(|| format!("insert into {table}"))?;
    }
    Ok(())
}

/// PK for a row:
///   * the joined natural-key columns when hinted and present — as a
///     uuidv5 of `"{table}:{joined}"` for [`schema_raw::is_uuid_keyed`]
///     tables (`connections`, keyed by profile URL), else the raw join;
///   * otherwise a uuidv5 over `table` + the row's canonical JSON.
fn row_id(table: &str, payload: &Value, id_cols: Option<&[&str]>) -> String {
    if let Some(cols) = id_cols {
        let parts: Vec<&str> = cols
            .iter()
            .filter_map(|c| payload.get(*c).and_then(Value::as_str))
            .filter(|s| !s.is_empty())
            .collect();
        if !parts.is_empty() {
            let joined = parts.join("\u{1f}");
            return if schema_raw::is_uuid_keyed(table) {
                schema_raw::ns_id(&format!("{table}:{joined}"))
            } else {
                joined
            };
        }
    }
    let recipe = format!("{table}\u{0}{payload}");
    Uuid::new_v5(&linkedin_ns(), recipe.as_bytes())
        .as_hyphenated()
        .to_string()
}

/// Recursively collect every `*.csv` under `root`, sorted for stable
/// ordering.
fn discover_csvs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("csv")) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Recursively collect every `*.html` under an `Articles/` directory in
/// the export, sorted. Empty when the export has no articles.
fn discover_articles(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut in_articles = vec![false];
    while let Some(dir) = stack.pop() {
        let under = in_articles.pop().unwrap_or(false);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                let name_is_articles = p
                    .file_name()
                    .is_some_and(|n| n.eq_ignore_ascii_case("articles"));
                stack.push(p);
                in_articles.push(under || name_is_articles);
            } else if under
                && p.extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("html"))
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// The raw table name for a CSV's path-relative-to-`root`. Delegates to
/// [`schema_raw::canonical_table`]: lowercase, non-alphanumeric runs
/// collapse to `_`, and the per-member numeric filename suffix is
/// stripped. e.g. `Email Addresses.csv` → `email_addresses`,
/// `Comments_17529409.csv` → `comments`.
fn table_name(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let stem = rel.with_extension("");
    canonical_table(&stem.to_string_lossy())
}

/// Drop a leading `Notes:` preamble (a `Notes:` line, an explanatory
/// paragraph, then a blank line) so the real header is row 1. No-op when
/// the file doesn't start with `Notes:`.
fn strip_notes_preamble(text: &str) -> String {
    let trimmed = text.trim_start_matches('\u{feff}');
    if !trimmed.trim_start().starts_with("Notes:") {
        return trimmed.to_string();
    }
    // Skip everything up to and including the first blank line.
    let mut lines = trimmed.lines();
    for line in lines.by_ref() {
        if line.trim().is_empty() {
            break;
        }
    }
    lines.collect::<Vec<_>>().join("\n")
}

/// Disambiguate duplicate header names with a ` (2)`, ` (3)` … suffix,
/// and give empty headers a positional `column_<i>` name.
fn dedup_headers(headers: &csv::StringRecord) -> Vec<String> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut out = Vec::with_capacity(headers.len());
    for (i, h) in headers.iter().enumerate() {
        let base = if h.trim().is_empty() {
            format!("column_{i}")
        } else {
            h.trim().to_string()
        };
        let count = seen.entry(base.clone()).or_insert(0);
        *count += 1;
        out.push(if *count == 1 {
            base
        } else {
            format!("{base} ({count})")
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn table_names_slugify() {
        let root = Path::new("/x");
        assert_eq!(
            table_name(root, Path::new("/x/Email Addresses.csv")),
            "email_addresses"
        );
        assert_eq!(table_name(root, Path::new("/x/messages.csv")), "messages");
        assert_eq!(
            table_name(root, Path::new("/x/Jobs/Saved.csv")),
            "jobs_saved"
        );
        assert_eq!(
            table_name(root, Path::new("/x/Receipts_v2.csv")),
            "receipts_v2"
        );
        // Per-member numeric suffix is stripped to a canonical name.
        assert_eq!(
            table_name(root, Path::new("/x/Comments_17529409.csv")),
            "comments"
        );
    }

    #[test]
    fn strips_notes_block() {
        let csv = "Notes:\n\"blah blah\"\n\nFirst Name,URL\nA,u\n";
        assert_eq!(strip_notes_preamble(csv), "First Name,URL\nA,u");
        let plain = "A,B\n1,2\n";
        assert_eq!(strip_notes_preamble(plain), plain);
    }

    #[test]
    fn dedups_and_names_headers() {
        let rec = csv::StringRecord::from(vec!["Company Names", "Company Names", "", "X"]);
        assert_eq!(
            dedup_headers(&rec),
            vec!["Company Names", "Company Names (2)", "column_2", "X"]
        );
    }

    #[test]
    fn row_id_prefers_natural_key_then_hashes() {
        let v: Value = serde_json::json!({"URL": "https://x/in/abc", "Name": "A"});
        // A non-uuid-keyed hinted table returns the raw joined key.
        let inv: Value = serde_json::json!({"inviterProfileUrl": "https://x/in/abc"});
        assert_eq!(
            row_id("invitations", &inv, Some(&["inviterProfileUrl"])),
            "https://x/in/abc"
        );
        // `connections` is uuid-keyed: the id is a uuidv5 derived from
        // the URL, and matches schema_raw::connection_uuid.
        let conn_id = row_id("connections", &v, Some(&["URL"]));
        assert_eq!(conn_id.len(), 36);
        assert_eq!(conn_id, schema_raw::connection_uuid("https://x/in/abc"));
        // Empty hinted column → hash fallback (stable, 36-char uuid).
        let empty: Value = serde_json::json!({"URL": ""});
        let id = row_id("connections", &empty, Some(&["URL"]));
        assert_eq!(id.len(), 36);
        // Hash is deterministic.
        assert_eq!(row_id("t", &v, None), row_id("t", &v, None));
    }
}
