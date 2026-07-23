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
/// [`frankweiler_etl::scope_state::since_for_scope`] that truncates
/// the returned RFC 3339 timestamp to `YYYY-MM-DD` (what GitHub's
/// `updated:>=` syntax expects). Behavior is otherwise identical to
/// gitlab's: state is the cursor; window is a cold-start floor.
fn since_for_scope(
    state: &HashMap<String, String>,
    scope: &str,
    refresh_window_days: u32,
    full: bool,
) -> Option<String> {
    let raw =
        frankweiler_etl::scope_state::since_for_scope(state, scope, refresh_window_days, full)?;
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

/// Union-of-scopes discovery. Returns sorted unique `(repo_full_name, number)`
/// pairs plus the max `updated_at` per scope for the next-run state.
async fn discover_prs(
    client: &GitHubClient,
    scopes: &[String],
    state: &HashMap<String, String>,
    refresh_window_days: u32,
    full: bool,
) -> Result<(Vec<(String, u32)>, HashMap<String, String>)> {
    let mut seen: std::collections::BTreeSet<(String, u32)> = Default::default();
    let mut new_state: HashMap<String, String> = Default::default();
    for scope in scopes {
        let since = since_for_scope(state, scope, refresh_window_days, full);
        tracing::info!(scope, ?since, "searching PRs");
        let results = match search_prs(client, scope, since.as_deref()).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(scope, error = %e, "search failed; skipping scope");
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
    Ok((seen.into_iter().collect(), new_state))
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

    let client = GitHubClient::new();
    let mut summary = FetchSummary::default();

    let work = async {
        fetch_self(&client, &db).await?;

        let had_prs = db.any_pull_requests().await?;
        let pr_keys: Vec<(String, u32)> = if !opts.targets.is_empty() {
            opts.targets.clone()
        } else {
            let state = db.load_scope_state().await?;
            let (keys, new_scope_state) = discover_prs(
                &client,
                &opts.scopes,
                &state,
                opts.refresh_window_days,
                opts.full_sync || !had_prs,
            )
            .await?;
            // Persist updated state *before* per-PR fetch so a crash
            // halfway doesn't lose discovery progress.
            for (k, v) in &new_scope_state {
                db.upsert_scope_state(k, v).await?;
            }
            keys
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
