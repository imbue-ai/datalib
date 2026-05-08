//! axum router for the Frankweiler HTTP API.
//!
//! Endpoints:
//!   GET /api/health
//!   GET /api/search?q=…&limit=…  → grid_rows query against `<root>/mirror.sqlite`
//!   GET /api/columns             → grid column metadata
//!   GET /api/chat/{uuid}         → conversation header (from grid_rows) + raw QMD body
//!
//! Dolt is the source of truth; ingest writes a portable `mirror.sqlite`
//! mirror that this service reads. **QMDs are write-only output** — the
//! `/api/chat` endpoint serves the file body verbatim (sans frontmatter)
//! and lets the UI render markdown once. We never parse a QMD back into
//! structured data; structured fields come from `grid_rows`.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    routing::get,
    Router,
};
use frankweiler_core::db::{chat_meta, grid_rows, qmd_path_for_conversation};
use frankweiler_core::query::parse_query;
use frankweiler_core::search::SearchRow;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

#[derive(Clone)]
pub struct AppState {
    pub root: Arc<PathBuf>,
}

#[derive(Debug, Serialize)]
pub struct Health {
    pub ok: bool,
    pub version: &'static str,
    pub root: String,
    pub root_exists: bool,
}

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query_echo: serde_json::Value,
    pub rows: Vec<SearchRow>,
    pub columns: Vec<ColumnSpec>,
    pub total_estimated: u64,
}

#[derive(Debug, Serialize, Clone)]
pub struct ColumnSpec {
    pub field: String,
    pub header: String,
    pub default_visible: bool,
}

/// Response shape for `/api/chat/{uuid}`. The body is the raw QMD content
/// minus the YAML frontmatter — the UI runs markdown-it on it directly. We
/// do **not** ship a structured `messages[]` array; per-message scrolling
/// uses the `<div id="m-{uuid}" data-msg-index="…">` wrappers the renderer
/// emits in the body.
#[derive(Debug, Serialize)]
pub struct ChatResponse {
    pub conversation_uuid: String,
    pub name: Option<String>,
    pub account: Option<String>,
    pub project: Option<String>,
    pub channel: Option<String>,
    pub created_at: Option<String>,
    pub source_label: Option<String>,
    pub body: String,
}

pub fn router(state: AppState) -> Router {
    // Slack image attachments are symlinked into `<root>/media/slack/<file_id>/`
    // by ingest; serve them verbatim so QMD-embedded `![](...)` URLs resolve.
    let media_dir = state.root.join("media");
    Router::new()
        .route("/api/health", get(health))
        .route("/api/search", get(search_handler))
        .route("/api/columns", get(columns))
        .route("/api/accounts", get(accounts))
        .route("/api/chat/{conversation_uuid}", get(chat))
        .nest_service("/api/media", ServeDir::new(media_dir))
        .with_state(state)
        .layer(CorsLayer::permissive())
}

async fn accounts(State(s): State<AppState>) -> Json<serde_json::Value> {
    // Ingest writes `<root>/accounts.json` mapping account UUIDs → display
    // names. We surface it verbatim so the UI can do UUID → label lookups
    // late, in render code, with the UUID still in hand.
    let path = s.root.join("accounts.json");
    let v: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    Json(v)
}

async fn health(State(s): State<AppState>) -> Json<Health> {
    Json(Health {
        ok: true,
        version: env!("CARGO_PKG_VERSION"),
        root: s.root.display().to_string(),
        root_exists: s.root.exists(),
    })
}

async fn search_handler(
    State(s): State<AppState>,
    Query(p): Query<SearchParams>,
) -> Json<SearchResponse> {
    let parsed = parse_query(p.q.as_deref().unwrap_or(""));
    let limit = p.limit.unwrap_or(200).min(100_000);
    let rows = grid_rows(&s.root, &parsed, limit);
    let total = rows.len() as u64;
    Json(SearchResponse {
        query_echo: serde_json::json!({
            "free_text": parsed.free_text,
            "resolved_type": format!("{:?}", parsed.resolved_type),
            "filters": parsed.filters.iter()
                .map(|(k, v)| (format!("{:?}", k), v.clone()))
                .collect::<Vec<_>>(),
        }),
        rows,
        columns: default_columns(),
        total_estimated: total,
    })
}

async fn columns() -> Json<Vec<ColumnSpec>> {
    Json(default_columns())
}

async fn chat(
    State(s): State<AppState>,
    Path(conversation_uuid): Path<String>,
) -> Result<Json<ChatResponse>, StatusCode> {
    // QMDs are write-only output. We read the file just to ship its body
    // to the UI as-is; structured metadata comes from grid_rows. Per-message
    // anchors in the body (<div id="m-{uuid}" data-msg-index="…">) let the
    // UI scroll-and-highlight without a structured chat schema.
    let path =
        qmd_path_for_conversation(&s.root, &conversation_uuid).ok_or(StatusCode::NOT_FOUND)?;
    let raw = std::fs::read_to_string(&path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let body = strip_frontmatter(&raw).to_string();
    let meta = chat_meta(&s.root, &conversation_uuid).unwrap_or_default();
    Ok(Json(ChatResponse {
        conversation_uuid,
        name: meta.name,
        account: meta.account,
        project: meta.project,
        channel: meta.channel,
        created_at: meta.when_ts,
        source_label: meta.source_label,
        body,
    }))
}

/// Strip a leading `---\n…\n---\n` YAML frontmatter block. This is text
/// trimming, not parsing — we don't look at the YAML contents and we don't
/// care if it's malformed; the body is whatever's after the closing `---`.
fn strip_frontmatter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text;
    };
    let Some(end) = rest.find("\n---") else {
        return text;
    };
    let after = &rest[end + 4..];
    after.strip_prefix('\n').unwrap_or(after)
}

fn default_columns() -> Vec<ColumnSpec> {
    vec![
        col("source", "Source", true),
        col("kind", "Type", true),
        col("when", "Time", true),
        col("snippet", "Contents", true),
        col("author", "Author", true),
        col("account", "Account", true),
        col("conversation_name", "Conversation Name", false),
        col("project", "Project", false),
        col("entire_chat", "Entire Chat", false),
    ]
}

fn col(field: &str, header: &str, default_visible: bool) -> ColumnSpec {
    ColumnSpec {
        field: field.into(),
        header: header.into(),
        default_visible,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_columns_listed() {
        assert_eq!(default_columns().len(), 9);
    }

    #[tokio::test]
    async fn router_compiles() {
        let _r = router(AppState {
            root: Arc::new(PathBuf::from("/tmp/nonexistent-fw-root")),
        });
    }
}
