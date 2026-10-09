//! What a run moved in `sync_scope_state`, the table that holds upstream
//! delta tokens and sweep markers, for the run's summary.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::SqlitePool;

pub async fn snapshot(pool: &SqlitePool) -> Result<HashMap<String, String>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT scope, last_seen_at_utc FROM sync_scope_state")
            .fetch_all(pool)
            .await
            .context("snapshot sync_scope_state")?;
    Ok(rows.into_iter().collect())
}

#[derive(Debug, Clone, Serialize)]
pub struct CursorMove {
    pub scope: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// Per-scope advancement between two `sync_scope_state` snapshots.
/// Only scopes that *moved* are returned — unchanged scopes are
/// noise. A scope that appears in `after` but not `before` shows up
/// with `before = None` (first sync for that scope).
pub fn diff(before: HashMap<String, String>, after: HashMap<String, String>) -> Vec<CursorMove> {
    let mut moves = Vec::new();
    for (scope, after_val) in &after {
        let before_val = before.get(scope);
        if before_val.map(String::as_str) != Some(after_val.as_str()) {
            moves.push(CursorMove {
                scope: scope.clone(),
                before: before_val.cloned(),
                after: Some(after_val.clone()),
            });
        }
    }
    // Scopes that vanished entirely (rare; only if a provider deletes
    // its scope_state row) are also flagged for completeness.
    for (scope, before_val) in &before {
        if !after.contains_key(scope) {
            moves.push(CursorMove {
                scope: scope.clone(),
                before: Some(before_val.clone()),
                after: None,
            });
        }
    }
    moves.sort_by(|a, b| a.scope.cmp(&b.scope));
    moves
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn diff_flags_advanced_scopes_only() {
        let before = state_with(&[("a", "T1"), ("b", "T1")]);
        let after = state_with(&[("a", "T2"), ("b", "T1"), ("c", "T1")]);
        let moves = diff(before, after);
        // `a` advanced T1→T2; `c` appeared; `b` is unchanged so not
        // included.
        assert_eq!(moves.len(), 2);
        assert_eq!(moves[0].scope, "a");
        assert_eq!(moves[0].before.as_deref(), Some("T1"));
        assert_eq!(moves[0].after.as_deref(), Some("T2"));
        assert_eq!(moves[1].scope, "c");
        assert_eq!(moves[1].before, None);
        assert_eq!(moves[1].after.as_deref(), Some("T1"));
    }
}
