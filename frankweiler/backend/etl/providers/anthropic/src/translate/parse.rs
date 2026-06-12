//! Port of `src/ingest/providers/anthropic/parse.py`.
//!
//! Reads either the doltlite raw store written by [`crate::extract`]
//! (the production path), normalizing each conversation into export
//! shape at read time, or the legacy JSON tree (`users.json` +
//! `conversations.json` [+ optional `projects/*.json`]) used by the
//! in-crate render fixture test.
//!
//! `raw_json` carries the JSON minus any sibling rows we've exploded
//! out.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use frankweiler_etl::blob_cas::{self, BlobReader, InMemoryBlobReader};
use serde_json::{Map, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use crate::extract::db::{db_path_for, LoadedConversation, LoadedRaw};
use crate::extract::normalize::normalize_to_export_shape;

#[derive(Debug, Clone)]
pub struct AccountRow {
    pub account_uuid: String,
    pub email: Option<String>,
    pub full_name: Option<String>,
    pub raw_json: Value,
}

#[derive(Debug, Clone)]
pub struct ProjectRow {
    pub account_uuid: String,
    pub project_uuid: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub is_starter: Option<bool>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub raw_json: Value,
}

#[derive(Debug, Clone)]
pub struct ConversationRow {
    pub account_uuid: String,
    pub conversation_uuid: String,
    pub project_uuid: Option<String>,
    /// Owning Anthropic organization UUID, lifted from `_source.org_uuid`
    /// in the normalized payload. Used to disambiguate conversations
    /// that share an account but live in different orgs (e.g. personal
    /// Max plan vs. a Team-plan workspace).
    pub org_uuid: Option<String>,
    /// Human-readable org name, when available (from `_source.org_name`).
    pub org_name: Option<String>,
    pub name: Option<String>,
    pub summary: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub raw_json: Value,
}

#[derive(Debug, Clone)]
pub struct MessageRow {
    pub conversation_uuid: String,
    pub message_uuid: String,
    pub parent_message_uuid: Option<String>,
    pub sender: Option<String>,
    pub text: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub raw_json: Value,
}

#[derive(Debug, Clone)]
pub struct ContentBlockRow {
    pub message_uuid: String,
    pub block_index: usize,
    pub r#type: Option<String>,
    pub text: Option<String>,
    pub start_timestamp: Option<String>,
    pub stop_timestamp: Option<String>,
    pub raw_json: Value,
}

#[derive(Debug, Clone)]
pub struct AttachmentRow {
    pub message_uuid: String,
    pub attachment_index: usize,
    /// "attachment" or "file"
    pub kind: String,
    pub raw_json: Value,
}

/// One conversation as it sits between extract and render: the upstream
/// JSON payload (full, normalized to export shape — used for
/// fingerprinting and for on-demand shredding into messages / content
/// blocks / attachments) paired with the surfaced [`ConversationRow`]
/// metadata.
///
/// Translate is per-conversation: render fingerprints the payload,
/// skips it against the indexer's prior fingerprint, and only shreds
/// the `chat_messages` array when it has to render. That keeps the
/// steady-state translate near-free for unchanged conversations.
#[derive(Debug, Clone)]
pub struct AnthropicConversation {
    pub conv: ConversationRow,
    pub upstream_payload: Value,
}

/// Shredded form of one conversation. Built by [`shred`] only for
/// conversations that have actually changed (or are being rendered for
/// the first time).
#[derive(Debug, Clone)]
pub struct ShreddedConversation {
    pub conv: ConversationRow,
    pub messages: Vec<MessageRow>,
    pub content_blocks: Vec<ContentBlockRow>,
    pub attachments: Vec<AttachmentRow>,
}

/// Result of the dolt_diff scan. Travels alongside the parsed bag so
/// render can advance the cursor + log timing without a second
/// round-trip.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// `Some(set)` → render only conversations whose UUID is in
    /// `set`. `None` → cold start.
    pub changed_conversations: Option<HashSet<String>>,
    pub new_head: Option<String>,
    pub scan_elapsed: Option<Duration>,
}

