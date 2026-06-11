//! Parse the doltlite raw store into a small in-memory `ParsedSignal`
//! that the renderer can walk without re-querying.
//!
//! We read three tables — `recipients`, `chats`, `chat_items` — decode
//! each `chat_items.payload` JSON to extract the text body of any
//! `StandardMessage`, and bucket the result by (chat, period). Other
//! ChatItem variants (stickers, view-once, ChatUpdate, …) are skipped
//! silently in this render version; the raw doltlite still has the
//! payload, so a future `RENDER_VERSION` bump can surface them.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{self, BlobReader, BlobView, InMemoryBlobReader};
use frankweiler_etl::periodize::Period;
use frankweiler_signal_backup::backup;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

#[derive(Clone)]
pub struct ParsedSignal {
    pub recipients: HashMap<String, ParsedRecipient>,
    /// Chats indexed by `chat_id` for lookup from `DocBucket`. The
    /// chats themselves carry no items — each item ends up in the
    /// matching bucket in `docs`.
    pub chats: HashMap<String, ParsedChat>,
    /// One bucket per `(chat_id, period_key)` pair that needs
    /// re-rendering — i.e., buckets whose content fingerprint does
    /// **not** match the matching entry in the
    /// `prior_fingerprints` map passed in to [`parse`]. Buckets
    /// whose fingerprint is unchanged are entirely omitted (we don't
    /// even load their chat_items off disk). Ordered by chat_id then
    /// period_key so the rendered tree is deterministic.
    pub docs: Vec<DocBucket>,
    /// Count of buckets whose fingerprint matched a prior render and
    /// were therefore skipped. Reported into [`super::render::RenderSummary`]
    /// so the orchestrator's `docs_skipped` counter stays accurate
    /// even though the skipped buckets never appear in `docs`.
    pub docs_skipped: usize,
    /// Streaming handle to attachment bytes stored in the sibling
    /// CAS file. Render fetches one blob's bytes at a time on demand
    /// rather than bulk-loading them all into memory.
    pub blobs: Arc<dyn BlobReader>,
}

impl Default for ParsedSignal {
    fn default() -> Self {
        Self {
            recipients: HashMap::new(),
            chats: HashMap::new(),
            docs: Vec::new(),
            docs_skipped: 0,
            blobs: InMemoryBlobReader::empty_handle(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ParsedRecipient {
    pub id: String,
    pub identifier: Option<String>,
    pub display_name: Option<String>,
}

impl ParsedRecipient {
    pub fn display(&self) -> String {
        self.display_name
            .clone()
            .or_else(|| self.identifier.clone())
            .unwrap_or_else(|| format!("recipient_{}", self.id))
    }
}

#[derive(Debug, Clone)]
pub struct ParsedChat {
    pub id: String,
    pub recipient_id: String,
}

/// One rendered-markdown bucket: a slice of a chat covering a single
/// period key (`2024-03`, `2024-03-15`, `2024`, or `all`).
///
/// **Content fingerprint:** blake3 of the concatenation, in
/// `date_sent ASC` order, of every member chat_item's
/// `payload_blake3` + sorted attachment blake3 hashes. Computed by
/// [`parse`] from a single SQL `group_concat` aggregate — no need
/// to deserialize payloads to derive it. The render path writes it
/// into the sidecar's `header.source_fingerprint`; the next
/// translate run reads sidecars to build the `prior_fingerprints`
/// map and skips buckets whose fingerprint hasn't changed.
#[derive(Debug, Clone)]
pub struct DocBucket {
    pub chat_id: String,
    pub period_key: String,
    pub fingerprint: String,
    pub items: Vec<ParsedChatItem>,
}

#[derive(Debug, Clone)]
pub struct ParsedChatItem {
    /// `{chat_id}#{author_id}#{date_sent}` — matches the
    /// `chat_items.id` PK and the `blob_refs.owning_id` for every
    /// attachment under this item.
    pub item_pk: String,
    pub author_id: String,
    pub date_sent: i64,
    pub text: Option<String>,
    /// True when ChatItem.directionalDetails was `outgoing`. Drives
    /// "me" attribution in the rendered markdown.
    pub outgoing: bool,
    /// Attachments on this item, ordered by their position in the
    /// `StandardMessage.attachments` repeated field (matches the
    /// `slot` we stored at extract time).
    pub attachments: Vec<ParsedAttachment>,
}

/// One attachment referenced from a `ParsedChatItem`. `ref_id`
/// matches `blob_refs.id`; the render walks them, hands each to
/// `blob_cas::materialize_to_disk`, and emits an `![alt](blobs/<…>)`
/// link via `blob_cas::attachment_md`.
#[derive(Debug, Clone)]
pub struct ParsedAttachment {
    /// `local_media_name(plaintext_hash, local_key)` — the same key
    /// extract used when calling `db.store_blob(&RefStub { ref_id: … })`.
    pub ref_id: String,
    pub content_type: Option<String>,
    pub file_name: Option<String>,
    pub is_image: bool,
}

/// Compatibility wrapper: when sync hasn't passed an explicit period
/// (or for unit tests / repros) default to `Period::Month` — same
/// default the YAML knob falls back to. Forces a full re-render
/// (empty `prior_fingerprints`).
pub fn parse_raw_dir(input: &Path) -> Result<ParsedSignal> {
    let prior = HashMap::new();
    parse(input, Period::Month, "signal", &prior)
}

/// Two-phase load.
///
/// Phase 1 runs a `group_concat` aggregate over
/// `chat_items.payload_blake3` (joined with `chat_item_attachments.blake3`)
/// to compute one **bucket fingerprint** per `(chat_id, period_key)`
/// pair, *without* deserializing any payloads. Each fingerprint is
/// compared to the matching entry in `prior_fingerprints` (keyed by
/// `signal_markdown_uuid(signal_chat_uuid(source_name, chat_id), period_key)`),
/// and only the buckets whose fingerprint changed make it into
/// Phase 2.
///
/// Phase 2 runs a targeted SELECT against `chat_items` filtered to
/// the chat_ids + date_sent ranges of the to-render buckets,
/// builds [`DocBucket`]s for them, and returns. Buckets whose
/// fingerprint matched are reported via
/// [`ParsedSignal::docs_skipped`] and entirely absent from
/// [`ParsedSignal::docs`].
///
/// Recipients and chats are always loaded (they're small and
/// rendering needs them for display).
pub fn parse(
    input: &Path,
    period: Period,
    source_name: &str,
    prior_fingerprints: &HashMap<String, String>,
) -> Result<ParsedSignal> {
    let db_path = frankweiler_etl::doltlite_raw::db_path_for(input);
    if !db_path.is_file() {
        return Ok(ParsedSignal::default());
    }
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            parse_async(&db_path, period, source_name, prior_fingerprints).await
        })
    })
}

