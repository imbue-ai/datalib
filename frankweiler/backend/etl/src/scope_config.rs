//! Per-scope record of the config that produced the current cursor.
//!
//! Every incremental downloader answers "what can I skip this run?" one
//! of two ways. Either it re-fetches the upstream listing and filters it
//! (anthropic, chatgpt) — in which case the config is consulted every
//! run and stays live — or it stores a bookmark and resumes from it
//! (slack, github, gitlab, email, yolink). A bookmark is a *place*, not
//! a *rule*: once it exists it answers "where do I start?" all by
//! itself, and the config that originally set it never gets a second
//! vote. That is why widening `slack.sync.since` after the first sync
//! silently does nothing — the forward walk resumes from the channel's
//! stored watermark and `since` is only read on the cold-start arm.
//!
//! This module closes that gap by recording, next to each cursor, the
//! config subset that produced it. A later run diffs current-vs-stored
//! and reacts *proportionally*: a widened `since` backfills just the
//! newly-in-scope window instead of forcing a full re-download.
//!
//! # What belongs in the blob
//!
//! Only params that change **which data ends up on disk**. Not per-run
//! budgets (`max_prs`, `limit`), not one-off overrides (`conv_uuids`,
//! `targets`, `full_sync`), not paths or credentials. A `--max-prs 5`
//! smoke run must not read as a config change to the next real run.
//!
//! That curation is why this is a separate blob rather than a diff of
//! `sync_runs.config`: the latter is an audit log and deliberately
//! records everything, including the knobs above.
//!
//! Params that are already re-evaluated every run don't belong here
//! either — recording them just invites a pointless re-walk. Slack's
//! `refresh_window_days` is applied fresh on every sync, and a channel
//! newly added to `channels:` has no rows so it cold-starts on its own.
//! Neither needs remembering.
//!
//! # An absent blob is not "changed"
//!
//! [`load`] returns `None` for a store written before this table
//! existed — which is every installed data root the first time this
//! ships. Callers **must** treat `None` as "adopt the current config,
//! take no action". Treating it as "unknown, therefore re-download"
//! would kick off a simultaneous full backfill on every mirror in the
//! field, which is a much worse failure than the bug being fixed.
//!
//! The same tolerance applies per-key: a blob written by an older
//! version won't have keys added later, and a missing key reads as
//! "no information", never as "changed". The [`turned_on`] /
//! [`limit_relaxed`] / [`strings_added`] helpers all encode that.
//!
//! # Write on the success path only
//!
//! [`store`] belongs after the work future succeeds. A run that fails
//! or is cancelled partway hasn't actually satisfied the new config, so
//! leaving the previous blob in place is what makes the next run retry
//! the adjustment. (Slack's SIGINT handler exits the process, so a
//! cancelled sync never reaches the store call at all — which is the
//! behavior we want.)

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::{Row, SqlitePool};

/// Read the config recorded for `scope` by the last run that completed
/// successfully.
///
/// `None` means "never recorded" — a fresh store, or one that predates
/// the `sync_scope_config` table. See the module docs: that is *not* a
/// signal to re-download.
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

/// Upsert the config blob for `scope`. Call on the success path only.
pub async fn store(pool: &SqlitePool, scope: &str, config: &Value) -> Result<()> {
    let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
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
///
/// Encodes the rule every consumer needs: the record describes work that
/// has *actually happened*, so a run that failed, was cancelled, or
/// stepped over part of its scope must leave the previous record in
/// place for the next run to re-plan against. Bookkeeping failures are
/// logged and swallowed; they must never mask the run's own error.
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

// ─────────────────────────────────────────────────────────────────────
// Field comparisons
// ─────────────────────────────────────────────────────────────────────
//
// Deliberately small. Each helper answers exactly one question — "did
// this knob start admitting more data than it used to?" — because that
// is the only direction that needs work: a *narrowed* knob leaves the
// on-disk superset alone (nothing in the pipeline deletes), so it is
// always a no-op. Providers own anything richer; parsing a `since`
// string is the provider's business, not this module's.

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

/// An upper-bound knob (`Option<u64>`, `None` = unlimited) that now
/// admits strictly more than it did.
///
/// `false` when the blob is absent or the key is missing. A stored JSON
/// `null` means the bound was already unlimited, so nothing can relax
/// past it.
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

/// Entries in `cur` that aren't in the stored string array at `key`.
///
/// Empty when the blob is absent or the key is missing — an unknown
/// prior set can't tell us anything was added. Useful for providers
/// whose scope is a list (mailbox labels, notion subtree seeds) and
/// which can cold-start just the new members.
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
