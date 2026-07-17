//! Parse the email raw store, driven by **`dolt_diff_<table>`**.
//!
//! Incrementality is no longer maintained by per-row content
//! fingerprints (`emails.blake3`-style aggregates over a SQL CTE) and
//! a `prior_fingerprints` map. We ask doltlite directly which threads
//! touched any row since the cursor's commit, load envelopes/joins
//! only for those threads, and re-render every email in each. Source
//! of truth for "did anything change?" is the prolly-tree diff,
//! period.
//!
//! Phase 1 — union over `dolt_diff_emails`,
//! `dolt_diff_email_mailboxes`, `dolt_diff_email_keywords`,
//! `dolt_diff_email_attachments`, `dolt_diff_threads`. The first
//! three project `to_email_id`/`from_email_id` and join back to the
//! live `emails` table to find each touched email's `(account_id,
//! thread_id)`; thread changes project `thread_id` directly.
//!
//! Phase 2 — existing targeted `SELECT … WHERE thread_id IN (?, …)`
//! over `emails` + the three join tables. No change in shape from the
//! previous bucket-fingerprint era; just a smaller `to_load` set
//! filter coming in.
//!
//! Cold start (no cursor, or `dolt_diff_<table>` unavailable) loads
//! every thread that has at least one email.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{self, BlobBundle};
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
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
      FROM email_blobs
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

/// Compatibility entry point for tests / ad-hoc repros that don't
/// have a render cursor. Forces a cold start.
pub fn parse_export(input: &Path) -> Result<ParsedEmail> {
    parse(input, None)
}

/// Two-phase parse driven by `dolt_diff_<table>`.
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
    let opts =
        SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?.read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("open raw doltlite for render at {}", db_path.display()))?;

    let cas_path = blob_cas::cas_path_for(db_path);
    let cas_pool: Option<SqlitePool> = if cas_path.is_file() {
        let cas_opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", cas_path.display()))?
            .read_only(true);
        Some(
            SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(cas_opts)
                .await
                .with_context(|| format!("open CAS for render at {}", cas_path.display()))?,
        )
    } else {
        None
    };

    let accounts = load_payloads(&pool, "accounts").await?;
    let mailboxes = load_payloads(&pool, "mailboxes").await?;
    let threads = load_payloads(&pool, "threads").await?;

    // ── Phase 1: which threads changed since last_render_hash? ────
    let scan = scan_diff(&pool, last_render_hash).await?;

    let (to_load, docs_skipped) = match &scan.changed_threads {
        None => {
            // Cold start — load every thread with at least one email.
            let all = load_all_thread_keys(&pool).await?;
            (all, 0usize)
        }
        Some(changed) => {
            // Count "total threads that have any emails" so the
            // skipped count is meaningful; same denominator the
            // sidecar/load progress bar uses.
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

    Ok(ParsedEmail {
        accounts,
        mailboxes,
        threads,
        docs,
        docs_skipped,
        scan,
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
            let blob_id = frankweiler_etl::blob_cas::blake3_hex(&bytes);
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
/// [`frankweiler_etl::doltlite_raw::scan_buckets`] helper, which is
/// the same primitive every other provider uses (slack / chatgpt /
/// anthropic / signal). Bucket key shape is `"<account_id>|<thread_id>"`
/// so it fits the helper's `HashSet<String>` API; we split it back
/// into a `(String, String)` pair locally for the load step.
///
/// Tables that fan out to "render everything" — `accounts` and
/// `mailboxes` — are intentionally NOT in `global_fanout_tables`
/// because their changes do not affect rendered thread bytes:
/// account renames just relabel the per-thread frontmatter on the
/// next cold start, and mailbox renames already propagate via
/// `email_mailboxes` diffs.
async fn scan_diff(pool: &SqlitePool, last_render_hash: Option<&str>) -> Result<ScanResult> {
    let scan = frankweiler_etl::doltlite_raw::scan_buckets(
        pool,
        last_render_hash,
        &frankweiler_etl::doltlite_raw::DiffScanSpec {
            global_fanout_tables: &[],
            bucket_query: "
                SELECT DISTINCT account_id || '|' || thread_id AS bucket_key FROM (
                    SELECT to_account_id  AS account_id, to_thread_id  AS thread_id
                      FROM dolt_diff_emails
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                    UNION
                    SELECT from_account_id, from_thread_id
                      FROM dolt_diff_emails
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                    UNION
                    SELECT emails.account_id, emails.thread_id
                      FROM dolt_diff_email_mailboxes d
                      JOIN emails ON emails.id = coalesce(d.to_email_id, d.from_email_id)
                     WHERE d.from_ref = ?1 AND d.to_ref = 'HEAD' AND d.diff_type != 'unchanged'
                    UNION
                    SELECT emails.account_id, emails.thread_id
                      FROM dolt_diff_email_keywords d
                      JOIN emails ON emails.id = coalesce(d.to_email_id, d.from_email_id)
                     WHERE d.from_ref = ?1 AND d.to_ref = 'HEAD' AND d.diff_type != 'unchanged'
                    UNION
                    SELECT t.account_id,
                           coalesce(dt.to_id, dt.from_id) AS thread_id
                      FROM dolt_diff_threads dt
                      JOIN threads t ON t.id = coalesce(dt.to_id, dt.from_id)
                     WHERE dt.from_ref = ?1 AND dt.to_ref = 'HEAD' AND dt.diff_type != 'unchanged'
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

async fn load_payloads(pool: &SqlitePool, table: &str) -> Result<Vec<Value>> {
    let sql = format!("SELECT json(payload) AS payload FROM {table} WHERE payload IS NOT NULL");
    let rows = sqlx::query(&sql)
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

/// Phase 2: pull envelopes + joins for the to-render thread set.
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
           FROM emails
          WHERE thread_id IN ({placeholders})
          ORDER BY thread_id, received_at, id"
    );
    let mut q = sqlx::query(&sql);
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
        "SELECT email_id, mailbox_id FROM email_mailboxes WHERE email_id IN ({placeholders})"
    );
    let mut q = sqlx::query(&sql);
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
    let sql =
        format!("SELECT email_id, keyword FROM email_keywords WHERE email_id IN ({placeholders})");
    let mut q = sqlx::query(&sql);
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