async fn parse_async(
    db_path: &Path,
    period: Period,
    source_name: &str,
    prior_fingerprints: &HashMap<String, String>,
) -> Result<ParsedSignal> {
    let opts =
        SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?.read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("open raw doltlite for translate at {}", db_path.display()))?;

    // Sibling CAS file holds attachment bytes; render fetches them on
    // demand via `SignalBlobReader`, which joins
    // `chat_item_attachments` (entity db) to `cas_objects` (CAS db)
    // on blake3 — see the struct's doc for the full lookup chain.
    let cas_path = blob_cas::cas_path_for(db_path);
    let blobs: Arc<dyn BlobReader> = if cas_path.is_file() {
        let cas_opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", cas_path.display()))?
            .read_only(true);
        let cas_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(cas_opts)
            .await
            .with_context(|| format!("open CAS for translate at {}", cas_path.display()))?;
        Arc::new(SignalBlobReader::new(pool.clone(), cas_pool))
    } else {
        InMemoryBlobReader::empty_handle()
    };

    let recipients = load_recipients(&pool).await?;
    let chats = load_chats(&pool).await?;

    // ── Phase 1: bucket fingerprints via SQL ───────────────────────
    //
    // One row per `(chat_id, period_key)` bucket, no payloads
    // touched. Each row carries a `group_concat` of every member
    // chat_item's `payload_blake3` + that item's attachment blake3
    // hashes (sorted), in `date_sent ASC` order. We hash the
    // concat to get the bucket fingerprint, compare against
    // `prior_fingerprints` keyed by the bucket's
    // `signal_markdown_uuid`, and decide which buckets to load.
    let bucket_rows = bucket_fingerprint_query(&pool, period).await?;
    let mut to_load_buckets: Vec<(String, String, String)> = Vec::new();
    let mut docs_skipped: usize = 0;
    for (chat_id, period_key, bucket_concat) in bucket_rows {
        let fingerprint = frankweiler_etl::blob_cas::blake3_hex(bucket_concat.as_bytes());
        let chat_uuid = super::signal_chat_uuid(source_name, &chat_id);
        let markdown_uuid = super::signal_markdown_uuid(&chat_uuid, &period_key);
        if prior_fingerprints.get(&markdown_uuid) == Some(&fingerprint) {
            docs_skipped += 1;
        } else {
            to_load_buckets.push((chat_id, period_key, fingerprint));
        }
    }

    // ── Phase 2: load chat_items only for to-render buckets ────────
    let docs = if to_load_buckets.is_empty() {
        Vec::new()
    } else {
        load_buckets(&pool, period, &to_load_buckets).await?
    };

    Ok(ParsedSignal {
        recipients,
        chats,
        docs,
        docs_skipped,
        blobs,
    })
}

