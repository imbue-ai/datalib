//! GitHub downloader: identity + every authored/commented/@mentioned PR
//! plus its comments + reviews. Writes a single doltlite database at
//! `<data_root>/<group>/ingest/entities.doltlite_db`; see [`db`] for the
//! schema and [`datalib_etl::doltlite_raw`] for the design rationale.
//! The run itself — the searches, what is owed, the fetch loop — is
//! `datalib_etl_forge_ingest_common`; this crate supplies the endpoints
//! and the tables.

pub mod db;
pub mod schema_raw;

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::raw_store::Sealer;
use datalib_etl_forge_ingest_common::{
    get_change_request, sync, walk_children, Answer, Bounds, Forge, ForgeClient, Listed, Search,
    SyncOptions,
};
use datalib_etl_web::http::{
    default_retryability, HttpResponse, HttpService, LatchkeySettings, Retryability,
};
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{Sqlite, SqlitePool, Transaction};

pub use datalib_etl_forge_ingest_common::PER_PAGE;
pub use db::{block_on_load_all, db_path_for, LoadedChild, LoadedPullRequest, LoadedRaw, RawDb};

use schema_raw::{pr_pk, PullRequestRow};

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
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    /// Discovery scopes (search-issues `is:pr <scope>` clauses).
    pub scopes: Vec<String>,
    /// On a store with data, search only for PRs updated in the last N
    /// days; 0 is unbounded. A first search has no floor.
    pub refresh_window_days: u32,
    /// Most PRs to fetch this run (`None` = unbounded); the rest stay
    /// owed to later runs.
    pub max_prs: Option<usize>,
    /// Explicit PR targets. When non-empty, discovery is skipped and
    /// only these PRs are fetched. Each entry is `(repo_full_name,
    /// pr_number)`; callers parse user-supplied refs (URL or
    /// `owner/repo#NUM`) via [`parse_pr_ref`] beforehand.
    pub targets: Vec<(String, u32)>,
    /// Search everything and fetch everything listed: a full backfill.
    pub full_sync: bool,
    pub sleep_between: Duration,
    pub progress: datalib_etl::progress::Progress,
    /// Cross-provider knobs (the checkpoint cadence, the stop flag).
    pub control: datalib_etl::control::DownloadControl,
    /// The run's pinned clock: the top of every search.
    pub now: IsoOffsetTimestamp,
    /// Seals as PRs land, when the step driver hands one over.
    pub sealer: Option<Sealer>,
}

