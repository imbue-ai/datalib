//! GitHub downloader: identity + every authored/commented/@mentioned PR
//! plus its comments + reviews. Writes a single doltlite database at
//! `<data_root>/<name>/raw/entities.doltlite_db`; see [`db`] for the schema and
//! [`datalib_etl::doltlite_raw`] for the design rationale.

pub mod db;
pub mod schema_raw;

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::http::{
    default_retryability, HttpResponse, HttpService, LatchkeySettings, Retryability,
};
use datalib_etl_forge_ingest_common::{
    get_change_request, sync, walk_children, Forge, ForgeClient, Listed, SyncOptions,
};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;

pub use datalib_etl_forge_ingest_common::PER_PAGE;
pub use db::{block_on_load_all, db_path_for, LoadedChild, LoadedPullRequest, LoadedRaw, RawDb};

pub const BASE: &str = "https://api.github.com";

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
    /// Which latchkey identity the download authenticates as, from the
    /// source's `latchkey_settings:` block. Default = the only stored
    /// account for the service.
    pub latchkey: LatchkeySettings,
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: two live connections to
    /// one `.doltlite_db` make each other's `dolt_commit` fail. See
    /// `datalib/backend/etl/README.md`.
    pub db: RawDb,
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
    pub progress: datalib_etl::progress::Progress,
    /// Cross-provider knobs (the checkpoint cadence, the stop flag).
    pub control: datalib_etl::control::DownloadControl,
}

