//! `GET /api/pipeline/history?tree=<id>`: the commit history of every
//! doltlite store under one declared tree — a step's, or a group's with
//! its steps' under it. Reads the stores; never writes them.

use std::path::Path;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::{usage, AppState};

/// How many commits to walk when the request does not say. Each one
/// costs a diff against its parent, so the walk is bounded rather than
/// the whole log read back for a store that checkpoints every few
/// seconds.
const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 5_000;

#[derive(Debug, Deserialize)]
pub struct HistoryParams {
    tree: String,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct TreeHistory {
    pub tree: String,
    /// One per `.doltlite_db` file found, in path order.
    pub stores: Vec<StoreEntry>,
}

#[derive(Debug, Serialize)]
pub struct StoreEntry {
    /// The file, data-root-relative.
    pub path: String,
    #[serde(flatten)]
    pub history: datalib_history::StoreHistory,
}

pub async fn tree_history(
    State(s): State<AppState>,
    Query(p): Query<HistoryParams>,
) -> Result<Json<TreeHistory>, (StatusCode, String)> {
    let declared = usage::declared_trees(&s.config_path());
    if !declared.contains(&p.tree) {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no group or step writes {}", p.tree),
        ));
    }
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let mut stores = Vec::new();
    for rel in stores_under(&s.root, &p.tree, &declared) {
        let history = datalib_history::read(&s.root.join(&rel), limit)
            .await
            .map_err(|e| {
                eprintln!("history: {rel}: {e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, format!("{rel}: {e:#}"))
            })?;
        stores.push(StoreEntry { path: rel, history });
    }
    Ok(Json(TreeHistory {
        tree: p.tree,
        stores,
    }))
}

/// Every `.doltlite_db` directly inside the tree's directory, then inside
/// each declared tree under it — a group's steps. Root-relative, sorted.
/// Not a recursive walk: a render tree holds thousands of markdown files
/// and no store below its top level.
fn stores_under(root: &Path, tree: &str, declared: &[String]) -> Vec<String> {
    let prefix = format!("{tree}/");
    let dirs = std::iter::once(tree).chain(
        declared
            .iter()
            .map(String::as_str)
            .filter(|t| t.starts_with(&prefix)),
    );
    let mut found = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.ends_with(".doltlite_db") && entry.path().is_file() {
                found.push(format!("{dir}/{name}"));
            }
        }
    }
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_finds_its_steps_stores_and_nothing_deeper() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for p in [
            "slack/ingest/entities.doltlite_db",
            "slack/ingest/blobs.doltlite_db",
            "slack/render_markdown/indexed_markdown.doltlite_db",
            "slack/render_markdown/deep/nested.doltlite_db",
            "slack/stray.doltlite_db",
            "other/ingest/entities.doltlite_db",
        ] {
            let path = root.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"").unwrap();
        }
        let declared: Vec<String> = ["slack", "slack/ingest", "slack/render_markdown", "other"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            stores_under(root, "slack", &declared),
            [
                "slack/ingest/blobs.doltlite_db",
                "slack/ingest/entities.doltlite_db",
                "slack/render_markdown/indexed_markdown.doltlite_db",
                "slack/stray.doltlite_db",
            ]
        );
        assert_eq!(
            stores_under(root, "slack/ingest", &declared),
            [
                "slack/ingest/blobs.doltlite_db",
                "slack/ingest/entities.doltlite_db",
            ]
        );
        assert!(stores_under(root, "missing", &declared).is_empty());
    }
}
