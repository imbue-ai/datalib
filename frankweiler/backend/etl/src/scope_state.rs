//! Shared incremental-sync cursor helpers.
//!
//! Until now, both [`crate::doltlite_raw`]'s `sync_scope_state` table
//! and the policy for *deriving* a `since` floor from it lived
//! separately in every provider that needed them — gitlab and github
//! each had a `since_for_scope` with subtly different semantics
//! (gitlab returned a full RFC3339 timestamp, github returned a date;
//! gitlab trusted state-when-present, github clamped to `min(state,
//! window_floor)`). That divergence was unintentional and bit us
//! recently (the gitlab `full_sync: true` override was hiding the
//! incremental path entirely — see `sync/main.rs:1529`'s git history).
//!
//! This module is the single source of truth:
//!
//! - [`since_for_scope`] is the canonical policy. State, if present,
//!   *is* the cursor; the refresh window is only a cold-start floor.
//! - [`snapshot`] reads every `(scope, last_seen_at)` row for diffing.
//! - [`CursorMove`] / [`diff`] turn a before/after snapshot pair into
//!   the per-scope advancement stamped into `sync_runs.summary.cursors`
//!   by `DownloadRun::finish`.

use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
use serde::Serialize;
use sqlx::SqlitePool;

/// Canonical `since` policy for an incremental-sync scope. Returns
/// what to pass as `updated_after` / `updated:>=` on the next listing
/// call — `None` means "no `since` filter; do a full scope walk."
///
/// Policy:
/// 1. `full == true`  → `None` (caller forced a full rescan).
/// 2. `state[scope] = Some(s)` → `Some(s)` (last successful sync's
///    timestamp is the authoritative cursor; refresh_window_days is
///    only a cold-start floor).
/// 3. otherwise, if `refresh_window_days > 0` → `Some(now - window)`
///    formatted as RFC 3339 (seconds precision, `Z` suffix).
/// 4. otherwise → `None` (no state, no window, no filter).
///
/// Returns full RFC 3339 (`2026-06-07T18:00:00Z`). Callers that need
/// a different form (github's `updated:>=YYYY-MM-DD`) should reformat
/// locally — the API surfaces both providers use accept either.
pub fn since_for_scope(
    state: &HashMap<String, String>,
    scope: &str,
    refresh_window_days: u32,
    full: bool,
) -> Option<String> {
    if full {
        return None;
    }
    if let Some(s) = state.get(scope) {
        return Some(s.clone());
    }
    if refresh_window_days == 0 {
        return None;
    }
    let floor = Utc::now() - ChronoDuration::days(refresh_window_days as i64);
    Some(floor.to_rfc3339_opts(SecondsFormat::Secs, true))
}

/// Snapshot every `(scope, last_seen_at)` row from `sync_scope_state`.
/// Used by [`DownloadRun`] to capture before/after cursor positions for
/// diffing into `summary.cursors`.
pub async fn snapshot(pool: &SqlitePool) -> Result<HashMap<String, String>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT scope, last_seen_at FROM sync_scope_state")
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
    fn full_returns_none_regardless_of_state_or_window() {
        let s = state_with(&[("created_by_me", "2026-01-01T00:00:00Z")]);
        assert_eq!(since_for_scope(&s, "created_by_me", 7, true), None);
    }

    #[test]
    fn state_present_takes_priority_over_window() {
        let s = state_with(&[("created_by_me", "2026-06-01T00:00:00Z")]);
        assert_eq!(
            since_for_scope(&s, "created_by_me", 7, false).as_deref(),
            Some("2026-06-01T00:00:00Z")
        );
    }

    #[test]
    fn no_state_no_window_returns_none() {
        let s = state_with(&[]);
        assert_eq!(since_for_scope(&s, "created_by_me", 0, false), None);
    }

    #[test]
    fn no_state_with_window_uses_window_floor() {
        let s = state_with(&[]);
        let got = since_for_scope(&s, "created_by_me", 7, false).expect("expected window floor");
        let parsed = chrono::DateTime::parse_from_rfc3339(&got).expect("rfc3339");
        let ago = Utc::now().signed_duration_since(parsed.with_timezone(&Utc));
        assert!(
            ago >= ChronoDuration::days(6) && ago <= ChronoDuration::days(8),
            "since={got} ago={ago:?}",
        );
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
