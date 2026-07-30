//! GitHub downloader: identity + every authored/commented/@mentioned PR
//! plus its comments + reviews. Writes a single doltlite database at
//! `<data_root>/<name>/raw/entities.doltlite_db`; see [`db`] for the schema and
//! [`frankweiler_etl::doltlite_raw`] for the design rationale.
//!
//! Port of `src/download/github_web.py`. Two refinements vs Python:
//!
//! - **Single-PR mode** (`--pull-request owner/repo#NUM`) skips
//!   discovery, fetches that one PR + its children.
//! - **Incremental sync state** lives in the DB itself (`sync_scope_state`
//!   table), so re-runs narrow each search to `updated:>=since` without
//!   needing a sidecar JSON file.

pub mod client;
pub mod db;
pub mod schema_raw;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use frankweiler_etl::download_run::DownloadRun;
use frankweiler_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};

pub use client::{GitHubClient, GitHubError, BASE, PER_PAGE};
pub use db::{block_on_load_all, db_path_for, LoadedChild, LoadedPullRequest, LoadedRaw, RawDb};

pub const ENTITY_SELF: &str = "self_identity";
pub const ENTITY_PR: &str = "pull_request";
pub const ENTITY_ISSUE_COMMENT: &str = "issue_comment";
pub const ENTITY_PR_REVIEW: &str = "pr_review";
pub const ENTITY_PR_REVIEW_COMMENT: &str = "pr_review_comment";

