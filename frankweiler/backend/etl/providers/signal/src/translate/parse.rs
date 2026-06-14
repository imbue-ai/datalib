//! Parse the doltlite raw store into a small in-memory `ParsedSignal`
//! that the renderer can walk without re-querying.
//!
//! Incrementality is driven by **`dolt_diff_<table>`**, not by Rust- or
//! SQL-side content hashes. The caller passes the doltlite commit hash
//! the renderer last successfully completed against (from
//! [`frankweiler_etl::render_cursor`]); we union the per-table diff
//! vtabs to enumerate the `chat_id`s touched between that hash and
//! `HEAD`, and load full chat data only for those. Cold start (no
//! cursor, or `dolt_diff_<table>` unavailable / non-doltlite sqlite)
//! falls back to "every chat with any chat_items" — the existing full
//! load.
//!
//! The trade we accept: when ANY row in a chat changes — message edit,
//! reaction, attachment swap — every period bucket of that chat
//! re-renders, including buckets whose markdown bytes would have
//! ended up identical. In exchange we drop a few hundred lines of
//! bucket-fingerprint bookkeeping (the per-row `payload_blake3` column,
//! the `bucket_fingerprint_query` CTE, and the `prior_fingerprints`
//! plumbing through translate/render/orchestrator). The dolt prolly-
//! tree diff itself is timed on every run and logged in the render
//! cursor — see `translate::render`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{self, BlobBundle};
use frankweiler_etl::periodize::Period;
use frankweiler_signal_backup::backup;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

/// SQL projection from Signal's `chat_item_attachments` edge to its
/// CAS blake3. Consumed by [`BlobBundle::load`].
const ATTACHMENTS_PROJECTION_SQL: &str = "
    SELECT ref_id, blake3,
           NULL AS content_type, NULL AS upstream_name
      FROM chat_item_attachments
     WHERE ref_id IN ({placeholders}) AND blake3 IS NOT NULL";

/// Result of the dolt_diff scan: the chats we need to re-render, the
/// current HEAD hash to stamp into the next cursor, and how long the
/// diff query took. All three travel together so `render` can write
/// the cursor + log the elapsed_ms without a second round-trip.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// `Some(set)` → render only chats whose id is in `set`. `None` →
    /// cold start, render every chat. (First run, or no on-disk
    /// doltlite, or `dolt_diff_<table>` unavailable, or no prior
    /// cursor.)
    pub changed_chats: Option<HashSet<String>>,
    /// The HEAD commit hash at scan time, ready to stamp into the
    /// render cursor on success. `None` if we couldn't read HEAD —
    /// next run is another cold start.
    pub new_head: Option<String>,
    /// Wall-clock time spent in the `dolt_diff_<table>` union query.
    /// `None` on a cold start that didn't run the query (no cursor,
    /// nothing to diff against).
    pub scan_elapsed: Option<Duration>,
}

