//! Port of `src/ingest/providers/anthropic/parse.py`.
//!
//! Reads the doltlite raw store written by [`crate::download`] — and
//! only that. Both source types that share this renderer put their rows
//! in the same six tables: `claude_api` from the live API walk,
//! `claude_export` from [`crate::download::export`], which ingests an
//! unpacked bulk export. There is one input shape here, deliberately;
//! the second reader that used to walk an export tree in place is gone
//! (issue #207).
//!
//! `raw_json` carries the JSON minus any sibling rows we've exploded
//! out.

use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use datalib_etl::blob_cas::{self, BlobBundle};
use serde_json::{Map, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

use crate::download::db::{self, db_path_for, LoadedConversation, LoadedRaw};
use crate::download::normalize::normalize_to_export_shape;

/// SQL projection that maps an Anthropic `file_uuid` to its CAS
/// blake3. Consumed by [`BlobBundle::load`].
const ATTACHMENTS_PROJECTION_SQL: &str = "
    SELECT file_uuid AS ref_id, blake3,
           NULL AS content_type, NULL AS upstream_name
      FROM anthropic_attachments
     WHERE file_uuid IN ({placeholders}) AND blake3 IS NOT NULL";

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
    /// Owning Anthropic organization, same provenance as
    /// [`ConversationRow::org_uuid`] — a project lives in exactly one
    /// org and its grid rows carry that scope.
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    /// The project's custom instructions ("prompt template" upstream).
    /// Only the detail endpoint returns this; absent from the listing.
    pub prompt_template: Option<String>,
    pub is_starter: Option<bool>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub raw_json: Value,
    /// This project's knowledge documents, sorted by `(created_at, uuid)`.
    pub docs: Vec<ProjectDocRow>,
}