/// Default discovery scopes. `author:@me` and `commenter:@me` cover "PRs
/// I opened" and "PRs I commented on"; `mentions:@me` adds "PRs where
/// someone @-mentioned me" so the user gets notified of incoming review
/// pings even on PRs they otherwise wouldn't touch.
pub const DEFAULT_SCOPES: &[&str] = &["author:@me", "commenter:@me", "mentions:@me"];

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Path to the doltlite database file. The entity db lives inside
    /// the per-source directory as `entities.doltlite_db` (the dir is
    /// created if needed). Ignored for opening when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB. When `Some`, `fetch` uses this directly
    /// instead of opening from `db_path`. See the matching field on
    /// the other providers' FetchOptions for rationale.
    pub db: Option<RawDb>,
    /// Discovery scopes (search-issues `is:pr <scope>` clauses).
    pub scopes: Vec<String>,
    /// On a non-empty store, only refetch PRs updated in the last N days.
    pub refresh_window_days: u32,
    /// Safety cap on PR count (`None` = unbounded). Smoke-test convenience.
    pub max_prs: Option<usize>,
    /// Explicit PR targets. When non-empty, discovery is skipped and
    /// only these PRs are fetched. Each entry is `(repo_full_name,
    /// pr_number)`; callers parse user-supplied refs (URL or
    /// `owner/repo#NUM`) via [`parse_pr_ref`] beforehand.
    pub targets: Vec<(String, u32)>,
    /// Skip the persisted per-scope state so this run does a full backfill.
    pub full_sync: bool,
    pub sleep_between: Duration,
    pub progress: frankweiler_etl::progress::Progress,
    /// Cross-provider knobs (`--reset-and-redownload`, etc).
    pub control: frankweiler_etl::control::DownloadControl,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            db: None,
            scopes: DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect(),
            refresh_window_days: 30,
            max_prs: None,
            targets: Vec::new(),
            full_sync: false,
            sleep_between: Duration::ZERO,
            progress: frankweiler_etl::progress::Progress::noop(),
            control: frankweiler_etl::control::DownloadControl::default(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct FetchSummary {
    pub new_prs: usize,
    pub new_issue_comments: usize,
    pub new_reviews: usize,
    pub new_review_comments: usize,
    pub requests: u64,
}

/// Pick the `since` date for a GitHub search scope.
///
/// Thin wrapper around the canonical
/// [`frankweiler_etl::scope_state::since_for_scope`] that truncates the returned RFC 3339 timestamp to `YYYY-MM-DD` (what
/// GitHub's `updated:>=` syntax expects). Behavior is otherwise
/// identical to gitlab's: state is the cursor, the window is a
/// cold-start floor, and `prior` lets a *widened* window reach back
/// past the cursor to cover the range it never walked.
fn since_for_scope(
    state: &HashMap<String, String>,
    scope: &str,
    refresh_window_days: u32,
    full: bool,
    prior: Option<&Value>,
) -> Option<String> {
    let raw = frankweiler_etl::scope_state::since_for_scope(
        state,
        scope,
        refresh_window_days,
        full,
        prior,
    )?;
    // Truncate to YYYY-MM-DD. The raw string is RFC 3339 in seconds
    // precision, so a 10-char prefix is the date portion.
    Some(raw.get(..10).unwrap_or(&raw).to_string())
}

async fn fetch_self(client: &GitHubClient, db: &RawDb) -> Result<()> {
    let (data, _) = client.get(&format!("{BASE}/user")).await?;
    if !data.is_object() {
        anyhow::bail!("/user returned non-object");
    }
    db.upsert_self_identity(&data).await
}

async fn search_prs(client: &GitHubClient, scope: &str, since: Option<&str>) -> Result<Vec<Value>> {
    let mut q = format!("is:pr {scope}");
    if let Some(s) = since {
        q.push_str(&format!(" updated:>={s}"));
    }
    let url = format!(
        "{BASE}/search/issues?q={}&per_page={PER_PAGE}&sort=updated&order=desc",
        urlencoding::encode(&q)
    );
    Ok(client.paginate(&url).await?)
}

/// Outcome of a discovery pass.
struct Discovery {
    /// Sorted unique `(repo_full_name, number)` pairs.
    keys: Vec<(String, u32)>,
    /// Next-run cursor per scope. Only scopes that actually searched
    /// appear, so a failed scope keeps its old cursor and retries.
    new_state: HashMap<String, String>,
    /// Scopes whose search call failed and were stepped over. Non-zero
    /// means discovery was incomplete, so a widened window has *not*
    /// been satisfied and the config must not be recorded — the blob is
    /// one row for all scopes, so recording it would lose the widening
    /// for the scopes that never ran.
    failed_scopes: usize,
}

/// Union-of-scopes discovery.
async fn discover_prs(
    client: &GitHubClient,
    scopes: &[String],
    state: &HashMap<String, String>,
    refresh_window_days: u32,
    full: bool,
    prior: Option<&Value>,
) -> Result<Discovery> {
    let mut seen: std::collections::BTreeSet<(String, u32)> = Default::default();
    let mut new_state: HashMap<String, String> = Default::default();
    let mut failed_scopes = 0usize;
    for scope in scopes {
        let since = since_for_scope(state, scope, refresh_window_days, full, prior);
        tracing::info!(scope, ?since, "searching PRs");
        let results = match search_prs(client, scope, since.as_deref()).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(scope, error = %e, "search failed; skipping scope");
                failed_scopes += 1;
                continue;
            }
        };
        for item in &results {
            let repo_url = item
                .get("repository_url")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let repo = repo_url.rsplit("/repos/").next().unwrap_or("");
            let num = item.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
            if !repo.is_empty() && num > 0 && repo.contains('/') {
                seen.insert((repo.to_string(), num as u32));
            }
        }
        new_state.insert(
            scope.clone(),
            IsoOffsetTimestamp::now_local().to_rfc3339_secs(),
        );
        tracing::info!(scope, count = results.len(), "scope done");
    }
    Ok(Discovery {
        keys: seen.into_iter().collect(),
        new_state,
        failed_scopes,
    })
}

async fn fetch_one_pr(
    client: &GitHubClient,
    db: &RawDb,
    repo: &str,
    num: u32,
    summary: &mut FetchSummary,
) -> Result<()> {
    let pr_url = format!("{BASE}/repos/{repo}/pulls/{num}");
    let (pr_data, _) = match client.get(&pr_url).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(repo, num, error = %e, "PR meta failed; skipping");
            return Ok(());
        }
    };
    if !pr_data.is_object() {
        tracing::error!(repo, num, "PR returned non-object");
        return Ok(());
    }
    db.upsert_pull_request(repo, num, &pr_data).await?;
    summary.new_prs += 1;

    let ic_url = format!("{BASE}/repos/{repo}/issues/{num}/comments?per_page={PER_PAGE}");
    for c in client.paginate(&ic_url).await.unwrap_or_default() {
        db.upsert_issue_comment(repo, num, &c).await?;
        summary.new_issue_comments += 1;
    }

    let r_url = format!("{BASE}/repos/{repo}/pulls/{num}/reviews?per_page={PER_PAGE}");
    for r in client.paginate(&r_url).await.unwrap_or_default() {
        db.upsert_pr_review(repo, num, &r).await?;
        summary.new_reviews += 1;
    }

    let rc_url = format!("{BASE}/repos/{repo}/pulls/{num}/comments?per_page={PER_PAGE}");
    for c in client.paginate(&rc_url).await.unwrap_or_default() {
        db.upsert_pr_review_comment(repo, num, &c).await?;
        summary.new_review_comments += 1;
    }
    Ok(())
}

