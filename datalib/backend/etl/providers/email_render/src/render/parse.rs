//! Parse the email raw store, driven by **`dolt_diff_<table>`**.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::{self, BlobBundle, CasEdgeRow};
use datalib_etl::doltlite_raw::Reader;
use datalib_etl_render::inputs::{Inputs, RawRange};
use datalib_etl_render::processor::Unparsed;
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl_email::ingest::db::{db_path_for, EmailJoins, LoadedEmail};
use datalib_etl_email::ingest::schema_raw::EmlBlobRow;

/// SQL projection from the `email_blobs` edge's `blake3` to `.eml`
/// bytes. Consumed by [`BlobBundle::load_many`]. After the eml-as-canonical
/// port we only load `.eml`s — attachment parts are mail-parsed out of
/// the loaded `.eml` bytes and added to the same per-bucket
/// `BlobBundle` under synthesized content-hash ref ids. `DISTINCT`
/// because several emails can edge to the same `.eml` blob.
const EML_PROJECTION_SQL: &str = "
    SELECT DISTINCT blob_id AS ref_id, blake3,
           'message/rfc822' AS content_type,
           NULL AS upstream_name
      FROM email_blobs
     WHERE blob_id IN ({placeholders}) AND blake3 IS NOT NULL";

/// Result of the dolt_diff scan. Travels alongside the parsed bag so
/// render can advance the cursor + log timing without a second
/// round-trip.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// `Some(set)` → load only threads whose `(account_id, thread_id)`
    /// is in `set`: the ones the driver found stale through their
    /// declared inputs, plus the ones the diff named. `None` → cold
    /// start, render every thread.
    pub render: Option<HashSet<(String, String)>>,
    /// Bucket keys the driver found stale that no email still belongs
    /// to — declared with nothing so their documents go.
    pub gone: Vec<String>,
    /// The HEAD commit hash at scan time. `None` if `dolt_log()` was
    /// unavailable (non-doltlite sqlite); cursor stays unwritten.
    pub new_head: Option<String>,
    /// Wall-clock time spent in the union query. `None` on cold
    /// start (no diff was issued).
    pub scan_elapsed: Option<Duration>,
}

/// How many threads' `.eml`s are read and mail-parsed together. A full
/// render holds this many threads' bytes at once, not the mailbox's.
pub const EML_BATCH: usize = 200;

#[derive(Default)]
pub struct ParsedEmail {
    /// `(account id, payload)`. The id rides beside the payload rather
    /// than inside it: a JMAP `Account` object has no `id` field — the
    /// id is the key it sits under in the session's `accounts` map.
    pub accounts: Vec<(String, Value)>,
    pub mailboxes: Vec<Value>,
    pub threads: Vec<Value>,
    /// One bucket per `(account_id, thread_id)` whose thread changed
    /// since the last render cursor. Threads whose dolt_diff entries
    /// were empty are entirely absent.
    pub docs: Vec<EmailThreadBucket>,
    /// Count of threads `dolt_diff` reported as unchanged, reported
    /// into the render summary.
    pub docs_skipped: usize,
    pub scan: ScanResult,
    /// Rows whose stored payload would not read as JSON, dropped —
    /// reported by the processor rather than skipped in silence.
    pub unparsed: Vec<Unparsed>,
    /// Where `docs`' `.eml`s are read from as they render. `None` when
    /// the buckets already carry their bytes, as a test's do.
    pub emls: Option<EmlStore>,
}

impl ParsedEmail {
    pub fn close(&mut self) {
        if let Some(emls) = self.emls.take() {
            emls.close();
        }
    }
}

/// The raw store and its blob CAS, held open through the render so each
/// thread's `.eml`s are read just before it renders, a batch at a time.
pub struct EmlStore {
    reader: Reader,
    cas: Option<SqlitePool>,
}

impl EmlStore {
    /// Copies of `batch` with their `.eml`s and attachment parts loaded.
    pub fn load(&self, batch: &[&EmailThreadBucket]) -> Result<Vec<EmailThreadBucket>> {
        block_on(self.load_async(batch))
    }

