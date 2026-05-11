//! Row shape returned by `db::grid_rows`. The legacy in-memory QMD scanner
//! that used to live here was deleted alongside `qmd.rs` — QMDs are
//! write-only output, and search runs against the `grid_rows` projection
//! in `mirror.sqlite`.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct SearchRow {
    /// Stable per-row identifier; equals the `uuid` column of `grid_rows`.
    pub uuid: String,
    pub conversation_uuid: String,
    pub message_index: Option<usize>,
    pub snippet: String,
    pub sender: String,
    pub when: String,
    pub conversation_name: String,
    pub project: String,
    pub account: String,
    pub entire_chat: String,
    pub source: String,
    pub kind: String,
    pub author: String,
    /// Slack channel display name for Slack rows; empty otherwise.
    pub channel: String,
    /// Deep-link URL to open this row in Slack; empty for non-Slack rows.
    pub slack_link: String,
    /// For Notion rows: the page-level UUID the row belongs to. Empty
    /// otherwise. Used by right-click "Filter by Notion Page".
    pub notion_page_uuid: String,
}