/// One knowledge document attached to a project. The text rides inline
/// in `content` — there is no separate fetch and nothing in the CAS.
#[derive(Debug, Clone)]
pub struct ProjectDocRow {
    pub project_uuid: String,
    pub doc_uuid: String,
    pub file_name: Option<String>,
    pub content: Option<String>,
    pub created_at: Option<String>,
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

/// One conversation as it sits between download and render: the upstream
/// JSON payload (full, normalized to export shape — used for
/// fingerprinting and for on-demand shredding into messages / content
/// blocks / attachments) paired with the surfaced [`ConversationRow`]
/// metadata.
///
/// Render is per-conversation: render fingerprints the payload,
/// skips it against the indexer's prior fingerprint, and only shreds
/// the `chat_messages` array when it has to render. That keeps the
/// steady-state render near-free for unchanged conversations.
#[derive(Debug, Clone)]
pub struct AnthropicConversation {
    pub conv: ConversationRow,
    pub upstream_payload: Value,
    /// This conversation's attachment bytes, loaded in bulk by
    /// [`parse`] (two SQL queries per conversation regardless of
    /// attachment count). Render walks it synchronously via
    /// [`BlobBundle::markdown_link`] and
    /// [`BlobBundle::materialize_to_dir`]. Empty when the conversation
    /// has no attachments or no doltlite db is present (legacy
    /// JSON-tree fixture).
    pub blobs: BlobBundle,
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
    /// `Some(set)` → render only the conversations and projects whose
    /// UUID is in `set`. `None` → cold start. Conversation and project
    /// UUIDs share one set because they are drawn from disjoint
    /// upstream id spaces, so a membership test can't confuse them.
    pub changed_buckets: Option<HashSet<String>>,
    pub new_head: Option<String>,
    pub scan_elapsed: Option<Duration>,
}

#[derive(Clone, Default)]
pub struct ParsedExport {
    pub accounts: Vec<AccountRow>,
    /// The projects to render this pass — narrowed by the diff scan,
    /// exactly like [`Self::conversations`].
    pub projects: Vec<ProjectRow>,
    /// `project_uuid → name` over **every** stored project, not just the
    /// changed ones. Conversations put their project's human name in the
    /// `project` grid column, and a conversation can be re-rendered in a
    /// pass where its project didn't change.
    pub project_name_by_uuid: std::collections::HashMap<String, String>,
    pub conversations: Vec<AnthropicConversation>,
    /// Count of docs (conversations + projects) `dolt_diff` reported as
    /// unchanged.
    pub docs_skipped: usize,
    pub scan: ScanResult,
}

fn str_field(v: &Map<String, Value>, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(String::from)
}

/// Two-phase parse driven by `dolt_diff_<table>`.
pub fn parse(path: &Path, last_render_hash: Option<&str>) -> Result<ParsedExport> {
    let db_path = db_path_for(path);
    if db_path.exists() {
        return parse_doltlite(&db_path, last_render_hash);
    }
    // No store: this source has never been downloaded. That is the
    // normal state of every source in a freshly scaffolded config, not
    // an error — render nothing and succeed. A store that exists but
    // can't be read still fails above. See docs/dev/step_protocol.md,
    // "Rendering a source with no data".
    Ok(ParsedExport::default())
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
        .with_context(|| format!("open anthropic doltlite for render {}", db_path.display()))?;

    let cas_path = blob_cas::cas_path_for(db_path);
    let cas_pool: Option<SqlitePool> = if cas_path.is_file() {
        let cas_opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", cas_path.display()))?
            .read_only(true);
        Some(
            SqlitePoolOptions::new()
                .max_connections(1)
                .acquire_timeout(Duration::from_secs(60))
                .connect_with(cas_opts)
                .await
                .with_context(|| format!("open anthropic CAS for render {}", cas_path.display()))?,
        )
    } else {
        None
    };

    let scan = scan_diff(&pool, last_render_hash).await?;

    // These three all read tables the download side also reads; the
    // single copy of each lives in `download::db` (users/orgs go
    // through the shared `doltlite_raw` helper). See "One reader per
    // table" there.
    let users = datalib_etl::doltlite_raw::load_payloads(&pool, "users").await?;
    let first_user_uuid = db::first_user_uuid_from(&pool).await?;
    let all_convs = db::load_conversations_from(&pool).await?;
    let total = all_convs.len();

    let (filtered, docs_skipped) = match &scan.changed_buckets {
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
    };

    let mut parsed = parse_loaded(raw);
    parsed.docs_skipped = docs_skipped;

    // Projects ride the same changed-bucket filter as conversations.
    // `project_name_by_uuid` is loaded unfiltered, though — an unchanged
    // conversation that *is* being re-rendered (because something else
    // in its bucket moved) still has to resolve its project's name.
    let all_projects = load_project_rows(&pool).await?;
    parsed.project_name_by_uuid = name_index(&all_projects);
    parsed.projects = match &scan.changed_buckets {
        None => all_projects,
        Some(changed) => {
            let before = all_projects.len();
            let kept: Vec<ProjectRow> = all_projects
                .into_iter()
                .filter(|p| changed.contains(&p.project_uuid))
                .collect();
            parsed.docs_skipped += before.saturating_sub(kept.len());
            kept
        }
    };
    parsed.scan = scan;

    // Per-doc BlobBundle: walk each conversation's
    // `chat_messages[*].files[*].file_uuid` and bulk-load the matching
    // edge-table rows + CAS bytes. Two SQL queries per conversation.
    if let Some(cas_pool) = cas_pool.as_ref() {
        for conv in &mut parsed.conversations {
            let refs = collect_attachment_ref_ids(&conv.upstream_payload);
            if refs.is_empty() {
                continue;
            }
            let ref_strs: Vec<&str> = refs.iter().map(String::as_str).collect();
            conv.blobs =
                BlobBundle::load(&pool, cas_pool, ATTACHMENTS_PROJECTION_SQL, &ref_strs).await?;
        }
    }

    Ok(parsed)
}

/// Walk one conversation's `chat_messages[*].files[*]` and enumerate
/// every `file_uuid` it references — the input set to
/// [`BlobBundle::load`].
fn collect_attachment_ref_ids(payload: &Value) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let Some(messages) = payload
        .as_object()
        .and_then(|o| o.get("chat_messages"))
        .and_then(Value::as_array)
    else {
        return out;
    };
    for msg in messages {
        let Some(files) = msg.get("files").and_then(Value::as_array) else {
            continue;
        };
        for f in files {
            if let Some(id) = f.get("file_uuid").and_then(Value::as_str) {
                if seen.insert(id.to_string()) {
                    out.push(id.to_string());
                }
            }
        }
    }
    out
}

