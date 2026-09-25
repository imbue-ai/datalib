//! GitLab downloader: identity + every MR the user authored / was
//! assigned to / was a reviewer on, plus all discussion notes. Writes a
//! single doltlite database at `<data_root>/<name>/raw/entities.doltlite_db`;
//! see [`db`] for schema and [`datalib_etl::doltlite_raw`] for
//! design rationale.

pub mod canonicalize;
pub mod db;
pub mod schema_raw;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::http::{default_retryability, HttpService, LatchkeySettings};
use datalib_etl_forge_ingest_common::{
    get_change_request, sync, walk_children, Forge, ForgeClient, Listed, SyncOptions,
};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;

pub use datalib_etl_forge_ingest_common::PER_PAGE;
pub use db::{
    block_on_load_all, db_path_for, LoadedDiscussion, LoadedMergeRequest, LoadedRaw, RawDb,
};

pub const BASE: &str = "https://gitlab.com/api/v4";

pub const ENTITY_SELF: &str = "self_identity";
pub const ENTITY_MR: &str = "merge_request";
pub const ENTITY_DISCUSSION: &str = "discussion";

pub const DEFAULT_SCOPES: &[&str] = &["created_by_me", "assigned_to_me", "reviewer"];

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
    pub scopes: Vec<String>,
    pub refresh_window_days: u32,
    pub max_mrs: Option<usize>,
    /// Explicit MR targets. When non-empty, discovery is skipped and
    /// only these MRs are fetched. Each entry is `(project_full_path,
    /// mr_iid)`; callers parse user-supplied refs (URL or
    /// `namespace/project!IID`) via [`parse_mr_ref`] beforehand.
    pub targets: Vec<(String, u32)>,
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
            max_mrs: None,
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
    pub new_mrs: usize,
    pub new_discussions: usize,
    /// MRs whose listing `updated_at` matched the local copy — the
    /// detail + discussions fetch was skipped. Counted separately so
    /// the per-source one-liner can show how much work the resume cursor
    /// + per-MR skip actually saved.
    pub skipped_unchanged_mrs: usize,
    /// Discussion threads GitLab no longer lists — deleted on their MR.
    pub pruned: usize,
    pub requests: u64,
}

pub(crate) fn project_full_path_from_web_url(web_url: &str) -> Option<String> {
    let rest = web_url.strip_prefix("https://gitlab.com/")?;
    let (path, _) = rest.split_once("/-/")?;
    Some(path.to_string())
}

struct Gitlab<'a> {
    db: &'a RawDb,
}

