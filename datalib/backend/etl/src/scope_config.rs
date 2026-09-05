//! Per-scope record of the config that produced the current cursor.

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::{Row, SqlitePool};

pub async fn load(pool: &SqlitePool, scope: &str) -> Result<Option<Value>> {
    let row = sqlx::query("SELECT config FROM sync_scope_config WHERE scope = ?")
        .bind(scope)
        .fetch_optional(pool)
        .await
        .with_context(|| format!("select sync_scope_config {scope}"))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let raw: String = row.try_get("config").context("read scope config")?;
    // A blob we can't parse is treated the same as an absent one: the
    // conservative direction is "take no action", not "re-download
    // everything because the bookkeeping is confusing".
    match serde_json::from_str(&raw) {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            tracing::warn!(
                event = "scope_config_unparseable",
                scope = scope,
                error = %e,
                "ignoring stored scope config",
            );
            Ok(None)
        }
    }
}

pub async fn store(pool: &SqlitePool, scope: &str, config: &Value) -> Result<()> {
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let body = serde_json::to_string(config).context("serialize scope config")?;
    sqlx::query(
        "INSERT INTO sync_scope_config (scope, config, updated_at) VALUES (?, ?, ?)
         ON CONFLICT(scope) DO UPDATE SET config = excluded.config, \
         updated_at = excluded.updated_at",
    )
    .bind(scope)
    .bind(&body)
    .bind(&now)
    .execute(pool)
    .await
    .with_context(|| format!("upsert sync_scope_config {scope}"))?;
    Ok(())
}

/// [`store`], but only when `satisfied` — and never fatal.
pub async fn store_if_satisfied(
    pool: &SqlitePool,
    scope: &str,
    config: &Value,
    satisfied: bool,
) -> bool {
    if !satisfied {
        tracing::info!(
            event = "scope_config_not_recorded",
            scope = scope,
            "run did not fully satisfy its config; keeping the prior record",
        );
        return false;
    }
    match store(pool, scope, config).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                event = "scope_config_store_failed",
                scope = scope,
                error = %format!("{e:#}"),
            );
            false
        }
    }
}

/// [`load`], downgrading a read failure to "no record" rather than
/// failing the run. A missing record only ever costs a skipped
/// adjustment; failing the sync over bookkeeping would cost the sync.
pub async fn load_or_none(pool: &SqlitePool, scope: &str) -> Option<Value> {
    match load(pool, scope).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                event = "scope_config_load_failed",
                scope = scope,
                error = %format!("{e:#}"),
            );
            None
        }
    }
}

// Field comparisons

/// A `bool` knob that went `false` → `true` since the recorded run.
///
/// `false` when the blob is absent, the key is missing, or the stored
/// value isn't a bool.
pub fn turned_on(prev: Option<&Value>, key: &str, cur: bool) -> bool {
    if !cur {
        return false;
    }
    matches!(
        prev.and_then(|p| p.get(key)).and_then(Value::as_bool),
        Some(false)
    )
}

pub fn limit_relaxed(prev: Option<&Value>, key: &str, cur: Option<u64>) -> bool {
    let Some(stored) = prev.and_then(|p| p.get(key)) else {
        return false;
    };
    match (stored.is_null(), stored.as_u64(), cur) {
        // Was unlimited — can't widen further.
        (true, _, _) => false,
        // Now unlimited, previously capped.
        (false, Some(_), None) => true,
        // Both capped: strictly larger cap admits more.
        (false, Some(prev_cap), Some(cur_cap)) => cur_cap > prev_cap,
        // Stored value isn't a number we understand: no information.
        _ => false,
    }
}

/// How a list-shaped filter moved, for the common convention where an
/// **empty list means "no filter"** — i.e. the *widest* setting, not the
/// narrowest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterChange {
    /// Nothing newly in scope. Includes every narrowing, and the case
    /// where the record is absent or lacks the key.
    Unchanged,
    /// The filter was removed entirely: everything is now in scope.
    WidenedToAll,
    /// These entries are newly in scope; the rest of the filter stands.
    Added(Vec<String>),
}