    async fn load_async(&self, batch: &[&EmailThreadBucket]) -> Result<Vec<EmailThreadBucket>> {
        let mut docs: Vec<EmailThreadBucket> = batch.iter().map(|b| (*b).clone()).collect();
        let refs = docs
            .iter()
            .enumerate()
            .map(|(i, bucket)| (i, bucket.emails.iter().map(|em| em.blob_id.as_str())));
        let mut blobs = BlobBundle::load_many(
            self.reader.pool(),
            self.cas.as_ref(),
            EML_PROJECTION_SQL,
            refs,
        )
        .await?;
        for (i, bucket) in docs.iter_mut().enumerate() {
            if let Some(b) = blobs.remove(&i) {
                bucket.blobs = b;
                extract_attachments_from_emls(bucket);
            }
        }
        Ok(docs)
    }

    pub fn close(self) {
        block_on(async {
            self.reader.close().await;
            if let Some(cas) = self.cas {
                cas.close().await;
            }
        })
    }
}

fn block_on<F: Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

/// One rendered-markdown bucket: every email in a single JMAP Thread
/// plus its joins. Its [`BlobBundle`] (the `.eml`s and each attachment
/// part) is empty until [`EmlStore::load`] fills a copy.
#[derive(Debug, Clone, Default)]
pub struct EmailThreadBucket {
    pub account_id: String,
    pub thread_id: String,
    pub emails: Vec<LoadedEmail>,
    pub joins: EmailJoins,
    pub blobs: BlobBundle,
    /// Every raw row this thread reads — its emails, their mailbox,
    /// keyword and `.eml` edges from the load; its account and the
    /// mailboxes it is filed under as render looks them up.
    pub inputs: Inputs,
}

pub fn parse_export(input: &Path, source_id: &str) -> Result<ParsedEmail> {
    parse(input, source_id, RawRange::cold(), false)
}

/// `label_filter` says whether the render step filters threads by
/// mailbox label: then which threads render depends on the whole
/// mailbox tree, and a mailbox change renders everything.
pub fn parse(
    input: &Path,
    source_id: &str,
    range: RawRange<'_>,
    label_filter: bool,
) -> Result<ParsedEmail> {
    let db_path = db_path_for(input);
    if !db_path.is_file() {
        return Ok(ParsedEmail::default());
    }
    block_on(parse_async(&db_path, source_id, range, label_filter))
}

async fn parse_async(
    db_path: &Path,
    source_id: &str,
    range: RawRange<'_>,
    label_filter: bool,
) -> Result<ParsedEmail> {
    // Opened at the driver's commit, else HEAD. No commit means nothing
    // has been committed here to render: emptiness, not a reason to read
    // the working set.
    let Some(reader) = datalib_etl::doltlite_raw::open_reader(db_path, range.pin)
        .await
        .with_context(|| format!("open raw doltlite for render at {}", db_path.display()))?
    else {
        return Ok(ParsedEmail::default());
    };
    let pool = reader.pool().clone();
    let pin = reader.pin().clone();

    let mut unparsed: Vec<Unparsed> = Vec::new();
    let accounts = load_accounts(&pool, &mut unparsed).await?;
    let mailboxes = load_payloads(&pool, "mailboxes", &mut unparsed).await?;
    let threads = load_payloads(&pool, "threads", &mut unparsed).await?;
    // Every thread with at least one email — the load set on a cold
    // start, the denominator of the skipped count otherwise, and the
    // map from the driver's bucket keys back to thread keys.
    let all = load_all_thread_keys(&pool).await?;

    // ── Phase 1: which threads changed since the cursor? ──────────
    let scan = scan_diff(&pool, source_id, range, &pin, label_filter, &all).await?;

    let (to_load, docs_skipped) = match &scan.render {
        None => (all, 0usize),
        Some(changed) => {
            let skipped = all.difference(changed).count();
            let load: HashSet<(String, String)> = all.intersection(changed).cloned().collect();
            (load, skipped)
        }
    };

    // ── Phase 2: targeted load for to-render buckets ──────────────
    // Rows only: the `.eml` bytes wait for the render, which reads them
    // a batch at a time through the `EmlStore`.
    let docs = if to_load.is_empty() {
        Vec::new()
    } else {
        load_buckets(&pool, &to_load).await?
    };
    let cas = blob_cas::open_cas_for_render(db_path)
        .await
        .with_context(|| format!("open the blob store beside {}", db_path.display()))?;

    Ok(ParsedEmail {
        accounts,
        mailboxes,
        threads,
        docs,
        docs_skipped,
        scan,
        unparsed,
        emls: Some(EmlStore { reader, cas }),
    })
}

