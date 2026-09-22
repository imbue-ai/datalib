//! LinkedIn data-export ("takeout") ingester — dead simple, one table
//! per file.

pub mod photos;
pub mod schema_raw;

use datalib_etl::blob_cas::{cas_path_for, BlobCas};
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_macros::RawStoreHandle;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::control::DownloadControl;
use datalib_etl::doltlite_raw::{self as dr};
use datalib_etl::progress::Progress;
use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::sqlite::SqlitePool;
use tracing::warn;
use uuid::Uuid;

use schema_raw::{canonical_table, known_file, linkedin_ns, ARTICLES_TABLE};

pub use datalib_etl::doltlite_raw::db_path_for;

/// Rows per multi-VALUES INSERT statement. 2 binds/row keeps us well
/// under SQLite's 32k-param ceiling.
const INSERT_CHUNK: usize = 400;

#[derive(Clone, Debug, RawStoreHandle)]
pub struct RawDb {
    pool: SqlitePool,
    /// Connection profile photos. Opened with the handle rather than
    /// from a path at the call site, so there is one opener per store
    /// and `close_all` reaches it.
    ///
    /// `Some` on the download path even when `fetch_photos` is off — a
    /// handle whose store set depends on a config flag is one nobody
    /// can reason about, and an empty CAS file costs nothing. `None`
    /// only on a reader whose store predates any photo fetch, since
    /// opening a missing file read-only is an error and creating it
    /// would be a write render does not own.
    cas: Option<BlobCas>,
    /// The commit a reader is pinned at; `None` for the writer.
    pin: Option<datalib_etl::pin::Pin>,
}

impl RawDb {
    /// Open the raw store. CSV tables are created lazily during
    /// [`fetch`] (their names aren't known until we walk the export), so
    /// we open with just the shared bookkeeping DDL.
    /// Open this store to *read* it, for the render pass.
    ///
    /// The download step owns this store; render only reads it. An ordinary
    /// [`Self::open`] would discard the downloader's in-flight rows,
    /// reconcile the schema and commit on the way in — three writes to a
    /// file this caller does not own. See
    /// `datalib_etl::doltlite_raw::open_reader`.
    ///
    /// No DDL, so a store the current downloader has not touched keeps
    /// whatever columns it has; probe with `column_exists` and fall back
    /// where that matters.
    ///
    /// Pinned at `commit`, else HEAD; `None` when nothing is committed.
    pub async fn open_reader(db_path: &Path, commit: Option<&str>) -> Result<Option<Self>> {
        let Some(reader) = datalib_etl::doltlite_raw::open_reader(db_path, commit).await? else {
            return Ok(None);
        };
        let cas_path = cas_path_for(db_path);
        let cas = if cas_path.is_file() {
            Some(BlobCas::open_reader(&cas_path).await?)
        } else {
            None
        };
        Ok(Some(Self {
            pool: reader.pool().clone(),
            cas,
            pin: Some(reader.pin().clone()),
        }))
    }

    pub async fn open(db_path: &Path) -> Result<Self> {
        let pool = dr::open(db_path, &[]).await?;
        let cas = BlobCas::open(&cas_path_for(db_path)).await?;
        Ok(Self {
            pool,
            cas: Some(cas),
            pin: None,
        })
    }

    /// `None` only on a reader whose store has no CAS file — see the field.
    pub fn cas(&self) -> Option<&BlobCas> {
        self.cas.as_ref()
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// The commit this reader reads at. `None` on the writer's handle.
    pub fn pin(&self) -> Option<&datalib_etl::pin::Pin> {
        self.pin.as_ref()
    }

    /// Release every store this handle opened, and wait for the
    /// connections to go away. Dropping only schedules that.
    pub async fn close(self) {
        self.close_all().await;
    }

    pub async fn load_payloads(
        &self,
        reads: datalib_etl::pin::Reads<'_>,
        table: &str,
    ) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, reads, table).await
    }
}

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: two live connections to
    /// one `.doltlite_db` make each other's `dolt_commit` fail. See
    /// `datalib/backend/etl/README.md`.
    pub db: RawDb,
    /// Root of the user's LinkedIn export (the directory full of CSVs).
    pub input_path: PathBuf,
    /// When set, fetch each connection's profile photo (og:image) into
    /// the per-source CAS + `contact_photos` edge table. Off by default;
    /// once fetched, a connection is never re-fetched (see [`photos`]).
    pub fetch_photos: bool,
    /// Give up the photo sweep after this many *consecutive* transient
    /// fetch failures (LinkedIn hard-blocking). Resolved from the source's
    /// `download_params.maximum_sequential_failed_requests`.
    pub photo_max_consecutive_failures: u64,
    pub progress: Progress,
    pub control: DownloadControl,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct FetchSummary {
    pub files: usize,
    pub rows: usize,
    pub parse_errors: usize,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = opts.db.clone();

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
                warn!(event = "linkedin_csv_failed", file = %path.display(), table, error = %e, "a CSV of the export could not be ingested");
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
                warn!(event = "linkedin_articles_failed", error = %e, "the articles could not be ingested");
                summary.parse_errors += 1;
            }
        }
    }

    tx.commit().await.context("commit linkedin tx")?;

    // Photo fetch runs after the snapshot is committed (it needs the
    // `connections` rows persisted) and is a no-op unless enabled. Each
    // connection is fetched at most once across runs.
    // Through the handle's own CAS, so nothing here opens a second
    // store. `None` is a reader, which never reaches a fetch.
    if let (true, Some(cas)) = (opts.fetch_photos, db.cas()) {
        match photos::fetch_connection_photos(
            &db,
            cas,
            &opts.progress,
            opts.photo_max_consecutive_failures,
        )
        .await
        {
            Ok(s) => tracing::info!(
                event = "linkedin_photos",
                attempted = s.attempted,
                fetched = s.fetched,
                no_photo = s.no_photo,
                transient = s.transient,
                gave_up = s.gave_up,
                "fetched the profile photos"
            ),
            Err(e) => {
                warn!(event = "linkedin_photos_failed", error = %e, "the profile photos could not be fetched")
            }
        }
    }
    Ok(summary)
}

