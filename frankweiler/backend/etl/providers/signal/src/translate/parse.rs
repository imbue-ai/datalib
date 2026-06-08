//! Parse the doltlite raw store into a small in-memory `ParsedSignal`
//! that the renderer can walk without re-querying.
//!
//! We read three tables — `recipients`, `chats`, `chat_items` — and
//! decode the `payload` BLOB on `chat_items` to extract the text body
//! of each `StandardMessage`. Other ChatItem variants (stickers,
//! view-once, ChatUpdate, …) are skipped silently in this first
//! render version; the raw doltlite still has the bytes, so a future
//! `RENDER_VERSION` bump can surface them.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use frankweiler_signal_backup::backup;
use prost::Message;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;

#[derive(Debug, Default, Clone)]
pub struct ParsedSignal {
    pub recipients: HashMap<String, ParsedRecipient>,
    pub chats: Vec<ParsedChat>,
}

#[derive(Debug, Clone)]
pub struct ParsedRecipient {
    pub id: String,
    pub identifier: Option<String>,
    pub display_name: Option<String>,
}

impl ParsedRecipient {
    /// Best-effort display: `display_name` falls back to `identifier`
    /// falls back to the raw id (so the rendered markdown is always
    /// readable, even for distribution lists / groups we haven't
    /// modelled yet).
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
    pub items: Vec<ParsedChatItem>,
}

#[derive(Debug, Clone)]
pub struct ParsedChatItem {
    pub author_id: String,
    pub date_sent: i64,
    pub text: Option<String>,
    /// True when ChatItem.directionalDetails was `outgoing`. Drives
    /// "me" attribution in the rendered markdown.
    pub outgoing: bool,
}

pub fn parse_raw_dir(input: &Path) -> Result<ParsedSignal> {
    let db_path = frankweiler_etl::doltlite_raw::db_path_for(input);
    if !db_path.is_file() {
        return Ok(ParsedSignal::default());
    }
    // Borrow the orchestrator's tokio runtime; spinning up a fresh
    // one panics from inside `#[tokio::main]`.
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move { parse_async(&db_path).await })
    })
}

async fn parse_async(db_path: &Path) -> Result<ParsedSignal> {
    let opts =
        SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?.read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("open raw doltlite for translate at {}", db_path.display()))?;

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

    let crows = sqlx::query("SELECT id, recipient_id FROM chats ORDER BY id")
        .fetch_all(&pool)
        .await
        .context("read chats")?;
    let mut chats: Vec<ParsedChat> = crows
        .iter()
        .map(|r| {
            Ok(ParsedChat {
                id: r.try_get("id")?,
                recipient_id: r.try_get("recipient_id")?,
                items: Vec::new(),
            })
        })
        .collect::<Result<_>>()?;

    let irows = sqlx::query(
        "SELECT chat_id, author_id, date_sent, payload \
         FROM chat_items ORDER BY chat_id, date_sent",
    )
    .fetch_all(&pool)
    .await
    .context("read chat_items")?;
    let mut by_chat: HashMap<String, Vec<ParsedChatItem>> = HashMap::new();
    for r in &irows {
        let chat_id: String = r.try_get("chat_id")?;
        let author_id: String = r.try_get("author_id")?;
        let date_sent: i64 = r.try_get("date_sent")?;
        let payload: Vec<u8> = r.try_get("payload")?;
        let (text, outgoing) = decode_chat_item(&payload);
        by_chat.entry(chat_id).or_default().push(ParsedChatItem {
            author_id,
            date_sent,
            text,
            outgoing,
        });
    }
    for chat in &mut chats {
        if let Some(items) = by_chat.remove(&chat.id) {
            chat.items = items;
        }
    }

    Ok(ParsedSignal { recipients, chats })
}

/// Crack a `Frame.chat_item` blob and pull out (text, outgoing).
/// Returns `(None, false)` for non-StandardMessage chat items so the
/// renderer can skip them cleanly without panicking.
fn decode_chat_item(payload: &[u8]) -> (Option<String>, bool) {
    let frame = match backup::Frame::decode(payload) {
        Ok(f) => f,
        Err(_) => return (None, false),
    };
    let ci = match frame.item {
        Some(backup::frame::Item::ChatItem(ci)) => ci,
        _ => return (None, false),
    };
    let outgoing = matches!(
        ci.directional_details,
        Some(backup::chat_item::DirectionalDetails::Outgoing(_))
    );
    let text = match ci.item {
        Some(backup::chat_item::Item::StandardMessage(sm)) => sm.text.and_then(|t| {
            if t.body.is_empty() {
                None
            } else {
                Some(t.body)
            }
        }),
        _ => None,
    };
    (text, outgoing)
}
