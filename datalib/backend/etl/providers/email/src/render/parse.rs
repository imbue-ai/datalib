//! Parse the email raw store, driven by **`dolt_diff_<table>`**.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::{self, BlobBundle};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use crate::download::db::{db_path_for, EmailJoins, LoadedEmail};

/// SQL projection from the `email_blobs` edge's `blake3` to `.eml`
/// bytes. Consumed by [`BlobBundle::load`]. After the eml-as-canonical
/// port we only load `.eml`s — attachment parts are mail-parsed out of
/// the loaded `.eml` bytes and added to the same per-bucket
/// `BlobBundle` under synthesized content-hash ref ids. `DISTINCT`
/// because several emails can edge to the same `.eml` blob.
const EML_PROJECTION_SQL: &str = "
    SELECT DISTINCT blob_id AS ref_id, blake3,
           'message/rfc822' AS content_type,
           NULL AS upstream_name
      FROM pinned_email_blobs email_blobs
     WHERE blob_id IN ({placeholders}) AND blake3 IS NOT NULL";

/// Result of the dolt_diff scan. Travels alongside the parsed bag so
/// render can advance the cursor + log timing without a second
/// round-trip.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// `Some(set)` → load only threads whose `(account_id, thread_id)`
    /// is in `set`. `None` → cold start, render every thread.
    pub changed_threads: Option<HashSet<(String, String)>>,
    /// The HEAD commit hash at scan time. `None` if `dolt_log()` was
    /// unavailable (non-doltlite sqlite); cursor stays unwritten.
    pub new_head: Option<String>,
    /// Wall-clock time spent in the union query. `None` on cold
    /// start (no diff was issued).
    pub scan_elapsed: Option<Duration>,
}

#[derive(Clone, Default)]
pub struct ParsedEmail {
    pub accounts: Vec<Value>,
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
    /// `(account_id, thread_id)` pairs the diff named that no email or
    /// thread row still carries — threads the mailbox lost. Empty on a cold
    /// start, which looks at every thread and so has nothing to compare
    /// against.
    pub vanished_threads: Vec<(String, String)>,
}

/// One rendered-markdown bucket: every email in a single JMAP Thread
/// plus its joins. Carries the per-doc [`BlobBundle`] of attachment
/// bytes (.eml bodies + each attachment part) loaded in two SQL
/// queries by `parse`.
#[derive(Debug, Clone, Default)]
pub struct EmailThreadBucket {
    pub account_id: String,
    pub thread_id: String,
    pub emails: Vec<LoadedEmail>,
    pub joins: EmailJoins,
    pub blobs: BlobBundle,
}

pub fn parse_export(input: &Path) -> Result<ParsedEmail> {
    parse(input, None)
}

pub fn parse(input: &Path, last_render_hash: Option<&str>) -> Result<ParsedEmail> {
    let db_path = db_path_for(input);
    if !db_path.is_file() {
        return Ok(ParsedEmail::default());
    }
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(async move { parse_async(&db_path, last_render_hash).await })
    })
}

