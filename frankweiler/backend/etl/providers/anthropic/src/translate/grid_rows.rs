//! Port of the `_anthropic_*` helpers + per-conversation row emit loop
//! from `src/ingest/grid_rows.py`. Produces a Chat row, one row per
//! message, plus a row per `thinking` / `tool_use` / `tool_result`
//! block — same row set the Python ingest emits.

use frankweiler_schema::grid_rows::GridRow;
use serde_json::Value;

use super::parse::{ContentBlockRow, ConversationRow, MessageRow, ShreddedConversation};
use super::render::section_uuid_for_block;

// Bumped from 1 to 2 when the section-uuid scheme replaced
// `{msg_uuid}:{block_index}` for tool_use / tool_result / thinking
// rows. Old sidecars need a rebake to pick up the new uuids; bumping
// the version is the trigger for that.
pub const RENDER_VERSION: u32 = 2;

fn kind_for_sender(sender: &str) -> &'static str {
    match sender.to_ascii_lowercase().as_str() {
        "human" | "user" => "User Input",
        "assistant" => "LLM Response",
        _ => "Tool Call",
    }
}

fn kind_for_block(block_type: &str) -> &'static str {
    if block_type == "thinking" {
        "LLM Thinking"
    } else {
        "Tool Call"
    }
}

fn bump_micros(ts: &str, n: i64) -> String {
    frankweiler_time::bump_micros_str(ts, n).unwrap_or_else(|| ts.into())
}

fn qmd_path(account_uuid: &str, org_uuid: &str, conv_uuid: &str) -> String {
    // Page-dir layout: `<conv_uuid>/index.md` under
    // `<account>/<org>/llm_chats/`. Matches `Rendered::relative_path`
    // in `render.rs`. The org segment lets two conversations from the
    // same logged-in account but different orgs (e.g. personal Max
    // plan vs. a Team-plan workspace) render to disjoint paths.
    format!("rendered_md/anthropic/{account_uuid}/{org_uuid}/llm_chats/{conv_uuid}/index.md")
}

fn org_uuid_for_path(conv: &ConversationRow) -> &str {
    conv.org_uuid.as_deref().unwrap_or("unknown-org")
}

