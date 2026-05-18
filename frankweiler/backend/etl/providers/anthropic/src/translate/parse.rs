//! Port of `src/ingest/providers/anthropic/parse.py`. Reads a directory
//! laid out as `users.json` + `conversations.json` (+ optional
//! `projects/*.json`), matching what the live-API downloader writes
//! after `_normalize_to_export_shape()`. Flattens into typed rows;
//! `raw_json` carries the JSON minus any sibling rows we've exploded out.

use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde_json::{Map, Value};

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

#[derive(Debug, Default, Clone)]
pub struct ParsedExport {
    pub accounts: Vec<AccountRow>,
    pub projects: Vec<ProjectRow>,
    pub conversations: Vec<ConversationRow>,
    pub messages: Vec<MessageRow>,
    pub content_blocks: Vec<ContentBlockRow>,
    pub attachments: Vec<AttachmentRow>,
}

fn str_field(v: &Map<String, Value>, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(String::from)
}

pub fn parse_export(export_dir: &Path) -> Result<ParsedExport> {
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
        let Some(c_obj) = c.as_object() else { continue };
        let account_uuid = c_obj
            .get("account")
            .and_then(Value::as_object)
            .and_then(|a| a.get("uuid"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let conv_uuid =
            str_field(c_obj, "uuid").ok_or_else(|| anyhow!("conversation missing uuid"))?;
        let project_uuid = c_obj
            .get("project")
            .and_then(Value::as_object)
            .and_then(|p| p.get("uuid"))
            .and_then(Value::as_str)
            .map(String::from);

        let mut conv_raw = c_obj.clone();
        conv_raw.remove("chat_messages");
        out.conversations.push(ConversationRow {
            account_uuid,
            conversation_uuid: conv_uuid.clone(),
            project_uuid,
            name: str_field(c_obj, "name"),
            summary: str_field(c_obj, "summary"),
            created_at: str_field(c_obj, "created_at"),
            updated_at: str_field(c_obj, "updated_at"),
            raw_json: Value::Object(conv_raw),
        });

        let Some(msgs) = c_obj.get("chat_messages").and_then(Value::as_array) else {
            continue;
        };
        for m in msgs {
            let Some(m_obj) = m.as_object() else { continue };
            let mid = str_field(m_obj, "uuid").ok_or_else(|| anyhow!("message missing uuid"))?;
            let mut msg_raw = m_obj.clone();
            msg_raw.remove("content");
            msg_raw.remove("attachments");
            msg_raw.remove("files");
            out.messages.push(MessageRow {
                conversation_uuid: conv_uuid.clone(),
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
                    out.content_blocks.push(ContentBlockRow {
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
                    out.attachments.push(AttachmentRow {
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
                    out.attachments.push(AttachmentRow {
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

    Ok(out)
}
