//! Shared incremental-sync cursor helpers.

use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
use serde::Serialize;
use sqlx::SqlitePool;

pub fn since_for_scope(
    state: &HashMap<String, String>,
    scope: &str,
    refresh_window_days: u32,
    full: bool,
    prior: Option<&serde_json::Value>,
) -> Option<String> {
    if full {
        return None;
    }
    if let Some(s) = state.get(scope) {
        let Some(prev_window) = prior
            .and_then(|p| p.get(REFRESH_WINDOW_KEY))
            .and_then(serde_json::Value::as_u64)
        else {
            return Some(s.clone());
        };
        // `0` means "no floor", i.e. the widest possible window, so it
        // can't be compared as a plain number.
        let widened = match (prev_window, refresh_window_days as u64) {
            (0, _) => false, // was already unbounded
            (_, 0) => true,  // now unbounded
            (prev, cur) => cur > prev,
        };
        if !widened {
            return Some(s.clone());
        }
        if refresh_window_days == 0 {
            return None;
        }
        let floor = Utc::now() - ChronoDuration::days(refresh_window_days as i64);
        let floor = floor.to_rfc3339_opts(SecondsFormat::Secs, true);
        // Both are RFC 3339 at seconds precision in UTC, so the
        // lexicographic min is the chronological one.
        return Some(if floor < *s { floor } else { s.clone() });
    }
    if refresh_window_days == 0 {
        return None;
    }
    let floor = Utc::now() - ChronoDuration::days(refresh_window_days as i64);
    Some(floor.to_rfc3339_opts(SecondsFormat::Secs, true))
}

/// Key under which providers record `refresh_window_days` in their
/// [`crate::scope_config`] blob. Shared so writer and reader can't
/// drift — a typo in either half would degrade silently to "no
/// information, plan no work", the exact failure this machinery exists
/// to eliminate.
pub const REFRESH_WINDOW_KEY: &str = "refresh_window_days";

/// The [`crate::scope_config`] blob for a discovery-scoped provider
/// whose only scope-affecting knob is the refresh window. Written by
/// github and gitlab; paired with the `prior` argument to
/// [`since_for_scope`], which reads the same key.
pub fn refresh_window_blob(refresh_window_days: u32) -> serde_json::Value {
    serde_json::json!({ REFRESH_WINDOW_KEY: refresh_window_days })
}

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
        assert_eq!(since_for_scope(&s, "created_by_me", 7, true, None), None);
    }

    #[test]
    fn state_present_takes_priority_over_window() {
        let s = state_with(&[("created_by_me", "2026-06-01T00:00:00Z")]);
        assert_eq!(
            since_for_scope(&s, "created_by_me", 7, false, None).as_deref(),
            Some("2026-06-01T00:00:00Z")
        );
    }

    #[test]
    fn no_state_no_window_returns_none() {
        let s = state_with(&[]);
        assert_eq!(since_for_scope(&s, "created_by_me", 0, false, None), None);
    }

    #[test]
    fn no_state_with_window_uses_window_floor() {
        let s = state_with(&[]);
        let got =
            since_for_scope(&s, "created_by_me", 7, false, None).expect("expected window floor");
        let parsed = chrono::DateTime::parse_from_rfc3339(&got).expect("rfc3339");
        let ago = Utc::now().signed_duration_since(parsed.with_timezone(&Utc));
        assert!(
            ago >= ChronoDuration::days(6) && ago <= ChronoDuration::days(8),
            "since={got} ago={ago:?}",
        );
    }

    // ── config-change escape hatch ───────────────────────────────────

    fn prior(window: u64) -> serde_json::Value {
        serde_json::json!({ REFRESH_WINDOW_KEY: window })
    }

    fn days_back(since: &str) -> i64 {
        let parsed = chrono::DateTime::parse_from_rfc3339(since).expect("rfc3339");
        Utc::now()
            .signed_duration_since(parsed.with_timezone(&Utc))
            .num_days()
    }

    #[test]
    fn absent_prior_keeps_the_cursor() {
        // Every store predating `sync_scope_config`. Must not re-walk.
        let s = state_with(&[("a", "2026-06-01T00:00:00Z")]);
        assert_eq!(
            since_for_scope(&s, "a", 365, false, None).as_deref(),
            Some("2026-06-01T00:00:00Z")
        );
    }

    #[test]
    fn unchanged_window_keeps_the_cursor() {
        let s = state_with(&[("a", "2026-06-01T00:00:00Z")]);
        assert_eq!(
            since_for_scope(&s, "a", 30, false, Some(&prior(30))).as_deref(),
            Some("2026-06-01T00:00:00Z")
        );
    }

    #[test]
    fn narrowed_window_keeps_the_cursor() {
        // Store is already a superset; nothing to fetch.
        let s = state_with(&[("a", "2026-06-01T00:00:00Z")]);
        assert_eq!(
            since_for_scope(&s, "a", 7, false, Some(&prior(30))).as_deref(),
            Some("2026-06-01T00:00:00Z")
        );
    }

    #[test]
    fn widened_window_reaches_back_past_the_cursor() {
        // Cursor is recent, so the widened floor is the earlier bound and
        // wins — this is the case the whole change exists for.
        let s = state_with(&[(
            "a",
            Utc::now()
                .to_rfc3339_opts(SecondsFormat::Secs, true)
                .as_str(),
        )]);
        let got = since_for_scope(&s, "a", 365, false, Some(&prior(30)))
            .expect("expected a widened floor");
        assert!(
            (364..=366).contains(&days_back(&got)),
            "expected ~365d back, got {got}"
        );
    }

    #[test]
    fn widened_window_keeps_an_older_cursor() {
        // The cursor already predates the new floor, so it stays: no
        // reason to give up its precision and re-walk what it covers.
        let s = state_with(&[("a", "2020-01-01T00:00:00Z")]);
        assert_eq!(
            since_for_scope(&s, "a", 365, false, Some(&prior(30))).as_deref(),
            Some("2020-01-01T00:00:00Z")
        );
    }

    #[test]
    fn window_widened_to_unbounded_drops_the_filter() {
        let s = state_with(&[("a", "2026-06-01T00:00:00Z")]);
        assert_eq!(since_for_scope(&s, "a", 0, false, Some(&prior(30))), None);
    }

    #[test]
    fn already_unbounded_window_is_never_widened() {
        // `0` is the widest window there is; moving to a finite one is a
        // narrowing, so the cursor stands.
        let s = state_with(&[("a", "2026-06-01T00:00:00Z")]);
        assert_eq!(
            since_for_scope(&s, "a", 30, false, Some(&prior(0))).as_deref(),
            Some("2026-06-01T00:00:00Z")
        );
        assert_eq!(
            since_for_scope(&s, "a", 0, false, Some(&prior(0))).as_deref(),
            Some("2026-06-01T00:00:00Z")
        );
    }

    #[test]
    fn full_still_wins_over_a_widened_window() {
        let s = state_with(&[("a", "2026-06-01T00:00:00Z")]);
        assert_eq!(since_for_scope(&s, "a", 365, true, Some(&prior(30))), None);
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