impl FetchOptions {
    /// Every field defaulted except the store and the clock, which have
    /// none to give: a live handle the caller opens and closes, and the
    /// run's pinned now.
    pub fn new(db: RawDb, now: IsoOffsetTimestamp) -> Self {
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
            now,
            sealer: None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct FetchSummary {
    pub new_prs: usize,
    pub new_issue_comments: usize,
    pub new_reviews: usize,
    pub new_review_comments: usize,
    /// PRs the searches listed at the `updated_at` the store already
    /// holds them at: not fetched.
    pub unchanged_prs: usize,
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

/// A PR as fetched: its record and its three child lists, each whole.
pub struct PullRequest {
    payload: Value,
    children: Vec<(&'static Child, Vec<Value>)>,
}

struct Github<'a> {
    db: &'a RawDb,
}

#[async_trait]
impl Forge for Github<'_> {
    type Summary = FetchSummary;
    type Content = PullRequest;
    const ITEM: &'static str = "PR";
    const SIGIL: char = '#';
    const ITEM_TABLE: &'static str = PullRequestRow::TABLE;

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
        bounds: &Bounds,
    ) -> Result<Search> {
        Ok(client.search(&search_url(scope, bounds)).await?)
    }

    fn listed(&self, item: &Value) -> Option<Listed> {
        let repo_url = item.get("repository_url")?.as_str()?;
        let repo = repo_url.rsplit("/repos/").next()?;
        let number = item.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
        (!repo.is_empty() && number > 0 && repo.contains('/')).then(|| Listed {
            container: repo.to_string(),
            number: number as u32,
            updated_at: item
                .get("updated_at")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        })
    }

    fn item_key(&self, container: &str, number: u32) -> String {
        pr_pk(container, number)
    }

    fn stamp(&self, at: &IsoOffsetTimestamp) -> String {
        stamp(at)
    }

    async fn any_stored(&self) -> Result<bool> {
        self.db.any_pull_requests().await
    }

    async fn fetch_one(
        &self,
        client: &ForgeClient,
        repo: &str,
        num: u32,
    ) -> Result<Answer<PullRequest>> {
        let pr_url = format!("{BASE}/repos/{repo}/pulls/{num}");
        let payload = match get_change_request(client, &pr_url).await? {
            Ok(v) => v,
            Err(miss) => return Ok(miss),
        };
        // Each of these endpoints returns the PR's *whole* child list, so
        // a child we hold that the list did not mention was deleted on
        // GitHub — a resolved review thread, a comment its author removed.
        let mut shortfalls = Vec::new();
        let mut children = Vec::with_capacity(CHILDREN.len());
        for child in &CHILDREN {
            let url = format!("{BASE}/repos/{repo}/{}", (child.path)(num));
            let listed = match walk_children(client, &url, child.what).await? {
                Ok(listed) => listed,
                Err(e) => {
                    shortfalls.push(e);
                    continue;
                }
            };
            let without_id = listed.iter().filter(|v| id_of(v).is_none()).count();
            if without_id > 0 {
                shortfalls.push(format!(
                    "{without_id} of its {} came back without an id",
                    child.what
                ));
                continue;
            }
            children.push((child, listed));
        }
        Ok(Answer::from_shortfalls(
            PullRequest { payload, children },
            shortfalls,
        ))
    }

    async fn store_one(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        repo: &str,
        num: u32,
        pr: &PullRequest,
        summary: &mut FetchSummary,
    ) -> Result<()> {
        self.db
            .store_pull_request(tx, repo, num, &pr.payload)
            .await?;
        summary.new_prs += 1;
        let now = IsoOffsetTimestamp::now_local();
        for (child, listed) in &pr.children {
            let keep: HashSet<String> = listed
                .iter()
                .filter_map(id_of)
                .map(|n| n.to_string())
                .collect();
            self.db
                .store_children(tx, child.table, repo, num, listed, &now)
                .await?;
            *(child.count)(summary) += listed.len();
            summary.pruned += self
                .db
                .prune_pr_children(tx, child.table, repo, num, &keep)
                .await?;
        }
        Ok(())
    }

    fn record_unchanged(&self, summary: &mut FetchSummary, count: usize) {
        summary.unchanged_prs = count;
    }

    fn record_requests(&self, summary: &mut FetchSummary, requests: u64) {
        summary.requests = requests;
    }
}

fn id_of(v: &Value) -> Option<i64> {
    v.get("id").and_then(|i| i.as_i64())
}

/// One of a PR's child lists: where it is read, the table it lands in,
/// and the summary count it adds to.
struct Child {
    table: &'static str,
    what: &'static str,
    path: fn(u32) -> String,
    count: fn(&mut FetchSummary) -> &mut usize,
}

static CHILDREN: [Child; 3] = [
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

/// The search-issues request for one discovery scope, bounded by
/// `updated_at` as [`since_param`] makes it.
pub fn search_url(scope: &str, bounds: &Bounds) -> String {
    let mut q = format!("is:pr {scope}");
    match (&bounds.lo, &bounds.hi) {
        (Some(lo), Some(hi)) => q.push_str(&format!(
            " updated:{}..{}",
            since_param(lo),
            since_param(hi)
        )),
        (Some(lo), None) => q.push_str(&format!(" updated:>={}", since_param(lo))),
        (None, Some(hi)) => q.push_str(&format!(" updated:<={}", since_param(hi))),
        (None, None) => {}
    }
    format!(
        "{BASE}/search/issues?q={}&per_page={PER_PAGE}&sort=updated&order=desc",
        urlencoding::encode(&q)
    )
}

/// Search takes a date: a stamp's 10-char prefix is the date, which asks
/// for the whole day either end of what a bound names.
pub fn since_param(stamp: &str) -> String {
    stamp.get(..10).unwrap_or(stamp).to_string()
}

/// `at` as GitHub spells `updated_at`: UTC, to the second, `Z`.
pub fn stamp(at: &IsoOffsetTimestamp) -> String {
    at.inner()
        .with_timezone(&Utc)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

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
            now: &opts.now,
            stop: &opts.control.stop,
            sleep_between: opts.sleep_between,
            progress: &opts.progress,
            sealer: opts.sealer.as_ref(),
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
    fn a_pr_key_splits_back_into_repo_and_number() {
        assert_eq!(
            datalib_etl_forge_ingest_common::split_item_key(
                &pr_pk("o/r", 7),
                <Github<'_> as Forge>::SIGIL
            ),
            Some(("o/r".to_string(), 7))
        );
    }

    #[test]
    fn parse_pr_ref_accepts_hash_form_and_url() {
        let (r, n) = parse_pr_ref("imbue-ai/mngr#1650").unwrap();
        assert_eq!(r, "imbue-ai/mngr");
        assert_eq!(n, 1650);
        let (r, n) = parse_pr_ref("https://github.com/imbue-ai/mngr/pull/1650").unwrap();
        assert_eq!(r, "imbue-ai/mngr");
        assert_eq!(n, 1650);
    }

    /// A bound is a date, either end; the run's now is spelled as the
    /// search's `updated_at` so the two sort together.
    #[test]
    fn a_search_is_bounded_by_dates_in_githubs_own_spelling() {
        let q = |b: Bounds| {
            let url = search_url("author:@me", &b);
            urlencoding::decode(url.split("q=").nth(1).unwrap().split('&').next().unwrap())
                .unwrap()
                .into_owned()
        };
        assert_eq!(q(Bounds::default()), "is:pr author:@me");
        let lo = "2369-04-12T00:00:00Z".to_string();
        let hi = "2369-04-15T00:00:00Z".to_string();
        assert_eq!(
            q(Bounds {
                lo: Some(lo.clone()),
                hi: None
            }),
            "is:pr author:@me updated:>=2369-04-12"
        );
        assert_eq!(
            q(Bounds {
                lo: None,
                hi: Some(hi.clone())
            }),
            "is:pr author:@me updated:<=2369-04-15"
        );
        assert_eq!(
            q(Bounds {
                lo: Some(lo),
                hi: Some(hi)
            }),
            "is:pr author:@me updated:2369-04-12..2369-04-15"
        );
        let at = datalib_time::parse_strict("2369-04-15T02:00:00+02:00").unwrap();
        assert_eq!(stamp(&at), "2369-04-15T00:00:00Z");
    }
}
