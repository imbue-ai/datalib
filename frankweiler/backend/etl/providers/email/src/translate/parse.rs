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

use crate::extract::db::{db_path_for, EmailJoins, LoadedAttachment, LoadedEmail};

/// SQL projection from email's two CAS-edge columns (`emails.blake3`
/// for the `.eml` body, `email_attachments.blake3` for each attachment
/// part) to their bytes. Consumed by [`BlobBundle::load`]; the
/// `{placeholders}` token is substituted twice — once per UNION half.
const ATTACHMENTS_PROJECTION_SQL: &str = "
    SELECT blob_id AS ref_id, blake3,
           'message/rfc822' AS content_type,
           NULL AS upstream_name
      FROM emails
     WHERE blob_id IN ({placeholders}) AND blake3 IS NOT NULL
    UNION ALL
    SELECT blob_id AS ref_id, blake3,
           type AS content_type,
           name AS upstream_name
      FROM email_attachments
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
        .with_context(|| format!("open raw doltlite for translate at {}", db_path.display()))?;

    let cas_path = blob_cas::cas_path_for(db_path);
    let cas_pool: Option<SqlitePool> = if cas_path.is_file() {
        let cas_opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", cas_path.display()))?
            .read_only(true);
        Some(
            SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(cas_opts)
                .await
                .with_context(|| format!("open CAS for translate at {}", cas_path.display()))?,
        )
    } else {
        None
    };

    let accounts = load_payloads(&pool, "accounts").await?;
    let mailboxes = load_payloads(&pool, "mailboxes").await?;
    let threads = load_payloads(&pool, "threads").await?;

    // ── Phase 1: which threads changed since last_render_hash? ────
    let scan = scan_diff(&pool).await?;
    let scan = match last_render_hash {
        None => scan,
        Some(from_ref) => scan_with_from_ref(&pool, from_ref, scan).await?,
    };

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
    // CAS pointer) + every attachment's blob_id and bulk-load them
    // through the UNION-ALL projection. Two queries per bucket cover
    // both source tables.
    if let Some(cas_pool) = cas_pool.as_ref() {
        for bucket in &mut docs {
            let mut seen: HashSet<String> = HashSet::new();
            let mut refs: Vec<&str> = Vec::new();
            for em in &bucket.emails {
                if seen.insert(em.blob_id.clone()) {
                    refs.push(em.blob_id.as_str());
                }
            }
            for atts in bucket.joins.attachments.values() {
                for att in atts {
                    if seen.insert(att.blob_id.clone()) {
                        refs.push(att.blob_id.as_str());
                    }
                }
            }
            if refs.is_empty() {
                continue;
            }
            bucket.blobs =
                BlobBundle::load(&pool, cas_pool, ATTACHMENTS_PROJECTION_SQL, &refs).await?;
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

/// Read HEAD only — used on cold start where there's no `from_ref`
/// yet to diff against.
async fn scan_diff(pool: &SqlitePool) -> Result<ScanResult> {
    let new_head: Option<String> =
        sqlx::query_scalar("SELECT commit_hash FROM dolt_log() ORDER BY date DESC LIMIT 1")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    Ok(ScanResult {
        changed_threads: None,
        new_head,
        scan_elapsed: None,
    })
}

/// Run the union diff query against `from_ref` and project the touched
/// `(account_id, thread_id)` pairs.
async fn scan_with_from_ref(
    pool: &SqlitePool,
    from_ref: &str,
    mut scan: ScanResult,
) -> Result<ScanResult> {
    // The three join tables (`email_mailboxes`, `email_keywords`,
    // `email_attachments`) carry only `email_id` upstream — we join
    // back through the live `emails` table to project the natural
    // bucket key (`account_id`, `thread_id`). `emails` itself and
    // `threads` are projected directly.
    //
    // `accounts` and `mailboxes` changes don't enter the union: an
    // account rename doesn't change rendered thread bytes, and
    // mailbox label changes propagate via `email_mailboxes` (which
    // we already cover).
    let sql = "
        SELECT DISTINCT account_id, thread_id FROM (
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
            SELECT emails.account_id, emails.thread_id
              FROM dolt_diff_email_attachments d
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
    ";

    let started = std::time::Instant::now();
    let rows = sqlx::query(sql)
        .bind(from_ref)
        .fetch_all(pool)
        .await
        .context("query dolt_diff_* for changed email threads")?;
    scan.scan_elapsed = Some(started.elapsed());

    let mut set: HashSet<(String, String)> = HashSet::with_capacity(rows.len());
    for r in &rows {
        let a: String = r.try_get("account_id")?;
        let t: String = r.try_get("thread_id")?;
        set.insert((a, t));
    }
    scan.changed_threads = Some(set);
    Ok(scan)
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
        "SELECT id, account_id, thread_id, blob_id, message_id, received_at, sent_at,
                size, subject, from_json, has_attachment
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
            received_at: r
                .try_get::<Option<String>, _>("received_at")
                .unwrap_or(None),
            sent_at: r.try_get::<Option<String>, _>("sent_at").unwrap_or(None),
            size: r.try_get::<Option<i64>, _>("size").unwrap_or(None),
            subject: r.try_get::<Option<String>, _>("subject").unwrap_or(None),
            from_json: r.try_get::<Option<String>, _>("from_json").unwrap_or(None),
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

    // attachments
    let sql = format!(
        "SELECT email_id, part_id, blob_id, name, type, size, disposition, cid
           FROM email_attachments WHERE email_id IN ({placeholders})
          ORDER BY email_id, part_id"
    );
    let mut q = sqlx::query(&sql);
    for e in &email_ids_in_buckets {
        q = q.bind(e);
    }
    for r in q
        .fetch_all(pool)
        .await
        .context("phase 2 email_attachments select")?
    {
        let e: String = r.try_get("email_id").unwrap_or_default();
        let Some(&idx) = email_to_bucket.get(&e) else {
            continue;
        };
        docs[idx]
            .joins
            .attachments
            .entry(e)
            .or_default()
            .push(LoadedAttachment {
                part_id: r.try_get("part_id").unwrap_or_default(),
                blob_id: r.try_get("blob_id").unwrap_or_default(),
                name: r.try_get::<Option<String>, _>("name").unwrap_or(None),
                content_type: r.try_get::<Option<String>, _>("type").unwrap_or(None),
                size: r.try_get::<Option<i64>, _>("size").unwrap_or(None),
                disposition: r
                    .try_get::<Option<String>, _>("disposition")
                    .unwrap_or(None),
                cid: r.try_get::<Option<String>, _>("cid").unwrap_or(None),
            });
    }

    Ok(docs)
}
