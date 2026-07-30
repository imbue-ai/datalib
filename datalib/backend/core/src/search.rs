//! Row shape returned by `db::grid_rows`. QMDs are write-only output;
//! search runs against the `grid_rows` projection in Dolt.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct SearchRow {
    /// Stable per-row identifier; equals the `uuid` column of `grid_rows`.
    pub uuid: String,
    pub conversation_uuid: String,
    /// FK into the `markdowns` table — addresses the rendered `.md`
    /// this row lives inside. The UI passes this to
    /// `/api/chat/{markdown_uuid}` when the user clicks the row.
    pub markdown_uuid: Option<String>,
    pub message_index: Option<usize>,
    pub snippet: String,
    pub sender: String,
    /// Null when the row has no source-side timestamp (e.g. contacts
    /// without a `REV:` field, or any row whose underlying entity isn't
    /// event-shaped). See data_architecture_ingestion.md §"Entities
    /// without a time-shape".
    pub when: Option<String>,
    pub conversation_name: String,
    pub project: String,
    pub account: String,
    /// Owning Anthropic org UUID (stable, opaque). Empty for non-Anthropic
    /// rows. UI shows `org_name` as the cell value and uses `org_uuid` as
    /// the filter key.
    pub org_uuid: String,
    /// Human-readable org name corresponding to `org_uuid`. Empty when
    /// the upstream payload didn't carry one.
    pub org_name: String,
    pub entire_chat: String,
    pub source: String,
    pub kind: String,
    pub author: String,
    /// Slack channel display name for Slack rows; empty otherwise.
    pub channel: String,
    /// Deep-link URL to open this row in Slack; empty for non-Slack rows.
    /// Legacy column — new rows carry their public URL in `source_url`.
    pub slack_link: String,
    /// Public URL for the row's source artifact (Slack permalink,
    /// LinkedIn post, …); empty when the producer set none. The grid's
    /// "Open source" action prefers this and falls back to `slack_link`.
    pub source_url: String,
    /// For Notion rows: the page-level UUID the row belongs to. Empty
    /// otherwise. Used by right-click "Filter by Notion Page".
    pub notion_page_uuid: String,
    /// Provider-assigned stable id for the underlying entity, verbatim
    /// from the `external_id` grid_rows column. Empty when the producer
    /// didn't set one. For Perseus this is the locator path (`"1"`,
    /// `"1.2"`, `"1.2.3"` for book / chapter / section), which the
    /// scaife control panel parses to build its book→chapter→section
    /// tree.
    pub external_id: String,
    /// QMD-routed rank score for this row, when the search went through qmd.
    /// `None` for pure structured queries (no free text) and for the SQL-LIKE
    /// fallback path. Surfaced to the UI as a sortable "Score" column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}
