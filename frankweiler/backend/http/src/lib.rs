//! axum router for the Frankweiler HTTP API.
//!
//! Endpoints:
//!   GET /api/health
//!   GET /api/search?q=…&limit=…  → searches the QMD corpus under `root`
//!   GET /api/columns             → grid column metadata
//!   GET /api/chat/{uuid}         → full conversation parsed from QMD
//!
//! Dolt is the source of truth, but the QMDs are the search index — we read
//! them directly here. v0 reloads the corpus on each query; introduce caching
//! once the corpus warrants it.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    routing::get,
    Router,
};
use frankweiler_core::qmd::{self, Conversation};
use frankweiler_core::query::parse_query;
use frankweiler_core::search::{search, SearchRow};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

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

#[derive(Debug, Serialize)]
pub struct ChatResponse {
    pub conversation_uuid: String,
    pub name: Option<String>,
    pub account_uuid: Option<String>,
    pub project_uuid: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub summary: Option<String>,
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug, Serialize)]
pub struct ChatMessage {
    pub sender: String,
    pub when: Option<String>,
    pub text: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/search", get(search_handler))
        .route("/api/columns", get(columns))
        .route("/api/chat/{conversation_uuid}", get(chat))
        .with_state(state)
        .layer(CorsLayer::permissive())
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
    let limit = p.limit.unwrap_or(200).min(2000);
    let convs = qmd::load_all(&s.root);
    let total = convs.len() as u64;
    let rows = search(&convs, &parsed, limit);
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
    let convs = qmd::load_all(&s.root);
    let conv = convs
        .into_iter()
        .find(|c: &Conversation| c.frontmatter.uuid == conversation_uuid)
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(ChatResponse {
        conversation_uuid: conv.frontmatter.uuid,
        name: conv.frontmatter.name,
        account_uuid: conv.frontmatter.account_uuid,
        project_uuid: conv.frontmatter.project_uuid,
        created_at: conv.frontmatter.created_at,
        updated_at: conv.frontmatter.updated_at,
        summary: conv.frontmatter.summary,
        messages: conv
            .messages
            .into_iter()
            .map(|m| ChatMessage {
                sender: m.sender,
                when: m.when,
                text: m.text,
            })
            .collect(),
    }))
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
