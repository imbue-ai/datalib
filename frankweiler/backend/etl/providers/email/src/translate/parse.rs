//! Two-phase parse of the email raw store.
//!
//! **Phase 1 — bucket fingerprints via SQL.**
//! One row per `(account_id, thread_id)` bucket — that's the email
//! provider's rendering unit, one markdown doc per JMAP Thread. The
//! aggregate's `bucket_concat` column carries a deterministic
//! concatenation of every member email's content state: the
//! `emails.blake3` (Blake3 of the `.eml` bytes), sorted mailbox ids,
//! sorted keyword set, and sorted per-attachment blake3 hashes. We
//! hash the concat once in Rust → the per-thread bucket fingerprint.
//!
//! Compared to the per-thread `source_fingerprint` walk this replaced
//! (which read every `LoadedEmail` envelope + every join row even
//! for threads we'd skip), Phase 1 deserializes nothing and reads no
//! envelopes — just one SQL aggregate over indexes.
//!
//! **Phase 2 — load envelopes only for to-render buckets.**
//! Compare each row's fingerprint to the matching entry in
//! `prior_fingerprints` (keyed by `thread_uuid(account_id, thread_id)`,
//! the same key the indexer stores on rendered docs). Skip matches;
//! load only the survivors. Phase 2 issues one bounded `SELECT … WHERE
//! thread_id IN (?, …)` against `emails`, plus one each against the
//! three join tables, and routes the rows into per-bucket
//! [`EmailThreadBucket`]s.
//!
//! The two-phase shape mirrors the signal provider's `parse_async`
//! (see `frankweiler_etl_signal::translate::parse`); email is the
//! second provider on the pattern.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{self, BlobReader, InMemoryBlobReader};
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use crate::extract::db::{db_path_for, EmailJoins, LoadedAttachment, LoadedEmail};

// `RENDER_VERSION` is mixed into the bucket-fingerprint pre-image so
// bumping the renderer's bytes-for-bytes output forces every doc to
// re-render even when upstream payloads are unchanged. Same lever
// signal uses; lives in `translate::mod`.
use super::RENDER_VERSION;

/// Bag passed from `parse` to `render`. Mirrors signal's
/// `ParsedSignal`. Wide tables (accounts, mailboxes) load
/// unconditionally — they're small and the renderer needs them for
/// display lookups regardless of which thread is being written.
#[derive(Clone)]
pub struct ParsedEmail {
    /// Every JMAP account payload. Used for the human-readable
    /// account slug in the rendered tree.
    pub accounts: Vec<Value>,
    /// Every mailbox payload. Used for id → display-name resolution
    /// in the rendered `Mailboxes:` line.
    pub mailboxes: Vec<Value>,
    /// Every thread payload. Used only when the renderer needs
    /// a per-thread JMAP attribute we don't already promote.
    pub threads: Vec<Value>,
    /// One bucket per `(account_id, thread_id)` whose fingerprint
    /// changed. Buckets whose fingerprint matched a prior render are
    /// entirely absent from this list (we don't even load their
    /// envelopes). Ordered by `(account_id, thread_id)` so the
    /// rendered tree is deterministic.
    pub docs: Vec<EmailThreadBucket>,
    /// Count of buckets whose fingerprint matched a prior render and
    /// were therefore skipped. Reported into the render summary.
    pub docs_skipped: usize,
    /// Streaming handle to attachment bytes stored in the sibling CAS
    /// file. Render fetches one blob's bytes at a time on demand
    /// rather than bulk-loading them all into memory.
    pub blobs: Arc<dyn BlobReader>,
}

impl Default for ParsedEmail {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            mailboxes: Vec::new(),
            threads: Vec::new(),
            docs: Vec::new(),
            docs_skipped: 0,
            blobs: InMemoryBlobReader::empty_handle(),
        }
    }
}