#[derive(Clone, Default)]
pub struct ParsedSignal {
    pub recipients: HashMap<String, ParsedRecipient>,
    /// Chats indexed by `chat_id` for lookup from `DocBucket`. The
    /// chats themselves carry no items — each item ends up in the
    /// matching bucket in `docs`.
    pub chats: HashMap<String, ParsedChat>,
    /// One bucket per `(chat_id, period_key)` pair that needs
    /// re-rendering. A chat survives Phase 1 iff `dolt_diff_*`
    /// reported any row of it as added/modified/removed since the
    /// last render cursor — every period of that chat is then
    /// loaded in Phase 2. Buckets whose chat didn't change are
    /// entirely absent from `docs`. Ordered by chat_id then
    /// period_key so the rendered tree is deterministic.
    pub docs: Vec<DocBucket>,
    /// Count of chats `dolt_diff` said were unchanged.
    pub docs_skipped: usize,
    /// Scan diagnostics propagated up to render so it can write the
    /// cursor + log elapsed_ms.
    pub scan: ScanResult,
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
#[derive(Debug, Clone, Default)]
pub struct DocBucket {
    pub chat_id: String,
    pub period_key: String,
    pub items: Vec<ParsedChatItem>,
    /// This bucket's attachment bytes, loaded in bulk by [`parse`]
    /// from `chat_item_attachments` + CAS in two SQL queries. Render
    /// walks it synchronously via [`BlobBundle::markdown_link`] and
    /// [`BlobBundle::materialize_to_dir`].
    pub blobs: BlobBundle,
}

#[derive(Debug, Clone)]
pub struct ParsedChatItem {
    /// `{chat_id}#{author_id}#{date_sent}` — matches the
    /// `chat_items.id` PK.
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

#[derive(Debug, Clone)]
pub struct ParsedAttachment {
    /// `local_media_name(plaintext_hash, local_key)` — the same key
    /// extract used when calling `db.store_blob`.
    pub ref_id: String,
    pub content_type: Option<String>,
    pub file_name: Option<String>,
    pub is_image: bool,
}

/// Compatibility wrapper for callers that don't have a render cursor
/// (older unit tests, ad-hoc repros). Forces a cold start — every
/// chat renders.
pub fn parse_raw_dir(input: &Path) -> Result<ParsedSignal> {
    parse(input, Period::Month, "signal", None)
}

/// Two-phase parse driven by `dolt_diff_<table>`.
///
/// Phase 1 — ask doltlite which chats changed since `last_render_hash`.
/// Cold start (`last_render_hash = None`) loads every chat; same path
/// also taken when doltlite extensions aren't linked.
///
/// Phase 2 — `SELECT … WHERE chat_id IN (?, …)` over `chat_items` for
/// the surviving chats only, build [`DocBucket`]s with one entry per
/// period the chat spans.
pub fn parse(
    input: &Path,
    period: Period,
    source_name: &str,
    last_render_hash: Option<&str>,
) -> Result<ParsedSignal> {
    let db_path = frankweiler_etl::doltlite_raw::db_path_for(input);
    if !db_path.is_file() {
        return Ok(ParsedSignal::default());
    }
    let _ = source_name; // currently unused; keep param for symmetry with whatsapp
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(async move { parse_async(&db_path, period, last_render_hash).await })
    })
}

