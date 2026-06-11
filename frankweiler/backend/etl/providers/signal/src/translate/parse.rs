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
    /// One bucket per `(chat_id, period_key)` pair the run produced,
    /// ordered by chat_id then period_key so the rendered tree is
    /// deterministic.
    pub docs: Vec<DocBucket>,
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
/// period key (`2024-03`, `2024-03-15`, `2024`, or `all`). Drives
/// fingerprinting + the per-bucket .md file.
#[derive(Debug, Clone)]
pub struct DocBucket {
    pub chat_id: String,
    pub period_key: String,
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
/// default the YAML knob falls back to.
pub fn parse_raw_dir(input: &Path) -> Result<ParsedSignal> {
    parse(input, Period::Month)
}

pub fn parse(input: &Path, period: Period) -> Result<ParsedSignal> {
    let db_path = frankweiler_etl::doltlite_raw::db_path_for(input);
    if !db_path.is_file() {
        return Ok(ParsedSignal::default());
    }
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(async move { parse_async(&db_path, period).await })
    })
}

async fn parse_async(db_path: &Path, period: Period) -> Result<ParsedSignal> {
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
    // When the CAS file doesn't exist (e.g. extract ran before any
    // attachments landed), fall back to an empty in-memory reader so
    // render can still emit "(attachment not yet fetched)"
    // placeholders.
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

    // ── recipients ─────────────────────────────────────────────────
    let mut recipients: HashMap<String, ParsedRecipient> = HashMap::new();
    let rrows = sqlx::query("SELECT id, identifier, display_name FROM recipients")
        .fetch_all(&pool)
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

    // ── chats ──────────────────────────────────────────────────────
    let crows = sqlx::query("SELECT id, recipient_id FROM chats ORDER BY id")
        .fetch_all(&pool)
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

    // ── chat items, bucketed by (chat_id, period_key) ──────────────
    //
    // Bucketing happens in Rust (not SQL) because each item's text +
    // direction live inside the JSON payload; we have to parse it
    // anyway, and the date_sent we use for the period key is already
    // promoted to its own column. Single scan over chat_items;
    // bucket lookup is HashMap on the period key. The `json(payload)`
    // wrapper unwraps JSONB to text so `serde_json::from_str` can
    // parse it.
    let irows = sqlx::query(
        "SELECT chat_id, author_id, date_sent, json(payload) AS payload \
         FROM chat_items ORDER BY chat_id, date_sent",
    )
    .fetch_all(&pool)
    .await
    .context("read chat_items")?;

    let mut bucket_idx: HashMap<(String, String), usize> = HashMap::new();
    let mut docs: Vec<DocBucket> = Vec::new();
    for r in &irows {
        let chat_id: String = r.try_get("chat_id")?;
        let author_id: String = r.try_get("author_id")?;
        let date_sent: i64 = r.try_get("date_sent")?;
        let payload: String = r.try_get("payload")?;
        let item_pk =
            crate::extract::schema_raw::chat_item_id_recipe(&chat_id, &author_id, date_sent);
        let (text, outgoing, attachments) = decode_chat_item(&payload);
        let period_key = period.key_for_ms(date_sent);
        let key = (chat_id.clone(), period_key.clone());
        let idx = *bucket_idx.entry(key).or_insert_with(|| {
            docs.push(DocBucket {
                chat_id,
                period_key,
                items: Vec::new(),
            });
            docs.len() - 1
        });
        docs[idx].items.push(ParsedChatItem {
            item_pk,
            author_id,
            date_sent,
            text,
            outgoing,
            attachments,
        });
    }

    Ok(ParsedSignal {
        recipients,
        chats,
        docs,
        blobs,
    })
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
