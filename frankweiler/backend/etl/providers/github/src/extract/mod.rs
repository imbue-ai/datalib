//! GitHub downloader: identity + every authored/commented/@mentioned PR
//! plus its comments + reviews. Event-store JSONL layout under
//! `<out_dir>/<entity>/{created,updated}/events.jsonl` (one stream per
//! entity), consumed by [`crate::translate::parse`].
//!
//! Port of `src/download/github_web.py` with two refinements:
//!
//! - **Single-PR mode** (`--pull-request owner/repo#NUM`) skips
//!   discovery, fetches that one PR + its children. Useful for smoke
//!   tests and snapshot fixtures.
//! - **Incremental sync state** at `<out>/sync_state.json` carries a
//!   `last_seen_at` per discovery scope, so re-runs narrow each search
//!   to `updated:>=since` without losing edits that happen between runs
//!   (an overlap window protects against clock skew).

pub mod client;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
use frankweiler_etl::event_store::{diff_and_save, load_latest_by_key, make_record};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use client::{auto_set_latchkey_curl, GitHubClient, GitHubError, BASE, PER_PAGE};

pub const ENTITY_SELF: &str = "self_identity";
pub const ENTITY_PR: &str = "pull_request";
pub const ENTITY_ISSUE_COMMENT: &str = "issue_comment";
pub const ENTITY_PR_REVIEW: &str = "pr_review";
pub const ENTITY_PR_REVIEW_COMMENT: &str = "pr_review_comment";

/// Default discovery scopes. `author:@me` and `commenter:@me` cover
/// "PRs I opened" and "PRs I commented on"; `mentions:@me` adds "PRs
/// where someone @-mentioned me" so the user gets notified of incoming
/// review pings even on PRs they otherwise wouldn't touch.
pub const DEFAULT_SCOPES: &[&str] = &["author:@me", "commenter:@me", "mentions:@me"];