async fn parse_async(
    db_path: &Path,
    period: Period,
    last_render_hash: Option<&str>,
) -> Result<ParsedSignal> {
    let opts =
        SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?.read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("open raw doltlite for translate at {}", db_path.display()))?;

    // Sibling CAS file holds attachment bytes.
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

    let recipients = load_recipients(&pool).await?;
    let chats = load_chats(&pool).await?;

    // ── Phase 1: which chats changed since last_render_hash? ──────
    let scan = scan_diff(&pool, last_render_hash).await?;

    // Decide the load set.
    let (to_load_chats, docs_skipped) = match &scan.changed_chats {
        None => {
            let ids = load_all_chat_ids(&pool).await?;
            (ids, 0usize)
        }
        Some(changed) => {
            let total = chats.len();
            let to_load: HashSet<String> = changed
                .iter()
                .filter(|cid| chats.contains_key(*cid))
                .cloned()
                .collect();
            let skipped = total.saturating_sub(to_load.len());
            (to_load, skipped)
        }
    };

    // ── Phase 2: targeted load for to_load_chats ──────────────────
    let mut docs = if to_load_chats.is_empty() {
        Vec::new()
    } else {
        load_buckets(&pool, period, &to_load_chats).await?
    };

    // Per-bucket BlobBundle: each bucket gets its own bag of
    // attachment bytes loaded in two queries. Render walks them
    // synchronously.
    if let Some(cas_pool) = cas_pool.as_ref() {
        for bucket in &mut docs {
            let mut seen: HashSet<String> = HashSet::new();
            let mut refs: Vec<&str> = Vec::new();
            for item in &bucket.items {
                for att in &item.attachments {
                    if seen.insert(att.ref_id.clone()) {
                        refs.push(att.ref_id.as_str());
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

    Ok(ParsedSignal {
        recipients,
        chats,
        docs,
        docs_skipped,
        scan,
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

async fn load_all_chat_ids(pool: &sqlx::SqlitePool) -> Result<HashSet<String>> {
    let rows = sqlx::query("SELECT DISTINCT chat_id FROM chat_items")
        .fetch_all(pool)
        .await
        .context("load all chat_ids")?;
    let mut out: HashSet<String> = HashSet::with_capacity(rows.len());
    for r in &rows {
        out.insert(r.try_get::<String, _>("chat_id")?);
    }
    Ok(out)
}

/// Phase 1: ask doltlite which chats touched any row since
/// `last_render_hash`. Returns the change set + current HEAD + the
/// wall-clock time the diff query took.
///
/// `recipients` changes propagate as "every chat needs re-render"
/// because rendered chat names dereference recipient display names —
/// a renamed recipient must repaint every chat they appear in, and we
/// don't keep the recipient→chats reverse index handy. Cheap and
/// correct.
async fn scan_diff(pool: &SqlitePool, last_render_hash: Option<&str>) -> Result<ScanResult> {
    let scan = frankweiler_etl::doltlite_raw::scan_buckets(
        pool,
        last_render_hash,
        &frankweiler_etl::doltlite_raw::DiffScanSpec {
            // Recipients fan out to every chat — the renderer
            // dereferences recipient display names per chat.
            global_fanout_tables: &["recipients"],
            bucket_query: "
                SELECT DISTINCT chat_id FROM (
                    SELECT coalesce(to_id, from_id) AS chat_id
                      FROM dolt_diff_chats
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_chat_id, from_chat_id)
                      FROM dolt_diff_chat_items
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                    UNION
                    -- Attachment changes propagate to their owning chat by
                    -- joining the diff vtab back to the live `chat_items`
                    -- table (which is at HEAD, the same ref the
                    -- surrounding diff queries are projecting to).
                    SELECT chat_items.chat_id
                      FROM dolt_diff_chat_item_attachments ca
                      JOIN chat_items
                        ON chat_items.id = coalesce(ca.to_chat_item_id, ca.from_chat_item_id)
                     WHERE ca.from_ref = ?1 AND ca.to_ref = 'HEAD'
                       AND ca.diff_type != 'unchanged'
                )
                WHERE chat_id IS NOT NULL
            ",
        },
    )
    .await?;
    Ok(ScanResult {
        changed_chats: scan.changed_buckets,
        new_head: scan.new_head,
        scan_elapsed: scan.scan_elapsed,
    })
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

/// Phase 2: pull `chat_items.payload` for the chats we decided to
/// render, decode the items, and shape into per-period `DocBucket`s.
async fn load_buckets(
    pool: &sqlx::SqlitePool,
    period: Period,
    chat_ids: &HashSet<String>,
) -> Result<Vec<DocBucket>> {
    if chat_ids.is_empty() {
        return Ok(Vec::new());
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
          ORDER BY chat_id, period_key, date_sent"
    );
    let mut q = sqlx::query(&sql);
    for c in chat_ids {
        q = q.bind(c);
    }
    let irows = q.fetch_all(pool).await.context("read chat_items")?;

    let mut bucket_idx: HashMap<(String, String), usize> = HashMap::new();
    let mut docs: Vec<DocBucket> = Vec::new();
    for r in &irows {
        let chat_id: String = r.try_get("chat_id")?;
        let author_id: String = r.try_get("author_id")?;
        let date_sent: i64 = r.try_get("date_sent")?;
        let period_key: String = r.try_get("period_key")?;
        let payload: String = r.try_get("payload")?;
        let key = (chat_id.clone(), period_key.clone());
        let idx = *bucket_idx.entry(key).or_insert_with(|| {
            docs.push(DocBucket {
                chat_id: chat_id.clone(),
                period_key: period_key.clone(),
                items: Vec::new(),
                blobs: BlobBundle::default(),
            });
            docs.len() - 1
        });
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
/// the fields we need to address its bytes in the CAS.
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