#[derive(Clone)]
pub struct ParsedExport {
    pub accounts: Vec<AccountRow>,
    pub projects: Vec<ProjectRow>,
    pub conversations: Vec<AnthropicConversation>,
    /// Count of conversations `dolt_diff` reported as unchanged.
    pub docs_skipped: usize,
    pub scan: ScanResult,
    /// Streaming handle to blob bytes, keyed by upstream `file_uuid`.
    pub blobs: Arc<dyn BlobReader>,
}

impl Default for ParsedExport {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            projects: Vec::new(),
            conversations: Vec::new(),
            docs_skipped: 0,
            scan: ScanResult::default(),
            blobs: InMemoryBlobReader::empty_handle(),
        }
    }
}

fn str_field(v: &Map<String, Value>, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(String::from)
}

/// Cold-start entry point: no render cursor, render everything.
/// Kept for the in-crate JSON-tree fixture used by `anthropic_render`
/// and similar tests.
pub fn parse_export(path: &Path) -> Result<ParsedExport> {
    parse(path, None)
}

/// Two-phase parse driven by `dolt_diff_<table>`.
pub fn parse(path: &Path, last_render_hash: Option<&str>) -> Result<ParsedExport> {
    let db_path = db_path_for(path);
    if db_path.exists() {
        return parse_doltlite(&db_path, last_render_hash);
    }
    if path.is_dir() {
        return parse_export_json_dir(path);
    }
    Err(anyhow!(
        "anthropic source not found at {} (no .doltlite_db, no JSON tree)",
        path.display()
    ))
}

fn parse_doltlite(db_path: &Path, last_render_hash: Option<&str>) -> Result<ParsedExport> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(async move { parse_doltlite_async(db_path, last_render_hash).await })
    })
}

async fn parse_doltlite_async(
    db_path: &Path,
    last_render_hash: Option<&str>,
) -> Result<ParsedExport> {
    let opts =
        SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?.read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(60))
        .connect_with(opts)
        .await
        .with_context(|| {
            format!(
                "open anthropic doltlite for translate {}",
                db_path.display()
            )
        })?;

    let cas_path = blob_cas::cas_path_for(db_path);
    let blobs: Arc<dyn BlobReader> = if cas_path.is_file() {
        let cas_opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", cas_path.display()))?
            .read_only(true);
        let cas_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(60))
            .connect_with(cas_opts)
            .await
            .with_context(|| format!("open anthropic CAS for translate {}", cas_path.display()))?;
        super::blob_reader::AnthropicBlobReader::new(pool.clone(), cas_pool).into_handle()
    } else {
        InMemoryBlobReader::empty_handle()
    };

    let scan = scan_diff(&pool, last_render_hash).await?;

    let users = load_payloads(&pool, "users").await?;
    let first_user_uuid = load_first_user_uuid(&pool).await?;
    let all_convs = load_conversations(&pool).await?;
    let total = all_convs.len();

    let (filtered, docs_skipped) = match &scan.changed_conversations {
        None => (all_convs, 0usize),
        Some(changed) => {
            let kept: Vec<LoadedConversation> = all_convs
                .into_iter()
                .filter(|c| changed.contains(&c.id))
                .collect();
            let skipped = total.saturating_sub(kept.len());
            (kept, skipped)
        }
    };

    let raw = LoadedRaw {
        users,
        first_user_uuid,
        conversations: filtered,
        blobs,
    };

    let mut parsed = parse_loaded(raw);
    parsed.docs_skipped = docs_skipped;
    parsed.scan = scan;
    Ok(parsed)
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

async fn load_first_user_uuid(pool: &SqlitePool) -> Result<Option<String>> {
    let row = sqlx::query("SELECT id FROM users ORDER BY id LIMIT 1")
        .fetch_optional(pool)
        .await
        .context("first_user_uuid")?;
    Ok(row.and_then(|r| r.try_get::<String, _>("id").ok()))
}