pub fn rows_for_conversation(shredded: &ShreddedConversation) -> Vec<GridRow> {
    let conv = &shredded.conv;
    let conv_uuid = conv.conversation_uuid.as_str();
    let model = conv
        .raw_json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut rows = Vec::new();
    rows.push(chat_row(conv));

    let mut msgs: Vec<&MessageRow> = shredded.messages.iter().collect();
    msgs.sort_by(|a, b| {
        let ka = (
            a.created_at.as_deref().unwrap_or(""),
            a.message_uuid.as_str(),
        );
        let kb = (
            b.created_at.as_deref().unwrap_or(""),
            b.message_uuid.as_str(),
        );
        ka.cmp(&kb)
    });

    let mut blocks_by_msg: std::collections::HashMap<&str, Vec<&ContentBlockRow>> =
        std::collections::HashMap::new();
    for b in &shredded.content_blocks {
        blocks_by_msg.entry(&b.message_uuid).or_default().push(b);
    }

    for (msg_idx, m) in msgs.iter().enumerate() {
        let kind = kind_for_sender(m.sender.as_deref().unwrap_or(""));
        let author = match kind {
            "User Input" => Some(conv.account_uuid.clone()),
            "LLM Response" => Some(model.clone())
                .filter(|s| !s.is_empty())
                .or_else(|| m.sender.clone()),
            _ => m.sender.clone(),
        };

        let mut blocks: Vec<&ContentBlockRow> = blocks_by_msg
            .get(m.message_uuid.as_str())
            .cloned()
            .unwrap_or_default();
        blocks.sort_by_key(|b| b.block_index);

        let text_parts: Vec<&str> = blocks
            .iter()
            .filter(|b| b.r#type.as_deref() == Some("text"))
            .filter_map(|b| b.text.as_deref())
            .filter(|s| !s.is_empty())
            .collect();
        let row_text = if !text_parts.is_empty() {
            text_parts.join("\n\n")
        } else {
            m.text.clone().unwrap_or_default()
        };

        rows.push(GridRow {
            uuid: m.message_uuid.clone(),
            provider: "anthropic".into(),
            kind: kind.into(),
            source_label: "Claude".into(),
            when_ts: m.created_at.clone(),
            author,
            account: Some(conv.account_uuid.clone()),
            org_uuid: conv.org_uuid.clone(),
            org_name: conv.org_name.clone(),
            project: conv.project_uuid.clone(),
            channel: None,
            conversation_name: conv.name.clone(),
            conversation_uuid: conv_uuid.into(),
            message_index: Some(msg_idx as i64),
            entire_chat: format!("/chat/{conv_uuid}"),
            text: row_text,
            slack_link: None,
            qmd_path: Some(qmd_path(
                &conv.account_uuid,
                org_uuid_for_path(conv),
                conv_uuid,
            )),
            source_url: None,
            git_sha: None,
            external_id: None,
            notion_page_uuid: None,
            notion_block_uuid: None,
            markdown_uuid: Some(conv_uuid.into()),
        });

        for b in blocks {
            let btype = b.r#type.as_deref().unwrap_or("");
            if !matches!(btype, "tool_use" | "tool_result" | "thinking") {
                continue;
            }
            let mut btext = b.text.clone().unwrap_or_default();
            if btext.is_empty() && btype == "thinking" {
                if let Some(t) = b.raw_json.get("thinking").and_then(Value::as_str) {
                    btext = t.into();
                }
            }
            let row_when: Option<String> = b
                .start_timestamp
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    m.created_at
                        .as_deref()
                        .map(|t| bump_micros(t, (b.block_index + 1) as i64))
                });
            let row_author = if !model.is_empty() {
                model.clone()
            } else {
                btype.to_string()
            };
            let raw_obj = b.raw_json.as_object().cloned().unwrap_or_default();
            let row_uuid =
                section_uuid_for_block(&m.message_uuid, b.block_index, Some(btype), &raw_obj)
                    // section_uuid_for_block returns None only for `text`,
                    // which the outer `matches!` above already excluded; the
                    // synthetic fallback is purely defensive.
                    .unwrap_or_else(|| format!("blk-{}-{}", m.message_uuid, b.block_index));
            rows.push(GridRow {
                uuid: row_uuid,
                provider: "anthropic".into(),
                kind: kind_for_block(btype).into(),
                source_label: "Claude".into(),
                when_ts: row_when,
                author: Some(row_author),
                account: Some(conv.account_uuid.clone()),
                org_uuid: conv.org_uuid.clone(),
                org_name: conv.org_name.clone(),
                project: conv.project_uuid.clone(),
                channel: None,
                conversation_name: conv.name.clone(),
                conversation_uuid: conv_uuid.into(),
                message_index: Some(msg_idx as i64),
                entire_chat: format!("/chat/{conv_uuid}"),
                text: if btext.is_empty() {
                    btype.into()
                } else {
                    btext
                },
                slack_link: None,
                qmd_path: Some(qmd_path(
                    &conv.account_uuid,
                    org_uuid_for_path(conv),
                    conv_uuid,
                )),
                source_url: None,
                git_sha: None,
                external_id: None,
                notion_page_uuid: None,
                notion_block_uuid: None,
                markdown_uuid: Some(conv_uuid.into()),
            });
        }
    }
    rows
}

fn chat_row(conv: &ConversationRow) -> GridRow {
    let when = conv.created_at.clone().or_else(|| conv.updated_at.clone());
    let text = conv
        .summary
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| conv.name.clone())
        .unwrap_or_default();
    GridRow {
        uuid: conv.conversation_uuid.clone(),
        provider: "anthropic".into(),
        kind: "Chat".into(),
        source_label: "Claude".into(),
        when_ts: when,
        author: None,
        account: Some(conv.account_uuid.clone()),
        org_uuid: conv.org_uuid.clone(),
        org_name: conv.org_name.clone(),
        project: conv.project_uuid.clone(),
        channel: None,
        conversation_name: conv.name.clone(),
        conversation_uuid: conv.conversation_uuid.clone(),
        message_index: None,
        entire_chat: format!("/chat/{}", conv.conversation_uuid),
        text,
        slack_link: None,
        qmd_path: Some(qmd_path(
            &conv.account_uuid,
            org_uuid_for_path(conv),
            &conv.conversation_uuid,
        )),
        source_url: None,
        git_sha: None,
        external_id: None,
        notion_page_uuid: None,
        notion_block_uuid: None,
        markdown_uuid: Some(conv.conversation_uuid.clone()),
    }
}
