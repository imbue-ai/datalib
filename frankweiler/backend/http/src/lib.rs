//! F7: axum router. v0 wires four endpoints; the search/chat/columns ones
//! return placeholder responses until F2/F5/F6 land.

use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::Json,
    routing::get,
    Router,
};
use frankweiler_core::query::parse_query;
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;

#[derive(Debug, Serialize)]
pub struct Health {
    pub ok: bool,
    pub version: &'static str,
}

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query_echo: serde_json::Value,
    pub rows: Vec<serde_json::Value>,
    pub columns: Vec<ColumnSpec>,
    pub total_estimated: u64,
}

#[derive(Debug, Serialize, Clone)]
pub struct ColumnSpec {
    pub field: String,
    pub header: String,
    pub default_visible: bool,
}

pub fn router() -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/search", get(search))
        .route("/api/columns", get(columns))
        .route("/api/chat/:conversation_uuid", get(chat))
        .layer(CorsLayer::permissive())
}

async fn health() -> Json<Health> {
    Json(Health { ok: true, version: env!("CARGO_PKG_VERSION") })
}

async fn search(Query(p): Query<SearchParams>) -> Json<SearchResponse> {
    let parsed = parse_query(p.q.as_deref().unwrap_or(""));
    Json(SearchResponse {
        query_echo: serde_json::json!({
            "free_text": parsed.free_text,
            "resolved_type": format!("{:?}", parsed.resolved_type),
            "filters": parsed.filters.iter().map(|(k, v)| (format!("{:?}", k), v.clone())).collect::<Vec<_>>(),
        }),
        rows: vec![],
        columns: default_columns(),
        total_estimated: 0,
    })
}

async fn columns() -> Json<Vec<ColumnSpec>> {
    Json(default_columns())
}

async fn chat(Path(conversation_uuid): Path<String>) -> Result<Json<serde_json::Value>, StatusCode> {
    Ok(Json(serde_json::json!({
        "conversation_uuid": conversation_uuid,
        "messages": [],
        "note": "F6 chat assembler not yet implemented",
    })))
}

fn default_columns() -> Vec<ColumnSpec> {
    vec![
        col("snippet", "Snippet", true),
        col("sender", "Sender", true),
        col("when", "When", true),
        col("conversation_name", "Conversation Name", true),
        col("project", "Project", false),
        col("account", "Account", false),
        col("entire_chat", "Entire Chat", true),
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
        assert_eq!(default_columns().len(), 7);
    }

    #[tokio::test]
    async fn router_compiles() {
        let _r = router();
    }
}