/// Mail-parse each `.eml` already loaded into the bucket's
/// `BlobBundle` and pull out attachment-style parts. The bytes ARE
/// already in the `.eml` envelope — we just walk the MIME tree to
/// surface them as separate "blobs" for render's existing
/// attachment-link machinery. Each part is added to the bundle
/// under its content hash so render's `bucket.blobs.get(&blob_id)`
/// resolves it identically to a JMAP-supplied attachment ref id.
fn extract_attachments_from_emls(bucket: &mut EmailThreadBucket) {
    use mail_parser::{MessageParser, MimeHeaders, PartType};

    // Gathered, then added: the bundle is borrowed for the `.eml`s meanwhile.
    let mut parts = BlobBundle::new();
    for em in &bucket.emails {
        let Some(eml_blob) = bucket.blobs.get(&em.blob_id) else {
            continue;
        };
        let Some(msg) = MessageParser::default().parse(&eml_blob.bytes) else {
            continue;
        };
        // Walk both `msg.attachments` and `msg.html_body` (the latter
        // catches inline images that live inside `multipart/related`
        // and only show up via the html-body index). Filter out the
        // alternate text/html body parts mail-parser sometimes
        // surfaces in `attachments`.
        let body_idx: HashSet<usize> = msg
            .text_body
            .iter()
            .copied()
            .chain(msg.html_body.iter().copied())
            .collect();
        let mut seen_idx: HashSet<usize> = HashSet::new();
        let mut atts: Vec<datalib_etl_email::ingest::db::LoadedAttachment> = Vec::new();
        let candidate_idxs: Vec<usize> = msg
            .attachments
            .iter()
            .copied()
            .chain(msg.html_body.iter().copied())
            .collect();
        for idx in candidate_idxs {
            if !seen_idx.insert(idx) {
                continue;
            }
            let Some(part) = msg.part(idx) else { continue };
            if body_idx.contains(&idx) && part.content_id().is_none() {
                continue;
            }
            if matches!(part.body, PartType::Text(_) | PartType::Html(_))
                && part.content_id().is_none()
                && part.attachment_name().is_none()
            {
                continue;
            }
            let bytes: Vec<u8> = match &part.body {
                PartType::Binary(b) | PartType::InlineBinary(b) => b.to_vec(),
                PartType::Text(t) | PartType::Html(t) => t.as_bytes().to_vec(),
                _ => continue,
            };
            let name = part.attachment_name().map(str::to_string);
            let cid = part.content_id().map(str::to_string);
            let disposition = part.content_disposition().map(|cd| cd.ctype().to_string());
            let content_type = part.content_type().map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub),
                None => ct.ctype().to_string(),
            });
            let size = bytes.len() as i64;
            let blob_id = datalib_etl::blob_cas::blake3_hex(&bytes);
            atts.push(datalib_etl_email::ingest::db::LoadedAttachment {
                part_id: format!("p{idx}"),
                blob_id: blob_id.clone(),
                name: name.clone(),
                content_type: content_type.clone(),
                size: Some(size),
                disposition,
                cid,
            });
            parts.add_hashed(blob_id.clone(), blob_id, bytes, content_type, name);
        }
        if !atts.is_empty() {
            bucket.joins.attachments.insert(em.id.clone(), atts);
        }
    }
    bucket.blobs.extend(parts);
}