#[async_trait]
impl Forge for Gitlab<'_> {
    type Summary = FetchSummary;
    const ITEM: &'static str = "MR";
    const SIGIL: char = '!';
    const SCOPE_CONFIG_KEY: &'static str = "gitlab:download";

    fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    fn self_url(&self) -> String {
        format!("{BASE}/user")
    }

    async fn store_self(&self, me: &Value) -> Result<()> {
        self.db.upsert_self_identity(me).await
    }

    /// `reviewer` is not a `scope` GitLab takes: it is a filter on the
    /// user's own id.
    async fn search(
        &self,
        client: &ForgeClient,
        scope: &str,
        me: &Value,
        since: Option<&str>,
    ) -> Result<Vec<Value>> {
        let scope_param = if scope == "reviewer" {
            let user_id = me.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            format!("reviewer_id={user_id}")
        } else {
            format!("scope={scope}")
        };
        let mut url = format!(
            "{BASE}/merge_requests?{scope_param}&state=all&per_page={PER_PAGE}&order_by=updated_at&sort=desc"
        );
        if let Some(s) = since {
            url.push_str(&format!("&updated_after={}", urlencoding::encode(s)));
        }
        Ok(client.paginate(&url).await?)
    }

    fn listed(&self, item: &Value) -> Option<Listed> {
        let container = item
            .get("web_url")
            .and_then(|v| v.as_str())
            .and_then(project_full_path_from_web_url)?;
        let number = item.get("iid").and_then(|v| v.as_u64()).unwrap_or(0);
        (number > 0).then(|| Listed {
            container,
            number: number as u32,
            updated_at: item
                .get("updated_at")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        })
    }

    async fn any_stored(&self) -> Result<bool> {
        self.db.any_merge_requests().await
    }

    async fn stored_updated_at(&self) -> Result<HashMap<(String, u32), String>> {
        self.db.merge_request_updated_ats().await
    }

    async fn fetch_one(
        &self,
        client: &ForgeClient,
        cr: &Listed,
        summary: &mut FetchSummary,
    ) -> Result<()> {
        let (proj, iid) = (cr.container.as_str(), cr.number);
        let pid = urlencoding::encode(proj);
        let mr_url = format!("{BASE}/projects/{pid}/merge_requests/{iid}");
        let Some(mr_data) = get_change_request(client, &mr_url, "MR", cr).await else {
            return Ok(());
        };
        self.db.upsert_merge_request(proj, iid, &mr_data).await?;
        summary.new_mrs += 1;

        // The endpoint returns this MR's *whole* discussion list, so a
        // discussion we hold that it did not mention was deleted on
        // GitLab.
        let disc_url =
            format!("{BASE}/projects/{pid}/merge_requests/{iid}/discussions?per_page={PER_PAGE}");
        let Some(discussions) = walk_children(client, &disc_url, cr, "discussions").await else {
            return Ok(());
        };
        self.db.upsert_discussions(proj, iid, &discussions).await?;
        summary.new_discussions += discussions.len();
        summary.pruned += self
            .db
            .prune_mr_discussions(proj, iid, &discussions)
            .await?;
        Ok(())
    }

    fn record_skipped(&self, summary: &mut FetchSummary) {
        summary.skipped_unchanged_mrs += 1;
    }

    fn record_requests(&self, summary: &mut FetchSummary, requests: u64) {
        summary.requests = requests;
    }
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let client = ForgeClient::new(
        HttpService::Gitlab,
        default_retryability,
        opts.latchkey.clone(),
    );
    let run_config = json!({
        "scopes": opts.scopes,
        "refresh_window_days": opts.refresh_window_days,
        "max_mrs": opts.max_mrs,
        "targets": opts.targets,
        "full_sync": opts.full_sync,
    });
    sync(
        &Gitlab { db: &opts.db },
        &client,
        SyncOptions {
            scopes: &opts.scopes,
            refresh_window_days: opts.refresh_window_days,
            max_items: opts.max_mrs,
            targets: &opts.targets,
            full_sync: opts.full_sync,
            sleep_between: opts.sleep_between,
            progress: &opts.progress,
            run_config,
        },
    )
    .await
}

pub fn parse_mr_ref(s: &str) -> Result<(String, u32)> {
    if let Some((proj, iid)) = s.split_once('!') {
        let n: u32 = iid.parse().with_context(|| format!("bad MR iid {iid:?}"))?;
        return Ok((proj.to_string(), n));
    }
    if let Some(rest) = s.strip_prefix("https://gitlab.com/") {
        if let Some((proj, tail)) = rest.split_once("/-/merge_requests/") {
            let n: u32 = tail
                .split('/')
                .next()
                .unwrap_or("")
                .parse()
                .context("bad MR iid in URL")?;
            return Ok((proj.to_string(), n));
        }
    }
    anyhow::bail!("expected namespace/project!IID or a gitlab.com MR URL, got {s:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mr_ref_accepts_bang_form_and_url() {
        let (p, n) = parse_mr_ref("generally-intelligent/generally_intelligent!7643").unwrap();
        assert_eq!(p, "generally-intelligent/generally_intelligent");
        assert_eq!(n, 7643);
        let (p, n) = parse_mr_ref(
            "https://gitlab.com/generally-intelligent/generally_intelligent/-/merge_requests/7643",
        )
        .unwrap();
        assert_eq!(p, "generally-intelligent/generally_intelligent");
        assert_eq!(n, 7643);
    }

    #[test]
    fn project_full_path_extracts_namespace() {
        assert_eq!(
            project_full_path_from_web_url(
                "https://gitlab.com/generally-intelligent/generally_intelligent/-/merge_requests/7643"
            ),
            Some("generally-intelligent/generally_intelligent".to_string())
        );
    }
}