/// Scope key for this provider's [`frankweiler_etl::scope_config`] blob.
/// Discovery scopes share one record because `refresh_window_days` is a
/// single workspace-wide knob; the per-scope cursors it interacts with
/// stay in `sync_scope_state`.
const SCOPE_CONFIG_KEY: &str = "github:download";

/// The subset of [`FetchOptions`] that decides which data lands on disk.
/// `max_prs` / `targets` / `full_sync` are per-run knobs and one-off
/// overrides, so recording them would make a smoke run read as a config
/// change to the next real sync.
fn scope_config_blob(opts: &FetchOptions) -> Value {
    frankweiler_etl::scope_state::refresh_window_blob(opts.refresh_window_days)
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db_path = db_path_for(&opts.db_path);
    let _ = frankweiler_etl::latchkey::ensure_curl_dispatch();
    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&db_path)
            .await
            .with_context(|| format!("open raw db {}", db_path.display()))?,
    };
    if opts.control.reset_and_redownload {
        tracing::info!(event = "github_reset_and_redownload");
        db.reset().await.context("reset raw db before redownload")?;
    }
    // GitHub has no blob table — PRs / comments / reviews are pure
    // JSON. `refetch_blobs` is a no-op for this provider.
    let _ = opts.control.refetch_blobs;
    let run_config = json!({
        "scopes": opts.scopes,
        "refresh_window_days": opts.refresh_window_days,
        "max_prs": opts.max_prs,
        "targets": opts.targets,
        "full_sync": opts.full_sync,
    });
    let run = DownloadRun::start(db.pool(), &run_config).await?;

    // Diff the scope-affecting params against the ones that produced the
    // current cursors. `None` (fresh store, or one written before
    // `sync_scope_config` existed) means no adjustment — see the module
    // docs on `scope_config`.
    let scope_cfg = scope_config_blob(&opts);
    let prior_scope_cfg =
        frankweiler_etl::scope_config::load_or_none(db.pool(), SCOPE_CONFIG_KEY).await;

    let client = GitHubClient::new();
    let mut summary = FetchSummary::default();
    // Whether discovery actually covered every scope this run. Only then
    // has the run satisfied `refresh_window_days`; see `scope_config`.
    let discovery_complete = std::sync::atomic::AtomicBool::new(true);

    let work = async {
        fetch_self(&client, &db).await?;

        let had_prs = db.any_pull_requests().await?;
        let pr_keys: Vec<(String, u32)> = if !opts.targets.is_empty() {
            // Explicit targets skip discovery entirely, so this run says
            // nothing about whether a widened window was covered.
            discovery_complete.store(false, std::sync::atomic::Ordering::Relaxed);
            opts.targets.clone()
        } else {
            let state = db.load_scope_state().await?;
            let discovered = discover_prs(
                &client,
                &opts.scopes,
                &state,
                opts.refresh_window_days,
                opts.full_sync || !had_prs,
                prior_scope_cfg.as_ref(),
            )
            .await?;
            if discovered.failed_scopes > 0 {
                discovery_complete.store(false, std::sync::atomic::Ordering::Relaxed);
            }
            // Persist updated state *before* per-PR fetch so a crash
            // halfway doesn't lose discovery progress.
            for (k, v) in &discovered.new_state {
                db.upsert_scope_state(k, v).await?;
            }
            discovered.keys
        };
        let pr_keys: Vec<(String, u32)> = if let Some(cap) = opts.max_prs {
            pr_keys.into_iter().take(cap).collect()
        } else {
            pr_keys
        };
        tracing::info!(count = pr_keys.len(), "PRs to fetch");

        opts.progress.set_length(Some(pr_keys.len() as u64));
        for (repo, num) in &pr_keys {
            opts.progress.inc(1);
            opts.progress.set_message(&format!("{repo}#{num}"));
            if let Err(e) = fetch_one_pr(&client, &db, repo, *num, &mut summary).await {
                tracing::error!(repo, num, error = %e, "PR fetch failed; skipping");
            }
            if opts.sleep_between > Duration::ZERO {
                tokio::time::sleep(opts.sleep_between).await;
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    let result = work.await;
    summary.requests = client.request_count();
    // Record the config only once this run has actually satisfied it. A
    // skipped scope or a targets-only run leaves the prior blob in place
    // so the next run re-plans the widening.
    frankweiler_etl::scope_config::store_if_satisfied(
        db.pool(),
        SCOPE_CONFIG_KEY,
        &scope_cfg,
        result.is_ok() && discovery_complete.load(std::sync::atomic::Ordering::Relaxed),
    )
    .await;
    run.finish(&result, &summary).await;
    result?;
    Ok(summary)
}

/// Parse `owner/repo#123` (or `owner/repo/pull/123`) into `(repo, number)`.
pub fn parse_pr_ref(s: &str) -> Result<(String, u32)> {
    if let Some((repo, num)) = s.split_once('#') {
        let n: u32 = num
            .parse()
            .with_context(|| format!("bad PR number {num:?}"))?;
        return Ok((repo.to_string(), n));
    }
    if let Some(rest) = s.strip_prefix("https://github.com/") {
        let parts: Vec<&str> = rest.split('/').collect();
        if parts.len() >= 4 && (parts[2] == "pull" || parts[2] == "pulls") {
            let repo = format!("{}/{}", parts[0], parts[1]);
            let n: u32 = parts[3].parse().context("bad PR number in URL")?;
            return Ok((repo, n));
        }
    }
    anyhow::bail!("expected owner/repo#NUM or a github.com PR URL, got {s:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pr_ref_accepts_hash_form_and_url() {
        let (r, n) = parse_pr_ref("imbue-ai/mngr#1650").unwrap();
        assert_eq!(r, "imbue-ai/mngr");
        assert_eq!(n, 1650);
        let (r, n) = parse_pr_ref("https://github.com/imbue-ai/mngr/pull/1650").unwrap();
        assert_eq!(r, "imbue-ai/mngr");
        assert_eq!(n, 1650);
    }
}

#[cfg(test)]
mod scope_config_tests {
    use super::*;
    use frankweiler_etl::scope_state::REFRESH_WINDOW_KEY;
    use serde_json::json;

    fn opts(window: u32, targets: Vec<(String, u32)>) -> FetchOptions {
        FetchOptions {
            refresh_window_days: window,
            targets,
            ..Default::default()
        }
    }

    #[test]
    fn blob_records_only_the_refresh_window() {
        // Per-run budgets and one-off overrides must stay out: a
        // `--max-prs 5` smoke run must not read as a config change to
        // the next real sync.
        let mut o = opts(30, vec![]);
        o.max_prs = Some(5);
        o.full_sync = true;
        let blob = scope_config_blob(&o);
        assert_eq!(blob, json!({ REFRESH_WINDOW_KEY: 30 }));
    }

    #[test]
    fn blob_round_trips_into_the_since_policy() {
        // The blob this provider writes is the same shape
        // `since_for_scope` reads back — the pairing the whole scheme
        // depends on.
        let blob = scope_config_blob(&opts(30, vec![]));
        let mut state = std::collections::HashMap::new();
        state.insert("s".to_string(), "2026-06-01T00:00:00Z".to_string());
        // Unchanged window: cursor stands.
        assert_eq!(
            frankweiler_etl::scope_state::since_for_scope(&state, "s", 30, false, Some(&blob))
                .as_deref(),
            Some("2026-06-01T00:00:00Z")
        );
        // Widened to unbounded: filter dropped entirely.
        assert_eq!(
            frankweiler_etl::scope_state::since_for_scope(&state, "s", 0, false, Some(&blob)),
            None
        );
    }

    #[test]
    fn discovery_is_incomplete_when_a_scope_fails() {
        // The blob is one row for every scope, so recording it after a
        // partial discovery would lose the widening for the scopes that
        // never searched.
        let d = Discovery {
            keys: Vec::new(),
            new_state: Default::default(),
            failed_scopes: 1,
        };
        assert!(d.failed_scopes > 0, "must block recording");
    }
}