/// Load every project out of the raw store and hang its knowledge
/// documents off it. Two queries total, not one per project.
async fn load_project_rows(pool: &SqlitePool) -> Result<Vec<ProjectRow>> {
    let projects = db::load_projects_from(pool).await?;
    if projects.is_empty() {
        return Ok(Vec::new());
    }
    let mut docs_by_project: std::collections::HashMap<String, Vec<ProjectDocRow>> =
        std::collections::HashMap::new();
    for d in db::load_project_docs_from(pool).await? {
        docs_by_project
            .entry(d.project_uuid.clone())
            .or_default()
            .push(project_doc_row(d.project_uuid.clone(), d.id, d.payload));
    }

    let mut out = Vec::with_capacity(projects.len());
    for p in projects {
        let mut docs = docs_by_project.remove(&p.id).unwrap_or_default();
        // Deterministic order: the render is a golden test, and the
        // upstream listing order is not promised to be stable.
        docs.sort_by(|a, b| {
            (a.created_at.as_deref().unwrap_or(""), a.doc_uuid.as_str())
                .cmp(&(b.created_at.as_deref().unwrap_or(""), b.doc_uuid.as_str()))
        });
        out.push(project_row(p.id, p.org_uuid, p.org_name, p.payload, docs));
    }
    out.sort_by(|a, b| a.project_uuid.cmp(&b.project_uuid));
    Ok(out)
}

/// `project_uuid → name` over the projects passed in.
///
/// **Must be built from every stored project, not from the narrowed
/// render set.** Conversations put their project's name in the
/// `project` grid column, and a conversation can be re-rendered in a
/// pass where its own project didn't change — feeding this the filtered
/// list would silently degrade those rows back to a bare UUID.
fn name_index(projects: &[ProjectRow]) -> std::collections::HashMap<String, String> {
    projects
        .iter()
        .filter_map(|p| Some((p.project_uuid.clone(), p.name.clone()?)))
        .collect()
}

/// Build a [`ProjectRow`] from one stored project payload.
fn project_row(
    project_uuid: String,
    org_uuid: Option<String>,
    org_name: Option<String>,
    payload: Value,
    docs: Vec<ProjectDocRow>,
) -> ProjectRow {
    let obj = payload.as_object().cloned().unwrap_or_default();
    let creator = obj
        .get("creator")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    ProjectRow {
        account_uuid: str_field(&creator, "uuid").unwrap_or_default(),
        project_uuid,
        org_uuid,
        org_name,
        name: str_field(&obj, "name"),
        description: str_field(&obj, "description"),
        prompt_template: str_field(&obj, "prompt_template"),
        is_starter: obj.get("is_starter_project").and_then(Value::as_bool),
        created_at: str_field(&obj, "created_at"),
        updated_at: str_field(&obj, "updated_at"),
        raw_json: payload,
        docs,
    }
}

/// Build a [`ProjectDocRow`] from one stored knowledge-doc payload.
fn project_doc_row(project_uuid: String, doc_uuid: String, payload: Value) -> ProjectDocRow {
    let obj = payload.as_object().cloned().unwrap_or_default();
    ProjectDocRow {
        project_uuid,
        doc_uuid,
        file_name: str_field(&obj, "file_name"),
        content: str_field(&obj, "content"),
        created_at: str_field(&obj, "created_at"),
        raw_json: payload,
    }
}