async fn load_recipients(pool: &sqlx::SqlitePool) -> Result<HashMap<String, ParsedRecipient>> {
    let mut recipients: HashMap<String, ParsedRecipient> = HashMap::new();
    let rrows = sqlx::query("SELECT id, identifier, display_name FROM recipients")
        .fetch_all(pool)
        .await
        .context("read recipients")?;
    for r in &rrows {
        let id: String = r.try_get("id")?;
        let identifier: Option<String> = r.try_get("identifier")?;
        let display_name: Option<String> = r.try_get("display_name")?;
        recipients.insert(
            id.clone(),
            ParsedRecipient {
                id,
                identifier,
                display_name,
            },
        );
    }
    Ok(recipients)
}

async fn load_chats(pool: &sqlx::SqlitePool) -> Result<HashMap<String, ParsedChat>> {
    let crows = sqlx::query("SELECT id, recipient_id FROM chats ORDER BY id")
        .fetch_all(pool)
        .await
        .context("read chats")?;
    let mut chats: HashMap<String, ParsedChat> = HashMap::new();
    for r in &crows {
        let id: String = r.try_get("id")?;
        let recipient_id: String = r.try_get("recipient_id")?;
        chats.insert(
            id.clone(),
            ParsedChat {
                id: id.clone(),
                recipient_id,
            },
        );
    }
    Ok(chats)
}

/// Period::All bucket key — kept in one place so the SQL and Rust
/// paths agree.
const PERIOD_ALL_BUCKET_KEY: &str = "all";

/// Build the SQL fragment that derives the bucket key from
/// `date_sent` for a given [`Period`]. For non-`All` periods this
/// is a `strftime` over `date_sent / 1000` (sqlite expects
/// unix-seconds). For `Period::All` it's a literal string so every
/// chat_item lands in one bucket.
fn period_key_sql(period: Period) -> String {
    if matches!(period, Period::All) {
        format!("'{PERIOD_ALL_BUCKET_KEY}'")
    } else {
        format!(
            "strftime('{fmt}', date_sent / 1000, 'unixepoch')",
            fmt = period.strftime_fmt(),
        )
    }
}

/// Phase-1 query: one row per `(chat_id, period_key)`. Each row's
/// `bucket_concat` column is a deterministic concatenation of the
/// bucket's per-item content hashes, suitable for hashing into a
/// bucket fingerprint.
async fn bucket_fingerprint_query(
    pool: &sqlx::SqlitePool,
    period: Period,
) -> Result<Vec<(String, String, String)>> {
    let period_key_expr = period_key_sql(period);
    // The per-item hash is `payload_blake3` plus a `:` plus a
    // comma-separated list of this item's attachment blake3s sorted
    // by attachment id (deterministic). LEFT JOIN so items with no
    // attachments still appear; coalesce so the `:` and empty-list
    // appear consistently.
    //
    // `group_concat(... ORDER BY ...)` lands in sqlite 3.44+; doltlite
    // 0.11.x rides on sqlite 3.54 so the ORDER BY clause is honored
    // and the resulting string is deterministic across runs.
    let sql = format!(
        "WITH item_hash AS (
            SELECT ci.chat_id,
                   {period_key_expr} AS period_key,
                   ci.date_sent,
                   ci.id AS chat_item_id,
                   coalesce(ci.payload_blake3, '') ||
                       ':' ||
                       coalesce((
                           SELECT group_concat(blake3, ',' ORDER BY id)
                             FROM chat_item_attachments
                            WHERE chat_item_id = ci.id
                              AND blake3 IS NOT NULL
                       ), '') AS item_hash
              FROM chat_items ci
         )
         SELECT chat_id,
                period_key,
                group_concat(item_hash, '|' ORDER BY date_sent, chat_item_id) AS bucket_concat
           FROM item_hash
          GROUP BY chat_id, period_key
          ORDER BY chat_id, period_key"
    );
    let rows = sqlx::query(&sql)
        .fetch_all(pool)
        .await
        .context("bucket fingerprint query")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let chat_id: String = r.try_get("chat_id")?;
        let period_key: String = r.try_get("period_key")?;
        let bucket_concat: String = r.try_get("bucket_concat").unwrap_or_default();
        out.push((chat_id, period_key, bucket_concat));
    }
    Ok(out)
}