impl FetchOptions {
    /// Every field defaulted except the store, which has none to give:
    /// it is a live handle the caller opens and closes.
    pub fn new(db: RawDb) -> Self {
        Self {
            latchkey: LatchkeySettings::default(),
            db,
            scopes: DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect(),
            refresh_window_days: 30,
            max_prs: None,
            targets: Vec::new(),
            full_sync: false,
            sleep_between: Duration::ZERO,
            progress: datalib_etl::progress::Progress::noop(),
            control: datalib_etl::control::DownloadControl::default(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct FetchSummary {
    pub new_prs: usize,
    pub new_issue_comments: usize,
    pub new_reviews: usize,
    pub new_review_comments: usize,
    /// PR children GitHub no longer lists — deleted comments and reviews.
    pub pruned: usize,
    pub requests: u64,
}

/// GitHub's retry classifier. The default one already treats the
/// *secondary* rate limit (HTTP 429) and 5xx as retryable; GitHub's
/// *primary* rate limit is instead a `403` with `x-ratelimit-remaining:
/// 0` plus an `x-ratelimit-reset` epoch telling us when the window
/// resets. Map that to a retry with the computed wait so the shared loop
/// respects it.
fn github_retryability(resp: &HttpResponse) -> Retryability {
    if resp.status == 403 && resp.header("x-ratelimit-remaining") == Some("0") {
        let retry_after = resp.header("x-ratelimit-reset").and_then(|reset| {
            reset.parse::<i64>().ok().map(|ts| {
                let now = chrono::Utc::now().timestamp();
                Duration::from_secs(((ts - now).max(0) as u64).saturating_add(1))
            })
        });
        return Retryability::Retry { retry_after };
    }
    default_retryability(resp)
}

struct Github<'a> {
    db: &'a RawDb,
}

#[async_trait]
impl Forge for Github<'_> {
    type Summary = FetchSummary;
    const ITEM: &'static str = "PR";
    const SIGIL: char = '#';
    const SCOPE_CONFIG_KEY: &'static str = "github:download";

    fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    fn self_url(&self) -> String {
        format!("{BASE}/user")
    }

    async fn store_self(&self, me: &Value) -> Result<()> {
        self.db.upsert_self_identity(me).await
    }

    async fn search(
        &self,
        client: &ForgeClient,
        scope: &str,
        _me: &Value,
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

    /// Search takes a date: the stamp is RFC 3339 in seconds precision,
    /// so its 10-char prefix is the date.
    fn since_param(&self, stamp: String) -> String {
        stamp.get(..10).unwrap_or(&stamp).to_string()
    }

    /// No `updated_at`: GitHub's listing is not trusted to skip a fetch.
    fn listed(&self, item: &Value) -> Option<Listed> {
        let repo_url = item.get("repository_url")?.as_str()?;
        let repo = repo_url.rsplit("/repos/").next()?;
        let number = item.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
        (!repo.is_empty() && number > 0 && repo.contains('/')).then(|| Listed {
            container: repo.to_string(),
            number: number as u32,
            updated_at: String::new(),
        })
    }

    async fn any_stored(&self) -> Result<bool> {
        self.db.any_pull_requests().await
    }

    async fn fetch_one(
        &self,
        client: &ForgeClient,
        cr: &Listed,
        summary: &mut FetchSummary,
    ) -> Result<()> {
        let (repo, num) = (cr.container.as_str(), cr.number);
        let pr_url = format!("{BASE}/repos/{repo}/pulls/{num}");
        let Some(pr_data) = get_change_request(client, &pr_url, "PR", cr).await else {
            return Ok(());
        };
        self.db.upsert_pull_request(repo, num, &pr_data).await?;
        summary.new_prs += 1;

        // Each of these endpoints returns the PR's *whole* child list, so
        // a child we hold that the list did not mention was deleted on
        // GitHub — a resolved review thread, a comment its author removed.
        for child in CHILDREN {
            let url = format!("{BASE}/repos/{repo}/{}", (child.path)(num));
            let Some(listed) = walk_children(client, &url, cr, child.what).await else {
                continue;
            };
            let keep: HashSet<String> = listed
                .iter()
                .filter_map(|v| v.get("id").and_then(|i| i.as_i64()))
                .map(|n| n.to_string())
                .collect();
            self.db
                .upsert_children(child.table, repo, num, &listed)
                .await?;
            *(child.count)(summary) += listed.len();
            summary.pruned += self
                .db
                .prune_pr_children(child.table, repo, num, &keep)
                .await?;
        }
        Ok(())
    }

    fn record_requests(&self, summary: &mut FetchSummary, requests: u64) {
        summary.requests = requests;
    }
}

/// One of a PR's child lists: where it is read, the table it lands in,
/// and the summary count it adds to.
struct Child {
    table: &'static str,
    what: &'static str,
    path: fn(u32) -> String,
    count: fn(&mut FetchSummary) -> &mut usize,
}

const CHILDREN: [Child; 3] = [
    Child {
        table: "issue_comments",
        what: "issue comments",
        path: |num| format!("issues/{num}/comments?per_page={PER_PAGE}"),
        count: |s| &mut s.new_issue_comments,
    },
    Child {
        table: "pr_reviews",
        what: "reviews",
        path: |num| format!("pulls/{num}/reviews?per_page={PER_PAGE}"),
        count: |s| &mut s.new_reviews,
    },
    Child {
        table: "pr_review_comments",
        what: "review comments",
        path: |num| format!("pulls/{num}/comments?per_page={PER_PAGE}"),
        count: |s| &mut s.new_review_comments,
    },
];

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let client = ForgeClient::new(
        HttpService::Github,
        github_retryability,
        opts.latchkey.clone(),
    );
    let run_config = json!({
        "scopes": opts.scopes,
        "refresh_window_days": opts.refresh_window_days,
        "max_prs": opts.max_prs,
        "targets": opts.targets,
        "full_sync": opts.full_sync,
    });
    sync(
        &Github { db: &opts.db },
        &client,
        SyncOptions {
            scopes: &opts.scopes,
            refresh_window_days: opts.refresh_window_days,
            max_items: opts.max_prs,
            targets: &opts.targets,
            full_sync: opts.full_sync,
            sleep_between: opts.sleep_between,
            progress: &opts.progress,
            run_config,
        },
    )
    .await
}

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