async fn load_conversations(pool: &SqlitePool) -> Result<Vec<LoadedConversation>> {
    let rows = sqlx::query(
        "SELECT id, org_uuid, org_name, json(payload) AS payload FROM conversations \
          WHERE payload IS NOT NULL ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .context("load_conversations")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let id: String = r.try_get("id").unwrap_or_default();
        let org_uuid: Option<String> = r.try_get("org_uuid").ok();
        let org_name: Option<String> = r.try_get("org_name").ok();
        let Ok(s) = r.try_get::<String, _>("payload") else {
            continue;
        };
        let Ok(p) = serde_json::from_str::<Value>(&s) else {
            continue;
        };
        out.push(LoadedConversation {
            id,
            org_uuid: org_uuid.unwrap_or_default(),
            org_name,
            payload: p,
        });
    }
    Ok(out)
}

/// Phase 1: union over `dolt_diff_conversations` +
/// `dolt_diff_anthropic_attachments` to project changed conversation
/// UUIDs. Any change to `users` or `orgs` fans out to "render
/// everything" — rendered conversations dereference those names in
/// frontmatter / path slugs, so a rename has to repaint every doc in
/// the affected scope.
async fn scan_diff(pool: &SqlitePool, last_render_hash: Option<&str>) -> Result<ScanResult> {
    let new_head: Option<String> =
        sqlx::query_scalar("SELECT commit_hash FROM dolt_log() ORDER BY date DESC LIMIT 1")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
    let Some(from_ref) = last_render_hash else {
        return Ok(ScanResult {
            changed_conversations: None,
            new_head,
            scan_elapsed: None,
        });
    };

    // Fan-out triggers.
    for table in ["dolt_diff_users", "dolt_diff_orgs"] {
        let sql = format!(
            "SELECT 1 FROM {table} \
              WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged' LIMIT 1"
        );
        let any: Option<i64> = sqlx::query_scalar(&sql)
            .bind(from_ref)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
        if any.is_some() {
            return Ok(ScanResult {
                changed_conversations: None,
                new_head,
                scan_elapsed: None,
            });
        }
    }

    let sql = "
        SELECT DISTINCT conversation_uuid FROM (
            SELECT coalesce(to_id, from_id) AS conversation_uuid
              FROM dolt_diff_conversations
             WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
            UNION
            SELECT coalesce(to_conversation_uuid, from_conversation_uuid)
              FROM dolt_diff_anthropic_attachments
             WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
        )
        WHERE conversation_uuid IS NOT NULL
    ";
    let started = std::time::Instant::now();
    let res = sqlx::query(sql).bind(from_ref).fetch_all(pool).await;
    let elapsed = started.elapsed();
    let rows = match res {
        Ok(rows) => rows,
        Err(e) => {
            tracing::info!(
                source = "anthropic",
                error = %e,
                "dolt_diff scan failed — falling back to cold-start (render everything)"
            );
            return Ok(ScanResult {
                changed_conversations: None,
                new_head,
                scan_elapsed: Some(elapsed),
            });
        }
    };
    let set: HashSet<String> = rows.iter().map(|r| r.get::<String, _>(0)).collect();
    Ok(ScanResult {
        changed_conversations: Some(set),
        new_head,
        scan_elapsed: Some(elapsed),
    })
}

/// Build a [`ParsedExport`] from a snapshot already loaded out of the
/// doltlite DB. Each conversation is normalized into export shape (the
/// same step that used to happen at fetch time) before being walked.
pub fn parse_loaded(raw: crate::extract::db::LoadedRaw) -> ParsedExport {
    let mut out = ParsedExport {
        blobs: raw.blobs,
        ..Default::default()
    };
    for u in &raw.users {
        let Some(obj) = u.as_object() else { continue };
        let Some(uuid) = str_field(obj, "uuid") else {
            continue;
        };
        out.accounts.push(AccountRow {
            account_uuid: uuid,
            email: str_field(obj, "email_address"),
            full_name: str_field(obj, "full_name"),
            raw_json: u.clone(),
        });
    }
    let account_uuid = raw.first_user_uuid.as_deref();
    for LoadedConversation {
        id: _,
        org_uuid,
        org_name,
        payload,
    } in raw.conversations
    {
        let normalized =
            normalize_to_export_shape(payload, account_uuid, &org_uuid, org_name.as_deref());
        match build_conv_row(&normalized) {
            Ok(Some(conv)) => out.conversations.push(AnthropicConversation {
                conv,
                upstream_payload: normalized,
            }),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(event = "anthropic_build_conv_failed", error = %e);
            }
        }
    }
    out
}