/// One rendered-markdown bucket: every email in a single JMAP Thread,
/// plus its joins, plus the bucket fingerprint that Phase 1
/// pre-computed.
///
/// **Content fingerprint:** Blake3 of the per-thread `bucket_concat`
/// — see the SQL in [`bucket_fingerprint_query`] for the exact
/// concatenation order. Render writes it into the sidecar's
/// `header.source_fingerprint`; the next translate run reads sidecars
/// to build the `prior_fingerprints` map and skips buckets whose
/// fingerprint hasn't changed.
#[derive(Debug, Clone)]
pub struct EmailThreadBucket {
    pub account_id: String,
    pub thread_id: String,
    pub fingerprint: String,
    pub emails: Vec<LoadedEmail>,
    pub joins: EmailJoins,
}

/// Compatibility entry point for callers that don't pass a
/// `prior_fingerprints` map (older test code, ad-hoc repros). Forces
/// every bucket to render. New code should call [`parse`] directly.
pub fn parse_export(input: &Path) -> Result<ParsedEmail> {
    let prior = HashMap::new();
    parse(input, &prior)
}

/// Two-phase parse. Phase 1 reads only the small CTE result + the
/// account/mailbox/thread payload tables; Phase 2 reads emails +
/// joins for the buckets that survived the fingerprint comparison.
///
/// The bucket key is `thread_uuid(account_id, thread_id)` — same as
/// what the renderer writes to disk and what the indexer stores on
/// the rendered doc. Account_id is the natural namespace for email
/// (a personal Fastmail account and a Gmail `.mbox` export
/// land in distinct accounts, so thread ids can't collide across
/// sources without colliding on account_id first).
pub fn parse(input: &Path, prior_fingerprints: &HashMap<String, String>) -> Result<ParsedEmail> {
    let db_path = db_path_for(input);
    if !db_path.is_file() {
        return Ok(ParsedEmail::default());
    }
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(async move { parse_async(&db_path, prior_fingerprints).await })
    })
}

async fn parse_async(
    db_path: &Path,
    prior_fingerprints: &HashMap<String, String>,
) -> Result<ParsedEmail> {
    let opts =
        SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?.read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("open raw doltlite for translate at {}", db_path.display()))?;

    // Sibling CAS pool drives the per-provider BlobReader. Phase 2
    // doesn't touch attachment bytes — render does, lazily.
    let cas_path = blob_cas::cas_path_for(db_path);
    let blobs: Arc<dyn BlobReader> = if cas_path.is_file() {
        let cas_opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", cas_path.display()))?
            .read_only(true);
        let cas_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(cas_opts)
            .await
            .with_context(|| format!("open CAS for translate at {}", cas_path.display()))?;
        Arc::new(super::blob_reader::EmailBlobReader::new(
            pool.clone(),
            cas_pool,
        ))
    } else {
        InMemoryBlobReader::empty_handle()
    };

    let accounts = load_payloads(&pool, "accounts").await?;
    let mailboxes = load_payloads(&pool, "mailboxes").await?;
    let threads = load_payloads(&pool, "threads").await?;

    // ── Phase 1: bucket fingerprints via SQL ───────────────────────
    let bucket_rows = bucket_fingerprint_query(&pool).await?;
    let mut to_load: Vec<(String, String, String)> = Vec::new();
    let mut docs_skipped: usize = 0;
    for (account_id, thread_id, bucket_concat) in bucket_rows {
        let mut pre_image = RENDER_VERSION.to_le_bytes().to_vec();
        pre_image.extend_from_slice(bucket_concat.as_bytes());
        let fingerprint = blob_cas::blake3_hex(&pre_image);
        let tuid = super::render::thread_uuid(&account_id, &thread_id);
        if prior_fingerprints.get(&tuid) == Some(&fingerprint) {
            docs_skipped += 1;
        } else {
            to_load.push((account_id, thread_id, fingerprint));
        }
    }

    // ── Phase 2: targeted load for the to-render buckets ──────────
    let docs = if to_load.is_empty() {
        Vec::new()
    } else {
        load_buckets(&pool, &to_load).await?
    };

    Ok(ParsedEmail {
        accounts,
        mailboxes,
        threads,
        docs,
        docs_skipped,
        blobs,
    })
}

/// Generic payload loader for the account/mailbox/thread tables —
/// same code on every provider's read path; we don't have a sync
/// version of `dr::load_payloads` so we inline the SELECT here.
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