/// Phase-2 load: pull `chat_items.payload` for the buckets we
/// decided to render, decode the chat_item items, and shape into
/// [`DocBucket`]s with their pre-computed fingerprint attached.
async fn load_buckets(
    pool: &sqlx::SqlitePool,
    period: Period,
    to_load: &[(String, String, String)],
) -> Result<Vec<DocBucket>> {
    // Index the to-load list by (chat_id, period_key) for fast
    // routing of each loaded chat_item into its bucket.
    let mut bucket_idx: HashMap<(String, String), usize> = HashMap::new();
    let mut docs: Vec<DocBucket> = Vec::with_capacity(to_load.len());
    for (chat_id, period_key, fingerprint) in to_load {
        bucket_idx.insert((chat_id.clone(), period_key.clone()), docs.len());
        docs.push(DocBucket {
            chat_id: chat_id.clone(),
            period_key: period_key.clone(),
            fingerprint: fingerprint.clone(),
            items: Vec::new(),
        });
    }

    // Restrict to the chat_ids we need. Period filtering happens in
    // Rust (against the to-load set) — strftime in a WHERE clause
    // would be just as expensive and harder to write.
    let chat_ids: std::collections::HashSet<&str> =
        to_load.iter().map(|(c, _, _)| c.as_str()).collect();
    if chat_ids.is_empty() {
        return Ok(docs);
    }
    let placeholders = std::iter::repeat_n("?", chat_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let period_key_expr = period_key_sql(period);
    let sql = format!(
        "SELECT chat_id,
                author_id,
                date_sent,
                {period_key_expr} AS period_key,
                json(payload) AS payload
           FROM chat_items
          WHERE chat_id IN ({placeholders})
          ORDER BY chat_id, date_sent"
    );
    let mut q = sqlx::query(&sql);
    for c in &chat_ids {
        q = q.bind(*c);
    }
    let irows = q.fetch_all(pool).await.context("read chat_items")?;
    for r in &irows {
        let chat_id: String = r.try_get("chat_id")?;
        let author_id: String = r.try_get("author_id")?;
        let date_sent: i64 = r.try_get("date_sent")?;
        let period_key: String = r.try_get("period_key")?;
        let Some(&idx) = bucket_idx.get(&(chat_id.clone(), period_key.clone())) else {
            // chat_item falls into a bucket we marked as skipped —
            // discard. (Happens when only one of several periods of
            // a chat needs re-rendering.)
            continue;
        };
        let payload: String = r.try_get("payload")?;
        let item_pk =
            crate::extract::schema_raw::chat_item_id_recipe(&chat_id, &author_id, date_sent);
        let (text, outgoing, attachments) = decode_chat_item(&payload);
        docs[idx].items.push(ParsedChatItem {
            item_pk,
            author_id,
            date_sent,
            text,
            outgoing,
            attachments,
        });
    }
    Ok(docs)
}

/// Parse a `chat_items.payload` JSON string (a `Frame::ChatItem`
/// serialized via serde) and pull out (text, outgoing, attachments).
/// Returns empty defaults for non-StandardMessage chat items so the
/// renderer can skip them cleanly without panicking.
fn decode_chat_item(payload: &str) -> (Option<String>, bool, Vec<ParsedAttachment>) {
    let ci: backup::ChatItem = match serde_json::from_str(payload) {
        Ok(c) => c,
        Err(_) => return (None, false, Vec::new()),
    };
    let outgoing = matches!(
        ci.directional_details,
        Some(backup::chat_item::DirectionalDetails::Outgoing(_))
    );
    match ci.item {
        Some(backup::chat_item::Item::StandardMessage(sm)) => {
            let text = sm.text.and_then(|t| {
                if t.body.is_empty() {
                    None
                } else {
                    Some(t.body)
                }
            });
            let attachments = sm
                .attachments
                .iter()
                .filter_map(attachment_from_message)
                .collect();
            (text, outgoing, attachments)
        }
        _ => (None, outgoing, Vec::new()),
    }
}

/// Pull a `ParsedAttachment` out of a `MessageAttachment` if it has
/// the fields we need to address its bytes in the CAS. Returns `None`
/// for attachments that don't (no LocatorInfo, no plaintext hash, no
/// local key) — extract would have skipped these too, so render
/// surfacing them as missing-bytes placeholders would be misleading.
fn attachment_from_message(att: &backup::MessageAttachment) -> Option<ParsedAttachment> {
    let ptr = att.pointer.as_ref()?;
    let li = ptr.locator_info.as_ref()?;
    let local_key = li.local_key.as_deref()?;
    if local_key.len() != 64 {
        return None;
    }
    let plaintext_hash = match li.integrity_check.as_ref()? {
        backup::file_pointer::locator_info::IntegrityCheck::PlaintextHash(h) if !h.is_empty() => {
            h.clone()
        }
        _ => return None,
    };
    let mut lk = [0u8; 64];
    lk.copy_from_slice(local_key);
    let ref_id = frankweiler_signal_backup::local_media_name(&plaintext_hash, &lk);
    let content_type = ptr.content_type.clone();
    let is_image = content_type
        .as_deref()
        .map(|ct| ct.starts_with("image/"))
        .unwrap_or(false);
    Some(ParsedAttachment {
        ref_id,
        content_type,
        file_name: ptr.file_name.clone(),
        is_image,
    })
}

/// Signal-specific [`BlobReader`].
///
/// **Lookup chain.** `read_by_ref_id(ref_id)`:
///
/// 1. `SELECT blake3 FROM chat_item_attachments WHERE ref_id = ?
///    AND blake3 IS NOT NULL LIMIT 1` — resolve Signal's media_name
///    to the CAS content hash, dedupe-aware across chat_items that
///    share the same `media_name`.
/// 2. `SELECT bytes, content_type FROM cas_objects WHERE blake3 = ?`
///    — load the decrypted bytes and (universal-across-providers)
///    content_type that the CAS stored at extract time.
///
/// `upstream_name` / `source_url` on the returned [`BlobView`] are
/// always `None` — Signal's translate already pulls those from the
/// `chat_items.payload` (the `FilePointer` proto fields) and passes
/// them explicitly through [`blob_cas::attachment_md`], so the
/// BlobView never needs to carry them.
///
/// `read_by_owner` and `read_by_hash` are unused by Signal's render
/// path; they return `Ok(None)`. If a future caller needs them,
/// extend; today silently-empty is the right behavior.
pub struct SignalBlobReader {
    refs_pool: SqlitePool,
    cas_pool: SqlitePool,
}

impl SignalBlobReader {
    pub fn new(refs_pool: SqlitePool, cas_pool: SqlitePool) -> Self {
        Self {
            refs_pool,
            cas_pool,
        }
    }

    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }

    async fn read_by_ref_id_async(&self, ref_id: &str) -> Result<Option<BlobView>> {
        let blake3: Option<String> = sqlx::query_scalar(
            "SELECT blake3 FROM chat_item_attachments \
             WHERE ref_id = ? AND blake3 IS NOT NULL LIMIT 1",
        )
        .bind(ref_id)
        .fetch_optional(&self.refs_pool)
        .await
        .with_context(|| format!("lookup chat_item_attachments by ref_id {ref_id}"))?;
        let Some(blake3) = blake3 else {
            return Ok(None);
        };
        let row = sqlx::query("SELECT bytes, content_type FROM cas_objects WHERE blake3 = ?")
            .bind(&blake3)
            .fetch_optional(&self.cas_pool)
            .await
            .with_context(|| format!("lookup cas_objects by blake3 {blake3}"))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let bytes: Vec<u8> = row.try_get("bytes").unwrap_or_default();
        let content_type: Option<String> = row.try_get("content_type").ok();
        Ok(Some(BlobView {
            ref_id: ref_id.to_string(),
            owning_id: String::new(),
            slot: String::new(),
            blake3,
            content_type,
            upstream_name: None,
            source_url: None,
            bytes,
        }))
    }
}

impl BlobReader for SignalBlobReader {
    fn read_by_ref_id(&self, ref_id: &str) -> Result<Option<BlobView>> {
        self.block_on(self.read_by_ref_id_async(ref_id))
    }
    fn read_by_owner(&self, _owning_id: &str) -> Result<Option<BlobView>> {
        Ok(None)
    }
    fn read_by_hash(&self, _blake3_hash: &str) -> Result<Option<BlobView>> {
        Ok(None)
    }
}