/// Legacy fallback: walk a `users.json` / `conversations.json` /
/// `projects/*.json` tree. Kept around for the in-crate fixture used
/// by `tests/anthropic_render.rs`.
pub fn parse_export_json_dir(export_dir: &Path) -> Result<ParsedExport> {
    let mut out = ParsedExport::default();

    let users_path = export_dir.join("users.json");
    if !users_path.exists() {
        return Err(anyhow!("missing users.json in {}", export_dir.display()));
    }
    let users: Value = serde_json::from_str(&fs::read_to_string(&users_path)?)
        .with_context(|| format!("parsing {}", users_path.display()))?;
    let Value::Array(users_arr) = users else {
        return Err(anyhow!("users.json must be a list"));
    };
    for u in users_arr {
        let obj = u
            .as_object()
            .ok_or_else(|| anyhow!("user entry must be an object"))?;
        out.accounts.push(AccountRow {
            account_uuid: str_field(obj, "uuid").ok_or_else(|| anyhow!("user missing uuid"))?,
            email: str_field(obj, "email_address"),
            full_name: str_field(obj, "full_name"),
            raw_json: u.clone(),
        });
    }

    let projects_dir = export_dir.join("projects");
    if projects_dir.is_dir() {
        let mut files: Vec<_> = fs::read_dir(&projects_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();
        files.sort();
        for f in files {
            let p: Value = serde_json::from_str(&fs::read_to_string(&f)?)
                .with_context(|| format!("parsing {}", f.display()))?;
            let Some(obj) = p.as_object() else { continue };
            let creator = obj
                .get("creator")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            out.projects.push(ProjectRow {
                account_uuid: str_field(&creator, "uuid").unwrap_or_default(),
                project_uuid: str_field(obj, "uuid")
                    .ok_or_else(|| anyhow!("project missing uuid"))?,
                name: str_field(obj, "name"),
                description: str_field(obj, "description"),
                is_starter: obj.get("is_starter_project").and_then(Value::as_bool),
                created_at: str_field(obj, "created_at"),
                updated_at: str_field(obj, "updated_at"),
                raw_json: p.clone(),
            });
        }
    }

    let convs_path = export_dir.join("conversations.json");
    if !convs_path.exists() {
        return Err(anyhow!(
            "missing conversations.json in {}",
            export_dir.display()
        ));
    }
    let convs: Value = serde_json::from_str(&fs::read_to_string(&convs_path)?)
        .with_context(|| format!("parsing {}", convs_path.display()))?;
    let Value::Array(convs_arr) = convs else {
        return Err(anyhow!("conversations.json must be a list"));
    };
    for c in convs_arr {
        match build_conv_row(&c) {
            Ok(Some(conv)) => out.conversations.push(AnthropicConversation {
                conv,
                upstream_payload: c,
            }),
            Ok(None) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// Build the [`ConversationRow`] metadata for one fully-normalized
/// conversation payload. Returns `Ok(None)` if `c` isn't a JSON object.
/// The conversation's `chat_messages` (containing every message +
/// content block + attachment) is *not* walked here — that work is
/// deferred to [`shred`] so unchanged conversations never pay it.
pub fn build_conv_row(c: &Value) -> Result<Option<ConversationRow>> {
    let Some(c_obj) = c.as_object() else {
        return Ok(None);
    };
    let account_uuid = c_obj
        .get("account")
        .and_then(Value::as_object)
        .and_then(|a| a.get("uuid"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let conv_uuid = str_field(c_obj, "uuid").ok_or_else(|| anyhow!("conversation missing uuid"))?;
    let project_uuid = c_obj
        .get("project")
        .and_then(Value::as_object)
        .and_then(|p| p.get("uuid"))
        .and_then(Value::as_str)
        .map(String::from);

    let source = c_obj.get("_source").and_then(Value::as_object);
    let org_uuid = source
        .and_then(|s| s.get("org_uuid"))
        .and_then(Value::as_str)
        .map(String::from);
    let org_name = source
        .and_then(|s| s.get("org_name"))
        .and_then(Value::as_str)
        .map(String::from);

    let mut conv_raw = c_obj.clone();
    conv_raw.remove("chat_messages");
    Ok(Some(ConversationRow {
        account_uuid,
        conversation_uuid: conv_uuid,
        project_uuid,
        org_uuid,
        org_name,
        name: str_field(c_obj, "name"),
        summary: str_field(c_obj, "summary"),
        created_at: str_field(c_obj, "created_at"),
        updated_at: str_field(c_obj, "updated_at"),
        raw_json: Value::Object(conv_raw),
    }))
}

/// Walk a conversation's `chat_messages` array and emit its messages,
/// content blocks, and attachments. Only called for conversations the
/// renderer is actually going to re-render — for unchanged
/// conversations the fingerprint check short-circuits and we never
/// visit the array at all.
pub fn shred(c: &AnthropicConversation) -> ShreddedConversation {
    let mut messages = Vec::new();
    let mut content_blocks = Vec::new();
    let mut attachments = Vec::new();
    let cid = c.conv.conversation_uuid.as_str();

    if let Some(msgs) = c
        .upstream_payload
        .as_object()
        .and_then(|o| o.get("chat_messages"))
        .and_then(Value::as_array)
    {
        for m in msgs {
            let Some(m_obj) = m.as_object() else { continue };
            let Some(mid) = str_field(m_obj, "uuid") else {
                // Missing uuid — skip rather than panic; build_conv_row
                // succeeded so the rest of the chat still renders.
                continue;
            };
            let mut msg_raw = m_obj.clone();
            msg_raw.remove("content");
            msg_raw.remove("attachments");
            msg_raw.remove("files");
            messages.push(MessageRow {
                conversation_uuid: cid.to_string(),
                message_uuid: mid.clone(),
                parent_message_uuid: str_field(m_obj, "parent_message_uuid"),
                sender: str_field(m_obj, "sender"),
                text: str_field(m_obj, "text"),
                created_at: str_field(m_obj, "created_at"),
                updated_at: str_field(m_obj, "updated_at"),
                raw_json: Value::Object(msg_raw),
            });

            if let Some(content) = m_obj.get("content").and_then(Value::as_array) {
                for (i, blk) in content.iter().enumerate() {
                    let blk_obj = blk.as_object();
                    content_blocks.push(ContentBlockRow {
                        message_uuid: mid.clone(),
                        block_index: i,
                        r#type: blk_obj.and_then(|o| str_field(o, "type")),
                        text: blk_obj.and_then(|o| str_field(o, "text")),
                        start_timestamp: blk_obj.and_then(|o| str_field(o, "start_timestamp")),
                        stop_timestamp: blk_obj.and_then(|o| str_field(o, "stop_timestamp")),
                        raw_json: blk.clone(),
                    });
                }
            }
            let mut atch_idx = 0usize;
            if let Some(atch) = m_obj.get("attachments").and_then(Value::as_array) {
                for a in atch {
                    attachments.push(AttachmentRow {
                        message_uuid: mid.clone(),
                        attachment_index: atch_idx,
                        kind: "attachment".into(),
                        raw_json: a.clone(),
                    });
                    atch_idx += 1;
                }
            }
            if let Some(files) = m_obj.get("files").and_then(Value::as_array) {
                for f in files {
                    attachments.push(AttachmentRow {
                        message_uuid: mid.clone(),
                        attachment_index: atch_idx,
                        kind: "file".into(),
                        raw_json: f.clone(),
                    });
                    atch_idx += 1;
                }
            }
        }
    }

    ShreddedConversation {
        conv: c.conv.clone(),
        messages,
        content_blocks,
        attachments,
    }
}