/// Per-bucket dolt_diff scan. Delegates to the shared
/// [`datalib_etl::doltlite_raw::scan_buckets`] helper, which is
/// the same primitive every other provider uses (slack / chatgpt /
/// claude / signal). Bucket key shape is `"<account_id>|<thread_id>"`
/// so it fits the helper's `HashSet<String>` API; we split it back
/// into a `(String, String)` pair locally for the load step. An
/// account or mailbox change reaches a thread through the inputs it
/// declared — except under a label filter, where the mailbox tree
/// decides which threads render at all, so any mailbox change renders
/// everything.
async fn scan_diff(
    pool: &SqlitePool,
    source_id: &str,
    range: RawRange<'_>,
    pin: &datalib_etl::pin::Pin,
    label_filter: bool,
    all: &HashSet<(String, String)>,
) -> Result<ScanResult> {
    let scan = datalib_etl::doltlite_raw::scan_buckets(
        pool,
        range.cursor,
        pin,
        &datalib_etl::doltlite_raw::DiffScanSpec {
            global_fanout_tables: if label_filter { &["mailboxes"] } else { &[] },
            bucket_query: "
                SELECT DISTINCT account_id || '|' || thread_id AS bucket_key FROM (
                    SELECT to_account_id  AS account_id, to_thread_id  AS thread_id
                      FROM dolt_diff_emails
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT from_account_id, from_thread_id
                      FROM dolt_diff_emails
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT emails.account_id, emails.thread_id
                      FROM dolt_diff_email_mailboxes d
                      JOIN emails ON emails.id = coalesce(d.to_email_id, d.from_email_id)
                     WHERE d.from_ref = ?1 AND d.to_ref = ?2 AND d.diff_type != 'unchanged'
                    UNION
                    SELECT emails.account_id, emails.thread_id
                      FROM dolt_diff_email_keywords d
                      JOIN emails ON emails.id = coalesce(d.to_email_id, d.from_email_id)
                     WHERE d.from_ref = ?1 AND d.to_ref = ?2 AND d.diff_type != 'unchanged'
                    UNION
                    SELECT t.account_id,
                           coalesce(dt.to_id, dt.from_id) AS thread_id
                      FROM dolt_diff_threads dt
                      JOIN threads t ON t.id = coalesce(dt.to_id, dt.from_id)
                     WHERE dt.from_ref = ?1 AND dt.to_ref = ?2 AND dt.diff_type != 'unchanged'
                )
                WHERE account_id IS NOT NULL AND thread_id IS NOT NULL
            ",
        },
    )
    .await?;
    // The driver names buckets by the thread uuid; the load wants the
    // `(account, thread)` pair.
    let by_uuid: HashMap<String, String> = all
        .iter()
        .map(|(a, t)| (super::ids::thread(source_id, a, t).uuid, format!("{a}|{t}")))
        .collect();
    let narrowed = range.narrow_by(scan.render.as_ref(), |key| by_uuid.get(key).cloned());
    let split = |set: HashSet<String>| {
        set.into_iter()
            .filter_map(|key| {
                let (a, t) = key.split_once('|')?;
                Some((a.to_string(), t.to_string()))
            })
            .collect::<HashSet<(String, String)>>()
    };
    Ok(ScanResult {
        render: narrowed.render.map(split),
        gone: narrowed.gone,
        new_head: scan.new_head,
        scan_elapsed: scan.scan_elapsed,
    })
}

async fn load_all_thread_keys(pool: &SqlitePool) -> Result<HashSet<(String, String)>> {
    let rows = sqlx::query("SELECT DISTINCT account_id, thread_id FROM emails")
        .fetch_all(pool)
        .await
        .context("load all (account_id, thread_id) pairs")?;
    let mut out: HashSet<(String, String)> = HashSet::with_capacity(rows.len());
    for r in &rows {
        let a: String = r.try_get("account_id").unwrap_or_default();
        let t: String = r.try_get("thread_id").unwrap_or_default();
        if !a.is_empty() && !t.is_empty() {
            out.insert((a, t));
        }
    }
    Ok(out)
}

async fn load_accounts(
    pool: &SqlitePool,
    unparsed: &mut Vec<Unparsed>,
) -> Result<Vec<(String, Value)>> {
    let table = "accounts";
    let sql = format!("SELECT id, json(payload) AS payload FROM {table} WHERE payload IS NOT NULL");
    // Audited: `table` is a literal.
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .context("load accounts")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let id: String = r.try_get("id").unwrap_or_default();
        let s: String = r.try_get("payload").unwrap_or_default();
        match serde_json::from_str::<Value>(&s) {
            Ok(v) => out.push((id, v)),
            Err(_) => unparsed.push(Unparsed::new("accounts", &id, &s)),
        }
    }
    Ok(out)
}