/// Phase-1 query. Returns one row per `(account_id, thread_id)`. Each
/// row's `bucket_concat` column is a deterministic concatenation of
/// the bucket's per-email content state, suitable for hashing into
/// the bucket fingerprint.
///
/// **Per-email content state.** For each email we concatenate:
///   - `emails.blake3` (the `.eml` content hash; NULL → empty), then
///     `:`, then
///   - sorted mailbox ids joined with `,` (the `Mailboxes:` line in
///     the rendered markdown depends on this), then `:`, then
///   - sorted keyword set joined with `,` (`$seen`, `$flagged` etc.),
///     then `:`, then
///   - sorted attachment blake3 hashes joined with `,`
///     (attachment content changes mean the rendered preview /
///     materialized blob set changes).
///
/// **Bucket aggregation.** Per-email strings are joined with `|` in
/// `(received_at, email_id)` order — same display order the renderer
/// uses, so the concat is byte-stable across runs.
async fn bucket_fingerprint_query(pool: &SqlitePool) -> Result<Vec<(String, String, String)>> {
    let sql = "WITH per_email AS (
            SELECT
                em.account_id,
                em.thread_id,
                em.id AS email_id,
                em.received_at,
                coalesce(em.blake3, '') AS email_blake3,
                coalesce(
                    (SELECT group_concat(mailbox_id, ',' ORDER BY mailbox_id)
                       FROM email_mailboxes WHERE email_id = em.id),
                    ''
                ) AS mailbox_csv,
                coalesce(
                    (SELECT group_concat(keyword, ',' ORDER BY keyword)
                       FROM email_keywords WHERE email_id = em.id),
                    ''
                ) AS keyword_csv,
                coalesce(
                    (SELECT group_concat(blake3, ',' ORDER BY part_id)
                       FROM email_attachments
                      WHERE email_id = em.id AND blake3 IS NOT NULL),
                    ''
                ) AS attachment_blake3_csv
              FROM emails em
        )
        SELECT
            account_id,
            thread_id,
            group_concat(
                email_blake3 || ':' || mailbox_csv || ':' || keyword_csv
                    || ':' || attachment_blake3_csv,
                '|' ORDER BY received_at, email_id
            ) AS bucket_concat
          FROM per_email
         GROUP BY account_id, thread_id
         ORDER BY account_id, thread_id";
    let rows = sqlx::query(sql)
        .fetch_all(pool)
        .await
        .context("bucket fingerprint query")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let account_id: String = r.try_get("account_id")?;
        let thread_id: String = r.try_get("thread_id")?;
        let bucket_concat: String = r.try_get("bucket_concat").unwrap_or_default();
        out.push((account_id, thread_id, bucket_concat));
    }
    Ok(out)
}

/// Phase-2 load: pull envelopes + joins only for the to-render
/// buckets. One SELECT against `emails` filtered to the surviving
/// thread_ids; three more against the join tables filtered to the
/// matching email_ids. Then route rows into per-bucket
/// [`EmailThreadBucket`]s.
async fn load_buckets(
    pool: &SqlitePool,
    to_load: &[(String, String, String)],
) -> Result<Vec<EmailThreadBucket>> {
    // Build the bucket index up front so each loaded row knows which
    // bucket it belongs to in O(1).
    let mut bucket_idx: HashMap<(String, String), usize> = HashMap::new();
    let mut docs: Vec<EmailThreadBucket> = Vec::with_capacity(to_load.len());
    let mut wanted_thread_ids: HashSet<String> = HashSet::with_capacity(to_load.len());
    for (account_id, thread_id, fingerprint) in to_load {
        bucket_idx.insert((account_id.clone(), thread_id.clone()), docs.len());
        wanted_thread_ids.insert(thread_id.clone());
        docs.push(EmailThreadBucket {
            account_id: account_id.clone(),
            thread_id: thread_id.clone(),
            fingerprint: fingerprint.clone(),
            emails: Vec::new(),
            joins: EmailJoins::default(),
        });
    }

    // ── envelopes ────────────────────────────────────────────────
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

    // ── joins, filtered to the email ids we just loaded ──────────
    // Build a `(email_id → bucket_idx)` map so each join row routes
    // straight into its bucket's `EmailJoins`.
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