#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub out_dir: PathBuf,
    /// Discovery scopes (search-issues `is:pr <scope>` clauses).
    pub scopes: Vec<String>,
    /// On a non-empty out_dir, only refetch PRs updated in the last N days.
    pub refresh_window_days: u32,
    /// Safety cap on PR count (`None` = unbounded). Smoke-test convenience.
    pub max_prs: Option<usize>,
    /// Single-PR mode: skip discovery, fetch this one PR. (repo, number).
    pub single_pr: Option<(String, u32)>,
    /// Skip the sync_state.json read so this run does a full backfill.
    pub full_sync: bool,
    pub sleep_between: Duration,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            out_dir: PathBuf::new(),
            scopes: DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect(),
            refresh_window_days: 30,
            max_prs: None,
            single_pr: None,
            full_sync: false,
            sleep_between: Duration::ZERO,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FetchSummary {
    pub new_prs: usize,
    pub upd_prs: usize,
    pub new_issue_comments: usize,
    pub upd_issue_comments: usize,
    pub new_reviews: usize,
    pub upd_reviews: usize,
    pub new_review_comments: usize,
    pub upd_review_comments: usize,
    pub requests: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncState {
    /// Per-scope last-seen ISO timestamp from the search result `updated_at`.
    /// We use `min` across all scopes when narrowing the next run.
    #[serde(default)]
    scopes: HashMap<String, String>,
}

fn sync_state_path(out_dir: &Path) -> PathBuf {
    out_dir.join("sync_state.json")
}

fn load_sync_state(out_dir: &Path) -> SyncState {
    let p = sync_state_path(out_dir);
    if !p.exists() {
        return SyncState::default();
    }
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_sync_state(out_dir: &Path, state: &SyncState) -> Result<()> {
    let p = sync_state_path(out_dir);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&p, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

/// Pick the `since` ISO date for a search scope. We use the *minimum* of
/// (state.scopes[scope], now - refresh_window) so we never narrow tighter
/// than the safety window — that catches edits to old PRs that wouldn't
/// otherwise show up in a `last_seen_at`-based filter.
fn since_for_scope(
    state: &SyncState,
    scope: &str,
    refresh_window_days: u32,
    full: bool,
) -> Option<String> {
    if full || refresh_window_days == 0 {
        return None;
    }
    let window_floor = Utc::now() - ChronoDuration::days(refresh_window_days as i64);
    let from_state = state.scopes.get(scope).and_then(|s| {
        chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&Utc))
    });
    let since = match from_state {
        Some(s) if s < window_floor => s,
        _ => window_floor,
    };
    Some(since.date_naive().to_string())
}

fn key_self(rec: &Value) -> String {
    rec.get("user_id")
        .and_then(|v| v.as_i64())
        .map(|n| n.to_string())
        .unwrap_or_default()
}
fn key_pr(rec: &Value) -> String {
    format!(
        "{}#{}",
        rec.get("repo_full_name").and_then(|v| v.as_str()).unwrap_or(""),
        rec.get("pr_number").and_then(|v| v.as_i64()).unwrap_or(0)
    )
}
fn key_comment(rec: &Value) -> String {
    format!(
        "{}#{}",
        rec.get("repo_full_name").and_then(|v| v.as_str()).unwrap_or(""),
        rec.get("comment_id").and_then(|v| v.as_i64()).unwrap_or(0)
    )
}
fn key_review(rec: &Value) -> String {
    format!(
        "{}#{}",
        rec.get("repo_full_name").and_then(|v| v.as_str()).unwrap_or(""),
        rec.get("review_id").and_then(|v| v.as_i64()).unwrap_or(0)
    )
}

fn make_pr_record(repo: &str, data: &Value) -> Value {
    let mut k = Map::new();
    k.insert("repo_full_name".into(), Value::String(repo.into()));
    k.insert(
        "pr_number".into(),
        data.get("number").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "html_url".into(),
        data.get("html_url").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "state".into(),
        data.get("state").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "merged_at".into(),
        data.get("merged_at").cloned().unwrap_or(Value::Null),
    );
    let head = data.get("head").cloned().unwrap_or(Value::Null);
    let base = data.get("base").cloned().unwrap_or(Value::Null);
    k.insert("head_sha".into(), head.get("sha").cloned().unwrap_or(Value::Null));
    k.insert("head_ref".into(), head.get("ref").cloned().unwrap_or(Value::Null));
    k.insert("base_sha".into(), base.get("sha").cloned().unwrap_or(Value::Null));
    k.insert("base_ref".into(), base.get("ref").cloned().unwrap_or(Value::Null));
    k.insert(
        "updated_at".into(),
        data.get("updated_at").cloned().unwrap_or(Value::Null),
    );
    make_record(k, data.clone())
}

fn make_issue_comment_record(repo: &str, num: u32, c: &Value) -> Value {
    let mut k = Map::new();
    k.insert("repo_full_name".into(), Value::String(repo.into()));
    k.insert("pr_number".into(), Value::from(num));
    k.insert(
        "comment_id".into(),
        c.get("id").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "html_url".into(),
        c.get("html_url").cloned().unwrap_or(Value::Null),
    );
    let user = c.get("user").cloned().unwrap_or(Value::Null);
    k.insert(
        "user_login".into(),
        user.get("login").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "created_at".into(),
        c.get("created_at").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "updated_at".into(),
        c.get("updated_at").cloned().unwrap_or(Value::Null),
    );
    make_record(k, c.clone())
}

fn make_review_record(repo: &str, num: u32, r: &Value) -> Value {
    let mut k = Map::new();
    k.insert("repo_full_name".into(), Value::String(repo.into()));
    k.insert("pr_number".into(), Value::from(num));
    k.insert("review_id".into(), r.get("id").cloned().unwrap_or(Value::Null));
    k.insert(
        "html_url".into(),
        r.get("html_url").cloned().unwrap_or(Value::Null),
    );
    let user = r.get("user").cloned().unwrap_or(Value::Null);
    k.insert(
        "user_login".into(),
        user.get("login").cloned().unwrap_or(Value::Null),
    );
    k.insert("state".into(), r.get("state").cloned().unwrap_or(Value::Null));
    k.insert(
        "commit_id".into(),
        r.get("commit_id").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "submitted_at".into(),
        r.get("submitted_at").cloned().unwrap_or(Value::Null),
    );
    make_record(k, r.clone())
}

fn make_review_comment_record(repo: &str, num: u32, c: &Value) -> Value {
    let mut k = Map::new();
    k.insert("repo_full_name".into(), Value::String(repo.into()));
    k.insert("pr_number".into(), Value::from(num));
    k.insert(
        "comment_id".into(),
        c.get("id").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "in_reply_to_id".into(),
        c.get("in_reply_to_id").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "pull_request_review_id".into(),
        c.get("pull_request_review_id").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "html_url".into(),
        c.get("html_url").cloned().unwrap_or(Value::Null),
    );
    let user = c.get("user").cloned().unwrap_or(Value::Null);
    k.insert(
        "user_login".into(),
        user.get("login").cloned().unwrap_or(Value::Null),
    );
    k.insert("path".into(), c.get("path").cloned().unwrap_or(Value::Null));
    k.insert("line".into(), c.get("line").cloned().unwrap_or(Value::Null));
    k.insert(
        "original_line".into(),
        c.get("original_line").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "commit_id".into(),
        c.get("commit_id").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "original_commit_id".into(),
        c.get("original_commit_id").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "created_at".into(),
        c.get("created_at").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "updated_at".into(),
        c.get("updated_at").cloned().unwrap_or(Value::Null),
    );
    make_record(k, c.clone())
}

async fn fetch_self(client: &GitHubClient, out_dir: &Path) -> Result<()> {
    let (data, _) = client.get(&format!("{BASE}/user")).await?;
    let obj = data.as_object().context("/user returned non-object")?;
    let mut k = Map::new();
    k.insert(
        "user_id".into(),
        obj.get("id").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "login".into(),
        obj.get("login").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "html_url".into(),
        obj.get("html_url").cloned().unwrap_or(Value::Null),
    );
    let rec = make_record(k, data.clone());
    let existing = load_latest_by_key(out_dir, ENTITY_SELF, key_self)?;
    diff_and_save(out_dir, ENTITY_SELF, &[rec], &existing, key_self)?;
    Ok(())
}

async fn search_prs(
    client: &GitHubClient,
    scope: &str,
    since: Option<&str>,
) -> Result<Vec<Value>> {
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
/// pairs plus the max `updated_at` per scope for the next-run state file.
async fn discover_prs(
    client: &GitHubClient,
    scopes: &[String],
    state: &SyncState,
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
            let repo_url = item.get("repository_url").and_then(|v| v.as_str()).unwrap_or("");
            let repo = repo_url.rsplit("/repos/").next().unwrap_or("");
            let num = item.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
            if !repo.is_empty() && num > 0 && repo.contains('/') {
                seen.insert((repo.to_string(), num as u32));
            }
        }
        // Record the run timestamp so next-time we narrow further.
        new_state.insert(
            scope.clone(),
            Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        );
        tracing::info!(scope, count = results.len(), "scope done");
    }
    Ok((seen.into_iter().collect(), new_state))
}