async fn load_payloads(
    pool: &SqlitePool,
    table: &'static str,
    unparsed: &mut Vec<Unparsed>,
) -> Result<Vec<Value>> {
    let pinned = table;
    let sql =
        format!("SELECT id, json(payload) AS payload FROM {pinned} WHERE payload IS NOT NULL");
    // Audited: `table` is a literal at both callsites.
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .with_context(|| format!("load_payloads {table}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let id: String = r.try_get("id").unwrap_or_default();
        let s: String = r.try_get("payload").unwrap_or_default();
        match serde_json::from_str::<Value>(&s) {
            Ok(v) => out.push(v),
            Err(_) => unparsed.push(Unparsed::new(table, &id, &s)),
        }
    }
    Ok(out)
}

async fn load_buckets(
    pool: &SqlitePool,
    to_load: &HashSet<(String, String)>,
) -> Result<Vec<EmailThreadBucket>> {
    if to_load.is_empty() {
        return Ok(Vec::new());
    }
    let mut bucket_idx: HashMap<(String, String), usize> = HashMap::new();
    let mut docs: Vec<EmailThreadBucket> = Vec::with_capacity(to_load.len());
    let mut wanted_thread_ids: HashSet<String> = HashSet::with_capacity(to_load.len());
    let mut sorted: Vec<&(String, String)> = to_load.iter().collect();
    sorted.sort();
    for (account_id, thread_id) in sorted {
        bucket_idx.insert((account_id.clone(), thread_id.clone()), docs.len());
        wanted_thread_ids.insert(thread_id.clone());
        docs.push(EmailThreadBucket {
            account_id: account_id.clone(),
            thread_id: thread_id.clone(),
            emails: Vec::new(),
            joins: EmailJoins::default(),
            blobs: BlobBundle::default(),
            inputs: Inputs::default(),
        });
    }

    let erows = sqlx::query(
        "SELECT id, account_id, thread_id, blob_id, message_id, in_reply_to, references_header,
                received_at, sent_at, size, subject, from_json, to_json, cc_json, has_attachment
           FROM emails
          WHERE thread_id IN (SELECT value FROM json_each(?))
          ORDER BY thread_id, received_at, id",
    )
    .bind(serde_json::to_string(&wanted_thread_ids)?)
    .fetch_all(pool)
    .await
    .context("phase 2 emails select")?;
    let mut email_ids_in_buckets: HashSet<String> = HashSet::with_capacity(erows.len());
    for r in &erows {
        let id: String = r.try_get("id").unwrap_or_default();
        let account_id: String = r.try_get("account_id").unwrap_or_default();
        let thread_id: String = r.try_get("thread_id").unwrap_or_default();
        let Some(&idx) = bucket_idx.get(&(account_id.clone(), thread_id.clone())) else {
            continue;
        };
        email_ids_in_buckets.insert(id.clone());
        let blob_id: String = r.try_get("blob_id").unwrap_or_default();
        docs[idx].inputs.read("emails", &id);
        docs[idx]
            .inputs
            .read("email_blobs", &EmlBlobRow::pk_recipe(&id, &blob_id));
        docs[idx].emails.push(LoadedEmail {
            id,
            account_id,
            thread_id,
            blob_id,
            message_id: r.try_get::<Option<String>, _>("message_id").unwrap_or(None),
            in_reply_to: r
                .try_get::<Option<String>, _>("in_reply_to")
                .unwrap_or(None),
            references: r
                .try_get::<Option<String>, _>("references_header")
                .unwrap_or(None),
            received_at: r
                .try_get::<Option<String>, _>("received_at")
                .unwrap_or(None),
            sent_at: r.try_get::<Option<String>, _>("sent_at").unwrap_or(None),
            size: r.try_get::<Option<i64>, _>("size").unwrap_or(None),
            subject: r.try_get::<Option<String>, _>("subject").unwrap_or(None),
            from_json: r.try_get::<Option<String>, _>("from_json").unwrap_or(None),
            to_json: r.try_get::<Option<String>, _>("to_json").unwrap_or(None),
            cc_json: r.try_get::<Option<String>, _>("cc_json").unwrap_or(None),
            has_attachment: r
                .try_get::<Option<i64>, _>("has_attachment")
                .unwrap_or(None)
                .unwrap_or(0)
                != 0,
        });
    }

    if email_ids_in_buckets.is_empty() {
        return Ok(docs);
    }

    let mut email_to_bucket: HashMap<String, usize> = HashMap::new();
    for (idx, bucket) in docs.iter().enumerate() {
        for em in &bucket.emails {
            email_to_bucket.insert(em.id.clone(), idx);
        }
    }

    let email_ids = serde_json::to_string(&email_ids_in_buckets)?;

    let mailbox_rows = sqlx::query(
        "SELECT id, email_id, mailbox_id FROM email_mailboxes
          WHERE email_id IN (SELECT value FROM json_each(?))",
    )
    .bind(&email_ids)
    .fetch_all(pool)
    .await
    .context("phase 2 email_mailboxes select")?;
    for r in mailbox_rows {
        let e: String = r.try_get("email_id").unwrap_or_default();
        let m: String = r.try_get("mailbox_id").unwrap_or_default();
        let Some(&idx) = email_to_bucket.get(&e) else {
            continue;
        };
        docs[idx].inputs.read(
            "email_mailboxes",
            &r.try_get::<String, _>("id").unwrap_or_default(),
        );
        docs[idx].joins.mailboxes.entry(e).or_default().push(m);
    }

    let keyword_rows = sqlx::query(
        "SELECT id, email_id, keyword FROM email_keywords
          WHERE email_id IN (SELECT value FROM json_each(?))",
    )
    .bind(&email_ids)
    .fetch_all(pool)
    .await
    .context("phase 2 email_keywords select")?;
    for r in keyword_rows {
        let e: String = r.try_get("email_id").unwrap_or_default();
        let k: String = r.try_get("keyword").unwrap_or_default();
        let Some(&idx) = email_to_bucket.get(&e) else {
            continue;
        };
        docs[idx].inputs.read(
            "email_keywords",
            &r.try_get::<String, _>("id").unwrap_or_default(),
        );
        docs[idx].joins.keywords.entry(e).or_default().push(k);
    }

    // Attachments are extracted later by mail-parsing each loaded
    // `.eml` — see `extract_attachments_from_emls`. No
    // `email_attachments` table to query after the eml-as-canonical
    // port.

    Ok(docs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl::bulk::SQLITE_MAX_VARIABLES;
    use datalib_etl_email::ingest::schema_raw::{EmailKeywordRow, EmailMailboxRow, EmailRow};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    /// A full render of a mailbox with more threads than SQLite binds in
    /// one statement (#1156): each thread, its email and the email's
    /// mailbox and keyword all come back.
    #[tokio::test]
    async fn loads_more_threads_than_sqlite_binds_in_one_statement() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(SqliteConnectOptions::from_str("sqlite::memory:").unwrap())
            .await
            .unwrap();
        let ddl = [
            EmailRow::all_ddl(),
            EmailMailboxRow::all_ddl(),
            EmailKeywordRow::all_ddl(),
        ];
        for sql in ddl.concat() {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&pool)
                .await
                .unwrap();
        }
        let n = SQLITE_MAX_VARIABLES + 1;
        // `?1` rows numbered from 1, for each table.
        let numbered = "WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < ?1)";
        for insert in [
            "INSERT INTO emails (id, account_id, thread_id, blob_id, references_header)
             SELECT 'e' || value, 'acct', 't' || value, 'b' || value, '<r' || value || '>' FROM s",
            "INSERT INTO email_mailboxes (id, email_id, mailbox_id)
             SELECT 'e' || value || '#inbox', 'e' || value, 'inbox' FROM s",
            "INSERT INTO email_keywords (id, email_id, keyword)
             SELECT 'e' || value || '#$seen', 'e' || value, '$seen' FROM s",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!("{numbered} {insert}")))
                .bind(n as i64)
                .execute(&pool)
                .await
                .unwrap();
        }
        let to_load: HashSet<(String, String)> = (1..=n)
            .map(|i| ("acct".to_string(), format!("t{i}")))
            .collect();

        let docs = load_buckets(&pool, &to_load).await.unwrap();

        assert_eq!(docs.len(), n);
        let last = docs
            .iter()
            .find(|d| d.thread_id == format!("t{n}"))
            .unwrap();
        let email = &last.emails[..];
        assert_eq!(email.len(), 1);
        assert_eq!(
            email[0].references.as_deref(),
            Some(format!("<r{n}>").as_str())
        );
        assert_eq!(last.joins.mailboxes[&email[0].id], vec!["inbox"]);
        assert_eq!(last.joins.keywords[&email[0].id], vec!["$seen"]);
    }
}