async fn parse_async(db_path: &Path, last_render_hash: Option<&str>) -> Result<ParsedEmail> {
    let pool = datalib_etl::doltlite_raw::open_reader(db_path)
        .await
        .with_context(|| format!("open raw doltlite for render at {}", db_path.display()))?;

    let cas_path = blob_cas::cas_path_for(db_path);
    let cas_pool: Option<SqlitePool> = if cas_path.is_file() {
        Some(
            datalib_etl::doltlite_raw::open_reader(&cas_path)
                .await
                .with_context(|| format!("open CAS for render at {}", cas_path.display()))?,
        )
    } else {
        None
    };

    // Pin before anything reads this store. The diff below and the rows
    // behind it have to name one commit, and the `pinned_<table>` views must
    // already exist when the diff runs — its bucket query joins live tables.
    // No commit at all means nothing has been committed here to render, which
    // is emptiness, not a reason to read the working set.
    let Some(pin) = datalib_etl::pin::head(&pool).await? else {
        return Ok(ParsedEmail::default());
    };
    datalib_etl::pin::install_views(&pool, &pin)
        .await
        .context("pin the email raw store for render")?;

    let accounts = load_payloads(&pool, datalib_etl::pin::Reads::At(&pin), "accounts").await?;
    let mailboxes = load_payloads(&pool, datalib_etl::pin::Reads::At(&pin), "mailboxes").await?;
    let threads = load_payloads(&pool, datalib_etl::pin::Reads::At(&pin), "threads").await?;

    // ── Phase 1: which threads changed since last_render_hash? ────
    let scan = scan_diff(&pool, last_render_hash, &pin).await?;

    let (to_load, docs_skipped) = match &scan.changed_threads {
        None => {
            // Cold start — load every thread with at least one email.
            let all = load_all_thread_keys(&pool).await?;
            (all, 0usize)
        }
        Some(changed) => {
            // Count "total threads that have any emails" so the
            // skipped count is meaningful; same denominator the
            // render/load progress bar uses.
            let total = load_all_thread_keys(&pool).await?;
            let skipped = total.difference(changed).count();
            let load: HashSet<(String, String)> = total.intersection(changed).cloned().collect();
            (load, skipped)
        }
    };

    // ── Phase 2: targeted load for to-render buckets ──────────────
    let mut docs = if to_load.is_empty() {
        Vec::new()
    } else {
        load_buckets(&pool, &to_load).await?
    };

    // Per-bucket BlobBundle: gather every email's blob_id (the .eml
    // CAS pointer) and bulk-load the .eml bytes via the projection
    // SQL. Then mail-parse each loaded .eml to download attachment
    // parts, adding each part's bytes back into the bundle under a
    // synthesized content-hash ref id and populating
    // `bucket.joins.attachments[email_id]` so render's existing
    // `bucket.blobs.get(&att.blob_id)` lookup resolves uniformly.
    if let Some(cas_pool) = cas_pool.as_ref() {
        for bucket in &mut docs {
            let mut seen: HashSet<String> = HashSet::new();
            let mut refs: Vec<&str> = Vec::new();
            for em in &bucket.emails {
                if seen.insert(em.blob_id.clone()) {
                    refs.push(em.blob_id.as_str());
                }
            }
            if refs.is_empty() {
                continue;
            }
            bucket.blobs = BlobBundle::load(&pool, cas_pool, EML_PROJECTION_SQL, &refs).await?;
            extract_attachments_from_emls(bucket);
        }
    }

    // Threads the diff named that nothing in the store still carries.
    // Checked on `thread_id` alone rather than the `(account, thread)`
    // pair the bucket is keyed by: a thread id that survives under a
    // different account reads as present, so the error runs toward
    // missing a deletion rather than inventing one.
    let vanished_threads = match scan.changed_threads.as_ref() {
        Some(changed) => {
            let ids: std::collections::HashSet<String> =
                changed.iter().map(|(_, t)| t.clone()).collect();
            let gone: std::collections::HashSet<String> =
                datalib_etl::doltlite_raw::buckets_without_rows(
                    &pool,
                    datalib_etl::pin::Reads::At(&pin),
                    &ids,
                    &[("threads", "id"), ("emails", "thread_id")],
                )
                .await?
                .into_iter()
                .collect();
            let mut out: Vec<(String, String)> = changed
                .iter()
                .filter(|(_, t)| gone.contains(t))
                .cloned()
                .collect();
            out.sort();
            out
        }
        None => Vec::new(),
    };

    Ok(ParsedEmail {
        accounts,
        mailboxes,
        threads,
        docs,
        docs_skipped,
        scan,
        vanished_threads,
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

    for em in &bucket.emails {
        let Some(eml_blob) = bucket.blobs.get(&em.blob_id) else {
            continue;
        };
        let eml_bytes = eml_blob.bytes.clone();
        let Some(msg) = MessageParser::default().parse(&eml_bytes) else {
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
        let mut atts: Vec<crate::download::db::LoadedAttachment> = Vec::new();
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
            bucket
                .blobs
                .add(&blob_id, bytes, content_type.clone(), name.clone());
            atts.push(crate::download::db::LoadedAttachment {
                part_id: format!("p{idx}"),
                blob_id,
                name,
                content_type,
                size: Some(size),
                disposition,
                cid,
            });
        }
        if !atts.is_empty() {
            bucket.joins.attachments.insert(em.id.clone(), atts);
        }
    }
}

/// Per-bucket dolt_diff scan. Delegates to the shared
/// [`datalib_etl::doltlite_raw::scan_buckets`] helper, which is
/// the same primitive every other provider uses (slack / chatgpt /
/// claude / signal). Bucket key shape is `"<account_id>|<thread_id>"`
/// so it fits the helper's `HashSet<String>` API; we split it back
/// into a `(String, String)` pair locally for the load step.
async fn scan_diff(
    pool: &SqlitePool,
    last_render_hash: Option<&str>,
    pin: &datalib_etl::pin::Pin,
) -> Result<ScanResult> {
    let scan = datalib_etl::doltlite_raw::scan_buckets(
        pool,
        last_render_hash,
        pin,
        &datalib_etl::doltlite_raw::DiffScanSpec {
            global_fanout_tables: &[],
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
                      JOIN pinned_emails emails ON emails.id = coalesce(d.to_email_id, d.from_email_id)
                     WHERE d.from_ref = ?1 AND d.to_ref = ?2 AND d.diff_type != 'unchanged'
                    UNION
                    SELECT emails.account_id, emails.thread_id
                      FROM dolt_diff_email_keywords d
                      JOIN pinned_emails emails ON emails.id = coalesce(d.to_email_id, d.from_email_id)
                     WHERE d.from_ref = ?1 AND d.to_ref = ?2 AND d.diff_type != 'unchanged'
                    UNION
                    SELECT t.account_id,
                           coalesce(dt.to_id, dt.from_id) AS thread_id
                      FROM dolt_diff_threads dt
                      JOIN pinned_threads t ON t.id = coalesce(dt.to_id, dt.from_id)
                     WHERE dt.from_ref = ?1 AND dt.to_ref = ?2 AND dt.diff_type != 'unchanged'
                )
                WHERE account_id IS NOT NULL AND thread_id IS NOT NULL
            ",
        },
    )
    .await?;
    let changed_threads = scan.changed_buckets.map(|set| {
        set.into_iter()
            .filter_map(|key| {
                let (a, t) = key.split_once('|')?;
                Some((a.to_string(), t.to_string()))
            })
            .collect::<HashSet<(String, String)>>()
    });
    Ok(ScanResult {
        changed_threads,
        new_head: scan.new_head,
        scan_elapsed: scan.scan_elapsed,
    })
}

async fn load_all_thread_keys(pool: &SqlitePool) -> Result<HashSet<(String, String)>> {
    let rows = sqlx::query("SELECT DISTINCT account_id, thread_id FROM pinned_emails")
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

async fn load_payloads(
    pool: &SqlitePool,
    reads: datalib_etl::pin::Reads<'_>,
    table: &str,
) -> Result<Vec<Value>> {
    let table = reads.table(table);
    let sql = format!("SELECT json(payload) AS payload FROM {table} WHERE payload IS NOT NULL");
    // Audited: `table` is a literal at both callsites.
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .with_context(|| format!("load_payloads {table}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let s: String = r.try_get("payload").unwrap_or_default();
        if let Ok(v) = serde_json::from_str::<Value>(&s) {
            out.push(v);
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
        });
    }

    let placeholders = std::iter::repeat_n("?", wanted_thread_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT id, account_id, thread_id, blob_id, message_id, in_reply_to, \"references\",
                received_at, sent_at, size, subject, from_json, to_json, cc_json, has_attachment
           FROM pinned_emails emails
          WHERE thread_id IN ({placeholders})
          ORDER BY thread_id, received_at, id"
    );
    // Audited: static template; the only interpolation is a `?,?,?` run sized
    // from the chunk length. Every value is bound.
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for t in &wanted_thread_ids {
        q = q.bind(t);
    }
    let erows = q.fetch_all(pool).await.context("phase 2 emails select")?;
    let mut email_ids_in_buckets: HashSet<String> = HashSet::with_capacity(erows.len());
    for r in &erows {
        let id: String = r.try_get("id").unwrap_or_default();
        let account_id: String = r.try_get("account_id").unwrap_or_default();
        let thread_id: String = r.try_get("thread_id").unwrap_or_default();
        let Some(&idx) = bucket_idx.get(&(account_id.clone(), thread_id.clone())) else {
            continue;
        };
        email_ids_in_buckets.insert(id.clone());
        docs[idx].emails.push(LoadedEmail {
            id,
            account_id,
            thread_id,
            blob_id: r.try_get("blob_id").unwrap_or_default(),
            message_id: r.try_get::<Option<String>, _>("message_id").unwrap_or(None),
            in_reply_to: r
                .try_get::<Option<String>, _>("in_reply_to")
                .unwrap_or(None),
            references: r.try_get::<Option<String>, _>("references").unwrap_or(None),
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

    let placeholders = std::iter::repeat_n("?", email_ids_in_buckets.len())
        .collect::<Vec<_>>()
        .join(",");

    // mailboxes
    let sql = format!(
        "SELECT email_id, mailbox_id FROM pinned_email_mailboxes email_mailboxes WHERE email_id IN ({placeholders})"
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for e in &email_ids_in_buckets {
        q = q.bind(e);
    }
    for r in q
        .fetch_all(pool)
        .await
        .context("phase 2 email_mailboxes select")?
    {
        let e: String = r.try_get("email_id").unwrap_or_default();
        let m: String = r.try_get("mailbox_id").unwrap_or_default();
        let Some(&idx) = email_to_bucket.get(&e) else {
            continue;
        };
        docs[idx].joins.mailboxes.entry(e).or_default().push(m);
    }

    // keywords
    let sql = format!(
        "SELECT email_id, keyword FROM pinned_email_keywords email_keywords WHERE email_id IN ({placeholders})"
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for e in &email_ids_in_buckets {
        q = q.bind(e);
    }
    for r in q
        .fetch_all(pool)
        .await
        .context("phase 2 email_keywords select")?
    {
        let e: String = r.try_get("email_id").unwrap_or_default();
        let k: String = r.try_get("keyword").unwrap_or_default();
        let Some(&idx) = email_to_bucket.get(&e) else {
            continue;
        };
        docs[idx].joins.keywords.entry(e).or_default().push(k);
    }

    // Attachments are extracted later by mail-parsing each loaded
    // `.eml` — see `extract_attachments_from_emls`. No
    // `email_attachments` table to query after the eml-as-canonical
    // port.

    Ok(docs)
}