async fn ingest_one(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    path: &Path,
) -> Result<usize> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let body = strip_notes_preamble(&raw);
    let rows = parse_rows(table, &body)?;
    replace_table(tx, table, &rows).await?;
    Ok(rows.len())
}

/// The export mixes two quoting dialects: `Comments_<id>.csv` escapes an
/// embedded quote as `\"`, while `Shares_<id>.csv`, `Positions.csv` and
/// the message feeds double it (`""`). Both are accepted at once — the
/// escape only applies inside a quoted field, and `double_quote` stays on.
/// Without the escape a `\"` closes the field and the rest of the message
/// splits into fragment rows, one of which lands its text in `Date`.
pub(crate) fn csv_reader(body: &str) -> csv::Reader<&[u8]> {
    csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .escape(Some(b'\\'))
        .from_reader(body.as_bytes())
}

fn parse_rows(table: &str, body: &str) -> Result<Vec<(String, String)>> {
    let mut rdr = csv_reader(body);
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
    Ok(rows)
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

async fn replace_table(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    rows: &[(String, String)],
) -> Result<()> {
    let ddl = dr::wire_payload_table_ddl(table, &[]);
    // Audited: `wire_payload_table_ddl` renders DDL from `table`, which is a
    // `&'static str` at every callsite; rows are bound below.
    sqlx::query(sqlx::AssertSqlSafe(ddl))
        .execute(&mut **tx)
        .await
        .with_context(|| format!("create table {table}"))?;
    sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
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
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
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
///   * the joined natural-key columns when hinted and present (a
///     connection's is its profile URL alone — [`schema_raw::connection_key`]);
///   * otherwise a uuidv5 over `table` + the row's canonical JSON.
fn row_id(table: &str, payload: &Value, id_cols: Option<&[&str]>) -> String {
    if let Some(cols) = id_cols {
        let parts: Vec<&str> = cols
            .iter()
            .filter_map(|c| payload.get(*c).and_then(Value::as_str))
            .filter(|s| !s.is_empty())
            .collect();
        if !parts.is_empty() {
            return parts.join("\u{1f}");
        }
    }
    let recipe = format!("{table}\u{0}{payload}");
    Uuid::new_v5(&linkedin_ns(), recipe.as_bytes())
        .as_hyphenated()
        .to_string()
}

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

pub(crate) fn strip_notes_preamble(text: &str) -> String {
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

    /// The export's two quoting dialects, seen in one real export:
    /// Comments escapes a quote as `\"` (also `\""` where the quoted text
    /// ends the field), Shares doubles it. Without the escape the first
    /// row here splits into fragments, and a fragment's text becomes a
    /// `Date`.
    #[test]
    fn parses_backslash_and_doubled_quotes() {
        let body = "Date,Link,Message\n\
            2024-06-10 18:53:49,https://x/feed/a,\"Back on the \\\"Hey Google\\\" team,\n\nkeystrokes \u{1f600}\"\n\
            2024-06-11 00:00:00,https://x/feed/b,\"He said: \\\"make it so.\\\"\"\n\
            2024-06-12 00:00:00,https://x/feed/c,\"They call it \"\"now more than ever\"\"!\"\n";
        let rows = parse_rows("comments", body).expect("parse");
        let payloads: Vec<Value> = rows
            .iter()
            .map(|(_, p)| serde_json::from_str(p).expect("json"))
            .collect();
        assert_eq!(payloads.len(), 3, "one row per record, no fragments");
        assert_eq!(
            payloads[0]["Message"],
            "Back on the \"Hey Google\" team,\n\nkeystrokes \u{1f600}"
        );
        assert_eq!(payloads[1]["Message"], "He said: \"make it so.\"");
        assert_eq!(
            payloads[2]["Message"],
            "They call it \"now more than ever\"!"
        );
        for p in &payloads {
            assert!(
                p["Date"].as_str().unwrap().starts_with("2024-06-1"),
                "every Date is a date: {p}"
            );
        }
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
        // `connections` is keyed by the URL itself, which is what the
        // photo fetch joins on.
        let conn_id = row_id("connections", &v, Some(&["URL"]));
        assert_eq!(conn_id, schema_raw::connection_key("https://x/in/abc"));
        assert_eq!(conn_id, "https://x/in/abc");
        // Empty hinted column → hash fallback (stable, 36-char uuid).
        let empty: Value = serde_json::json!({"URL": ""});
        let id = row_id("connections", &empty, Some(&["URL"]));
        assert_eq!(id.len(), 36);
        // Hash is deterministic.
        assert_eq!(row_id("t", &v, None), row_id("t", &v, None));
    }
}