/// Phase 1: union over `dolt_diff_conversations`,
/// `dolt_diff_anthropic_attachments` and `dolt_diff_project_docs` to
/// project the changed bucket keys — conversation UUIDs from the first
/// two, project UUIDs from the third.
///
/// `users`, `orgs` and `projects` fan out to "render everything":
/// rendered docs dereference those names in frontmatter, grid columns
/// and page titles, so a rename has to repaint every doc in the
/// affected scope. `projects` is on that list because every
/// conversation's `project` grid column carries its project's *name* —
/// a rename that only repainted the project's own page would leave
/// every conversation in it showing the old label.
///
/// `project_docs` deliberately is **not** a fanout table: editing a
/// knowledge document changes that project's page and nothing else, and
/// it is the one project-side write that happens with any regularity.
async fn scan_diff(pool: &SqlitePool, last_render_hash: Option<&str>) -> Result<ScanResult> {
    let scan = datalib_etl::doltlite_raw::scan_buckets(
        pool,
        last_render_hash,
        &datalib_etl::doltlite_raw::DiffScanSpec {
            global_fanout_tables: &["users", "orgs", "projects"],
            bucket_query: "
                SELECT DISTINCT bucket_uuid FROM (
                    SELECT coalesce(to_id, from_id) AS bucket_uuid
                      FROM dolt_diff_conversations
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_conversation_uuid, from_conversation_uuid)
                      FROM dolt_diff_anthropic_attachments
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_project_uuid, from_project_uuid)
                      FROM dolt_diff_project_docs
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                )
                WHERE bucket_uuid IS NOT NULL
            ",
        },
    )
    .await?;
    Ok(ScanResult {
        changed_buckets: scan.changed_buckets,
        new_head: scan.new_head,
        scan_elapsed: scan.scan_elapsed,
    })
}

/// Build a [`ParsedExport`] from a snapshot already loaded out of the
/// doltlite DB.
///
/// A conversation the **API** walk stored holds the raw `/api/...`
/// response, so it gets normalized into export shape here (the step
/// that used to happen at fetch time). A conversation the **export**
/// ingest stored is already in export shape and must not be
/// normalized: doing so would stamp it `_source: {via: "claude.ai/api",
/// org_uuid: ""}`, which is both a lie about where it came from and an
/// empty org on every one of its grid rows.
///
/// The two are told apart by the `org_uuid` column, which only the API
/// walk can fill — see [`crate::download::db::LoadedConversation`].
pub fn parse_loaded(raw: crate::download::db::LoadedRaw) -> ParsedExport {
    let mut out = ParsedExport::default();
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
        let normalized = match org_uuid {
            Some(org) => {
                normalize_to_export_shape(payload, account_uuid, &org, org_name.as_deref())
            }
            None => payload,
        };
        match build_conv_row(&normalized) {
            Ok(Some(conv)) => out.conversations.push(AnthropicConversation {
                conv,
                upstream_payload: normalized,
                blobs: BlobBundle::default(),
            }),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(event = "anthropic_build_conv_failed", error = %e);
            }
        }
    }
    out
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

#[cfg(test)]
mod no_data_tests {
    use super::*;

    /// A source that has never been downloaded renders as empty, not
    /// as a failure: that is the normal state of every source in a
    /// freshly scaffolded config. See docs/dev/step_protocol.md,
    /// "Rendering a source with no data".
    #[test]
    fn parse_missing_source_returns_empty_silently() {
        let parsed = parse(Path::new("/this/does/not/exist"), None).unwrap();
        assert!(parsed.conversations.is_empty());
        assert!(parsed.accounts.is_empty());
        assert!(parsed.projects.is_empty());
    }
}