async fn fetch_one_pr(
    client: &GitHubClient,
    out_dir: &Path,
    repo: &str,
    num: u32,
    existing_prs: &mut HashMap<String, Value>,
    existing_ic: &mut HashMap<String, Value>,
    existing_r: &mut HashMap<String, Value>,
    existing_rc: &mut HashMap<String, Value>,
    summary: &mut FetchSummary,
) -> Result<()> {
    // PR meta
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
    let pr_rec = make_pr_record(repo, &pr_data);
    let counts = diff_and_save(out_dir, ENTITY_PR, &[pr_rec.clone()], existing_prs, key_pr)?;
    summary.new_prs += counts.new;
    summary.upd_prs += counts.updated;
    existing_prs.insert(key_pr(&pr_rec), pr_rec);

    // Issue comments (PR conversation tab)
    let ic_url = format!("{BASE}/repos/{repo}/issues/{num}/comments?per_page={PER_PAGE}");
    let issue_comments = client.paginate(&ic_url).await.unwrap_or_default();
    let ic_recs: Vec<Value> = issue_comments
        .iter()
        .map(|c| make_issue_comment_record(repo, num, c))
        .collect();
    if !ic_recs.is_empty() {
        let counts = diff_and_save(out_dir, ENTITY_ISSUE_COMMENT, &ic_recs, existing_ic, key_comment)?;
        summary.new_issue_comments += counts.new;
        summary.upd_issue_comments += counts.updated;
        for r in &ic_recs {
            existing_ic.insert(key_comment(r), r.clone());
        }
    }

    // PR reviews
    let r_url = format!("{BASE}/repos/{repo}/pulls/{num}/reviews?per_page={PER_PAGE}");
    let reviews = client.paginate(&r_url).await.unwrap_or_default();
    let r_recs: Vec<Value> = reviews
        .iter()
        .map(|r| make_review_record(repo, num, r))
        .collect();
    if !r_recs.is_empty() {
        let counts = diff_and_save(out_dir, ENTITY_PR_REVIEW, &r_recs, existing_r, key_review)?;
        summary.new_reviews += counts.new;
        summary.upd_reviews += counts.updated;
        for r in &r_recs {
            existing_r.insert(key_review(r), r.clone());
        }
    }

    // PR review (inline diff) comments
    let rc_url = format!("{BASE}/repos/{repo}/pulls/{num}/comments?per_page={PER_PAGE}");
    let review_comments = client.paginate(&rc_url).await.unwrap_or_default();
    let rc_recs: Vec<Value> = review_comments
        .iter()
        .map(|c| make_review_comment_record(repo, num, c))
        .collect();
    if !rc_recs.is_empty() {
        let counts = diff_and_save(
            out_dir,
            ENTITY_PR_REVIEW_COMMENT,
            &rc_recs,
            existing_rc,
            key_comment,
        )?;
        summary.new_review_comments += counts.new;
        summary.upd_review_comments += counts.updated;
        for r in &rc_recs {
            existing_rc.insert(key_comment(r), r.clone());
        }
    }

    Ok(())
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    std::fs::create_dir_all(&opts.out_dir)
        .with_context(|| format!("create {}", opts.out_dir.display()))?;
    auto_set_latchkey_curl();
    let client = GitHubClient::new();
    let mut summary = FetchSummary::default();

    fetch_self(&client, &opts.out_dir).await?;

    let mut existing_prs = load_latest_by_key(&opts.out_dir, ENTITY_PR, key_pr)?;
    let mut existing_ic = load_latest_by_key(&opts.out_dir, ENTITY_ISSUE_COMMENT, key_comment)?;
    let mut existing_r = load_latest_by_key(&opts.out_dir, ENTITY_PR_REVIEW, key_review)?;
    let mut existing_rc =
        load_latest_by_key(&opts.out_dir, ENTITY_PR_REVIEW_COMMENT, key_comment)?;

    let pr_keys: Vec<(String, u32)> = if let Some(spr) = &opts.single_pr {
        vec![spr.clone()]
    } else {
        let state = load_sync_state(&opts.out_dir);
        let (keys, new_scope_state) = discover_prs(
            &client,
            &opts.scopes,
            &state,
            opts.refresh_window_days,
            opts.full_sync || existing_prs.is_empty(),
        )
        .await?;
        // Save updated sync state. We do this *before* per-PR fetch so a
        // crash halfway doesn't lose the discovery progress — incremental
        // sync is allowed to skip PRs that didn't change since last run.
        let mut merged = state;
        for (k, v) in new_scope_state {
            merged.scopes.insert(k, v);
        }
        save_sync_state(&opts.out_dir, &merged)?;
        keys
    };
    let pr_keys: Vec<(String, u32)> = if let Some(cap) = opts.max_prs {
        pr_keys.into_iter().take(cap).collect()
    } else {
        pr_keys
    };
    tracing::info!(count = pr_keys.len(), "PRs to fetch");

    for (repo, num) in &pr_keys {
        if let Err(e) = fetch_one_pr(
            &client,
            &opts.out_dir,
            repo,
            *num,
            &mut existing_prs,
            &mut existing_ic,
            &mut existing_r,
            &mut existing_rc,
            &mut summary,
        )
        .await
        {
            tracing::error!(repo, num, error = %e, "PR fetch failed; skipping");
        }
        if opts.sleep_between > Duration::ZERO {
            tokio::time::sleep(opts.sleep_between).await;
        }
    }

    summary.requests = client.request_count();
    Ok(summary)
}

/// Parse `owner/repo#123` (or `owner/repo/pull/123`) into `(repo, number)`.
pub fn parse_pr_ref(s: &str) -> Result<(String, u32)> {
    if let Some((repo, num)) = s.split_once('#') {
        let n: u32 = num.parse().with_context(|| format!("bad PR number {num:?}"))?;
        return Ok((repo.to_string(), n));
    }
    // URL form
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
