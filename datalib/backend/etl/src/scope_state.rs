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

/// More moves than this of one kind are counted, not listed: a first
/// Slack run moves one sweep marker per closed DM, hundreds of them.
const LISTED_PER_KIND: usize = 5;

/// Many scopes of one kind that moved, as a count and a few examples.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KindMoves {
    /// The scopes' shared prefix: `slack:sweep:closed_dm` for
    /// `slack:sweep:closed_dm:D123`.
    pub kind: String,
    pub moved: usize,
    /// How many of them had no row before this run.
    pub new: usize,
    pub examples: Vec<String>,
}

/// The moves of a kind with few of them, each listed, and the kinds with
/// more than [`LISTED_PER_KIND`], each counted.
pub fn summarize(moves: Vec<CursorMove>) -> (Vec<CursorMove>, Vec<KindMoves>) {
    let mut by_kind: std::collections::BTreeMap<&str, Vec<&CursorMove>> = Default::default();
    for m in &moves {
        by_kind.entry(kind_of(&m.scope)).or_default().push(m);
    }
    let mut listed = Vec::new();
    let mut counted = Vec::new();
    for (kind, of_kind) in by_kind {
        if of_kind.len() <= LISTED_PER_KIND {
            listed.extend(of_kind.into_iter().cloned());
            continue;
        }
        counted.push(KindMoves {
            kind: kind.to_string(),
            moved: of_kind.len(),
            new: of_kind.iter().filter(|m| m.before.is_none()).count(),
            examples: of_kind.iter().take(3).map(|m| m.scope.clone()).collect(),
        });
    }
    listed.sort_by(|a, b| a.scope.cmp(&b.scope));
    (listed, counted)
}

fn kind_of(scope: &str) -> &str {
    scope.rsplit_once(':').map_or(scope, |(kind, _)| kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A first Slack run listed ~190 `slack:sweep:closed_dm:<id>` moves in
    /// its summary, burying the few that said anything.
    #[test]
    fn many_moves_of_one_kind_are_counted_and_the_rest_listed() {
        let before = state_with(&[("slack:sweep:closed_dm:D0", "T0")]);
        let mut after: Vec<(String, String)> = (0..190)
            .map(|i| (format!("slack:sweep:closed_dm:D{i}"), "T1".to_string()))
            .collect();
        after.push(("slack:delta:C1".into(), "T1".into()));
        after.push(("slack:delta:C2".into(), "T1".into()));
        let (listed, counted) = summarize(diff(before, after.into_iter().collect()));
        let scopes: Vec<&str> = listed.iter().map(|m| m.scope.as_str()).collect();
        assert_eq!(scopes, ["slack:delta:C1", "slack:delta:C2"]);
        assert_eq!(counted.len(), 1);
        let closed = &counted[0];
        assert_eq!(closed.kind, "slack:sweep:closed_dm");
        assert_eq!((closed.moved, closed.new), (190, 189));
        assert_eq!(closed.examples.len(), 3);
        assert!(closed
            .examples
            .iter()
            .all(|e| e.starts_with("slack:sweep:closed_dm:")));
    }

    #[test]
    fn a_few_moves_of_a_kind_stay_listed() {
        let after = state_with(&[("gmail:history:a", "T1"), ("plain", "T1")]);
        let (listed, counted) = summarize(diff(HashMap::new(), after));
        assert_eq!(listed.len(), 2);
        assert!(counted.is_empty());
    }

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
