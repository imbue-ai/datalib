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
    routing::{get, post},
    Router,
};
use frankweiler_core::dolt_server::DoltServer;
use frankweiler_core::query::parse_query;
use frankweiler_core::repo::{DynRepo, RepoError};
use frankweiler_core::search::SearchRow;
use frankweiler_core::version::git_hash;
use frankweiler_schema::feedback::FeedbackRow;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

#[derive(Clone)]
pub struct AppState {
    /// Data root on disk — drives the static `/api/media/*` mount and
    /// the `accounts.json` lookup. The SQL store is reached through
    /// [`AppState::repo`] instead of going to `mirror.sqlite` directly.
    pub root: Arc<PathBuf>,
    /// All SQL flows through this seam. Default is
    /// [`frankweiler_core::dolt_repo::DoltRepo`] against the managed
    /// `dolt sql-server`; `--backend sqlite` swaps in
    /// [`frankweiler_core::sqlite_repo::SqliteRepo`] (read-only,
    /// reference / debug path).
    pub repo: DynRepo,
    /// Managed `dolt sql-server` subprocess. Held here so its `Drop`
    /// (SIGKILL + wait) runs only when the backend itself shuts down,
    /// keeping the server alive for every request handler. `None` when
    /// running under `--backend sqlite`.
    pub dolt_server: Option<Arc<DoltServer>>,
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
    pub source_url: Option<String>,
    pub body: String,
}

/// Client-supplied portion of a feedback submission. The server stamps
/// the rest (UUID, timestamp, app_version, git_hash) at insert time, so
/// the client only has to describe what was being clicked on and what the
/// user typed. `context` is whatever shape `feedback/context.ts` produced;
/// we round-trip it as JSON straight into the `context_json` column.
#[derive(Debug, Deserialize)]
pub struct FeedbackRequest {
    /// Optional thumb up/down — `null` when the user submitted just a
    /// comment without choosing a direction.
    pub sentiment: Option<String>,
    /// Required free-form comment. The UI disables Submit until non-empty;
    /// the server enforces the same constraint defensively.
    pub comment: String,
    /// Decoded `FeedbackContext`. Re-serialized into the row's
    /// `context_json` column verbatim.
    pub context: serde_json::Value,
}

/// What the client gets back after a successful POST. Mostly a confirmation
/// that the row landed; the UUID is useful for showing a "filed as X" toast
/// and for cross-referencing in `dolt log`.
#[derive(Debug, Serialize)]
pub struct FeedbackResponse {
    pub feedback_uuid: String,
    pub created_at: String,
    pub git_hash: &'static str,
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
        .route("/api/feedback", post(submit_feedback))
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
    let rows = s.repo.search(&parsed, limit).await.unwrap_or_default();
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
    let path = s
        .repo
        .qmd_path_for_conversation(&conversation_uuid)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let raw = std::fs::read_to_string(&path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let body = strip_frontmatter(&raw).to_string();
    let meta = s
        .repo
        .chat_meta(&conversation_uuid)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    // Synthesize page-level URLs for providers that don't carry one in
    // `source_url`. Claude/ChatGPT use the conversation UUID directly in
    // their public URL scheme.
    let source_url = meta
        .source_url
        .or_else(|| match meta.source_label.as_deref() {
            Some("Claude") => Some(format!("https://claude.ai/chat/{conversation_uuid}")),
            Some("ChatGPT") => Some(format!("https://chatgpt.com/c/{conversation_uuid}")),
            _ => None,
        });
    Ok(Json(ChatResponse {
        conversation_uuid,
        name: meta.name,
        account: meta.account,
        project: meta.project,
        channel: meta.channel,
        created_at: meta.when_ts,
        source_label: meta.source_label,
        source_url,
        body,
    }))
}

async fn submit_feedback(
    State(s): State<AppState>,
    Json(req): Json<FeedbackRequest>,
) -> Result<Json<FeedbackResponse>, StatusCode> {
    // The UI also disables Submit until non-empty, but enforce it here so
    // a hand-crafted POST can't slip an all-whitespace row past the audit
    // trail.
    if req.comment.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let context_json = serde_json::to_string(&req.context)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    // Server-stamped fields. We mint these here rather than trusting the
    // client so each row carries a server-vouched provenance and so
    // `feedback_uuid` collisions are impossible from the wire.
    let feedback_uuid = uuid::Uuid::new_v4().to_string();
    let created_at = chrono::Local::now().to_rfc3339();
    let app_version = env!("CARGO_PKG_VERSION").to_string();
    let git_hash_str = git_hash().to_string();
    let row = FeedbackRow {
        feedback_uuid: feedback_uuid.clone(),
        created_at: created_at.clone(),
        sentiment: req.sentiment,
        comment: req.comment,
        app_version,
        git_hash: git_hash_str,
        context_json,
    };
    match s.repo.insert_feedback(row).await {
        Ok(()) => Ok(Json(FeedbackResponse {
            feedback_uuid,
            created_at,
            git_hash: git_hash(),
        })),
        Err(RepoError::ReadOnly) => Err(StatusCode::SERVICE_UNAVAILABLE),
        Err(e) => {
            eprintln!("feedback insert failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
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
        let root = Arc::new(PathBuf::from("/tmp/nonexistent-fw-root"));
        let repo = frankweiler_core::repo::default_repo(root.clone()).await;
        let _r = router(AppState {
            root,
            repo,
            dolt_server: None,
        });
    }
}