pub fn filter_widened(prev: Option<&Value>, key: &str, cur: &[String]) -> FilterChange {
    let Some(stored) = prev.and_then(|p| p.get(key)).and_then(Value::as_array) else {
        return FilterChange::Unchanged;
    };
    if stored.is_empty() {
        // Was already unfiltered; nothing can widen past it.
        return FilterChange::Unchanged;
    }
    if cur.is_empty() {
        return FilterChange::WidenedToAll;
    }
    match strings_added(prev, key, cur) {
        added if added.is_empty() => FilterChange::Unchanged,
        added => FilterChange::Added(added),
    }
}

pub fn strings_added(prev: Option<&Value>, key: &str, cur: &[String]) -> Vec<String> {
    let Some(stored) = prev.and_then(|p| p.get(key)).and_then(Value::as_array) else {
        return Vec::new();
    };
    let before: std::collections::HashSet<&str> = stored.iter().filter_map(Value::as_str).collect();
    cur.iter()
        .filter(|s| !before.contains(s.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn blob() -> Value {
        json!({
            "since": "2024-01-01",
            "media": false,
            "blob_size_limit_bytes": 1000,
            "labels": ["Inbox", "Work"],
            "unlimited": Value::Null,
        })
    }

    // ── absent blob is inert ─────────────────────────────────────────

    #[test]
    fn absent_blob_never_reports_a_change() {
        assert!(!turned_on(None, "media", true));
        assert!(!limit_relaxed(None, "blob_size_limit_bytes", None));
        assert!(strings_added(None, "labels", &["New".to_string()]).is_empty());
    }

    #[test]
    fn missing_key_never_reports_a_change() {
        let b = blob();
        assert!(!turned_on(Some(&b), "nope", true));
        assert!(!limit_relaxed(Some(&b), "nope", None));
        assert!(strings_added(Some(&b), "nope", &["New".to_string()]).is_empty());
    }

    // ── turned_on ────────────────────────────────────────────────────

    #[test]
    fn turned_on_only_fires_false_to_true() {
        let b = blob();
        assert!(turned_on(Some(&b), "media", true));
        // true → false is a narrowing; nothing to fetch.
        assert!(!turned_on(Some(&b), "media", false));
        let on = json!({"media": true});
        assert!(!turned_on(Some(&on), "media", true));
        assert!(!turned_on(Some(&on), "media", false));
    }

    // ── limit_relaxed ────────────────────────────────────────────────

    #[test]
    fn limit_relaxed_detects_raised_and_lifted_caps() {
        let b = blob();
        assert!(limit_relaxed(Some(&b), "blob_size_limit_bytes", Some(2000)));
        assert!(limit_relaxed(Some(&b), "blob_size_limit_bytes", None));
    }

    #[test]
    fn limit_relaxed_ignores_tightened_and_equal_caps() {
        let b = blob();
        assert!(!limit_relaxed(Some(&b), "blob_size_limit_bytes", Some(500)));
        assert!(!limit_relaxed(
            Some(&b),
            "blob_size_limit_bytes",
            Some(1000)
        ));
    }

    #[test]
    fn already_unlimited_cannot_relax() {
        let b = blob();
        assert!(!limit_relaxed(Some(&b), "unlimited", None));
        assert!(!limit_relaxed(Some(&b), "unlimited", Some(10)));
    }

    // ── strings_added ────────────────────────────────────────────────

    #[test]
    fn strings_added_returns_only_new_entries() {
        let b = blob();
        let cur = vec![
            "Inbox".to_string(),
            "Work".to_string(),
            "Archive".to_string(),
        ];
        assert_eq!(strings_added(Some(&b), "labels", &cur), vec!["Archive"]);
    }

    #[test]
    fn strings_removed_are_not_additions() {
        let b = blob();
        let cur = vec!["Inbox".to_string()];
        assert!(strings_added(Some(&b), "labels", &cur).is_empty());
    }

    // ── filter_widened ───────────────────────────────────────────────

    fn labels(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_to_populated_is_a_narrowing() {
        // The trap: `[]` means "no filter", so adding an entry *shrinks*
        // scope even though the set grew.
        let b = json!({"labels": []});
        assert_eq!(
            filter_widened(Some(&b), "labels", &labels(&["Sent"])),
            FilterChange::Unchanged
        );
    }

    #[test]
    fn populated_to_empty_widens_to_all() {
        let b = json!({"labels": ["Sent"]});
        assert_eq!(
            filter_widened(Some(&b), "labels", &[]),
            FilterChange::WidenedToAll
        );
    }

    #[test]
    fn added_entries_are_reported() {
        let b = json!({"labels": ["Sent"]});
        assert_eq!(
            filter_widened(Some(&b), "labels", &labels(&["Sent", "Inbox"])),
            FilterChange::Added(vec!["Inbox".to_string()])
        );
    }

    #[test]
    fn removing_an_entry_is_unchanged() {
        let b = json!({"labels": ["Sent", "Inbox"]});
        assert_eq!(
            filter_widened(Some(&b), "labels", &labels(&["Sent"])),
            FilterChange::Unchanged
        );
    }

    #[test]
    fn absent_record_is_unchanged() {
        assert_eq!(
            filter_widened(None, "labels", &labels(&["Sent"])),
            FilterChange::Unchanged
        );
        assert_eq!(
            filter_widened(Some(&json!({})), "labels", &labels(&["Sent"])),
            FilterChange::Unchanged
        );
    }

    // ── storage ──────────────────────────────────────────────────────

    async fn test_pool(dir: &std::path::Path) -> SqlitePool {
        // No provider tables needed — `sync_scope_config` rides in
        // `SHARED_DDL`, which is exactly the property under test.
        crate::doltlite_raw::open(&dir.join("entities.doltlite_db"), &[])
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn absent_row_loads_as_none() {
        let d = tempfile::tempdir().unwrap();
        let pool = test_pool(d.path()).await;
        assert!(load(&pool, "slack:download").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn store_then_load_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let pool = test_pool(d.path()).await;
        let cfg = json!({"since": "2024-01-01", "media": true});
        store(&pool, "slack:download", &cfg).await.unwrap();
        assert_eq!(load(&pool, "slack:download").await.unwrap(), Some(cfg));
    }

    #[tokio::test]
    async fn store_overwrites_and_scopes_are_independent() {
        let d = tempfile::tempdir().unwrap();
        let pool = test_pool(d.path()).await;
        store(&pool, "a", &json!({"since": "2024-01-01"}))
            .await
            .unwrap();
        store(&pool, "b", &json!({"since": "2020-01-01"}))
            .await
            .unwrap();
        store(&pool, "a", &json!({"since": "2023-01-01"}))
            .await
            .unwrap();
        assert_eq!(
            load(&pool, "a").await.unwrap(),
            Some(json!({"since": "2023-01-01"}))
        );
        assert_eq!(
            load(&pool, "b").await.unwrap(),
            Some(json!({"since": "2020-01-01"}))
        );
    }

    #[tokio::test]
    async fn unparseable_stored_blob_reads_as_absent() {
        let d = tempfile::tempdir().unwrap();
        let pool = test_pool(d.path()).await;
        sqlx::query(
            "INSERT INTO sync_scope_config (scope, config, updated_at) VALUES ('x', '{oops', 'now')",
        )
        .execute(&pool)
        .await
        .unwrap();
        // Conservative direction: no information, not "re-download".
        assert!(load(&pool, "x").await.unwrap().is_none());
    }
}
