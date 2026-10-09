//! GitLab downloader: identity + every MR the user authored / was
//! assigned to / was a reviewer on, plus all discussion notes. Writes a
//! single doltlite database at `<data_root>/<group>/ingest/entities.doltlite_db`;
//! see [`db`] for schema and [`datalib_etl::doltlite_raw`] for
//! design rationale. The run itself — the listings, what is owed, the
//! fetch loop — is `datalib_etl_forge_ingest_common`; this crate
//! supplies the endpoints and the tables.

pub mod canonicalize;
pub mod db;
pub mod schema_raw;

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
use datalib_etl_web::http::{default_retryability, HttpService, LatchkeySettings};
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{Sqlite, SqlitePool, Transaction};

pub use datalib_etl_forge_ingest_common::PER_PAGE;
pub use db::{
    block_on_load_all, db_path_for, LoadedDiscussion, LoadedMergeRequest, LoadedRaw, RawDb,
};

use schema_raw::{mr_pk_recipe, MergeRequestRow};

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
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    pub scopes: Vec<String>,
    /// On a store with data, list only MRs updated in the last N days;
    /// 0 is unbounded. A first listing has no floor.
    pub refresh_window_days: u32,
    /// Most MRs to fetch this run (`None` = unbounded); the rest stay
    /// owed to later runs.
    pub max_mrs: Option<usize>,
    /// Explicit MR targets. When non-empty, discovery is skipped and
    /// only these MRs are fetched. Each entry is `(project_full_path,
    /// mr_iid)`; callers parse user-supplied refs (URL or
    /// `namespace/project!IID`) via [`parse_mr_ref`] beforehand.
    pub targets: Vec<(String, u32)>,
    /// List everything and fetch everything listed: a full backfill.
    pub full_sync: bool,
    pub sleep_between: Duration,
    pub progress: datalib_etl::progress::Progress,
    /// Cross-provider knobs (the checkpoint cadence, the stop flag).
    pub control: datalib_etl::control::DownloadControl,
    /// The run's pinned clock: the top of every listing.
    pub now: IsoOffsetTimestamp,
    /// Seals as MRs land, when the step driver hands one over.
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
            max_mrs: None,
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
    pub new_mrs: usize,
    pub new_discussions: usize,
    /// MRs the listings named at the `updated_at` the store already
    /// holds them at: not fetched.
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

/// An MR as fetched: its record and its whole discussion list.
pub struct MergeRequest {
    payload: Value,
    discussions: Vec<Value>,
}

struct Gitlab<'a> {
    db: &'a RawDb,
}

#[async_trait]
impl Forge for Gitlab<'_> {
    type Summary = FetchSummary;
    type Content = MergeRequest;
    const ITEM: &'static str = "MR";
    const SIGIL: char = '!';
    const ITEM_TABLE: &'static str = MergeRequestRow::TABLE;

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
        me: &Value,
        bounds: &Bounds,
    ) -> Result<Search> {
        let user_id = me.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        Ok(client.search(&search_url(scope, user_id, bounds)).await?)
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

    fn item_key(&self, container: &str, number: u32) -> String {
        mr_pk_recipe(container, number)
    }

    fn stamp(&self, at: &IsoOffsetTimestamp) -> String {
        stamp(at)
    }

    async fn any_stored(&self) -> Result<bool> {
        self.db.any_merge_requests().await
    }

    async fn fetch_one(
        &self,
        client: &ForgeClient,
        proj: &str,
        iid: u32,
    ) -> Result<Answer<MergeRequest>> {
        let pid = urlencoding::encode(proj);
        let mr_url = format!("{BASE}/projects/{pid}/merge_requests/{iid}");
        let payload = match get_change_request(client, &mr_url).await? {
            Ok(v) => v,
            Err(miss) => return Ok(miss),
        };
        // The endpoint returns this MR's *whole* discussion list, so a
        // discussion we hold that it did not mention was deleted on
        // GitLab.
        let disc_url =
            format!("{BASE}/projects/{pid}/merge_requests/{iid}/discussions?per_page={PER_PAGE}");
        let discussions = match walk_children(client, &disc_url, "discussions").await? {
            Ok(d) => d,
            Err(e) => return Ok(Answer::Short(vec![e])),
        };
        let without_id = discussions
            .iter()
            .filter(|d| d.get("id").and_then(|v| v.as_str()).is_none())
            .count();
        if without_id > 0 {
            return Ok(Answer::Short(vec![format!(
                "{without_id} of its discussions came back without an id"
            )]));
        }
        Ok(Answer::Whole(MergeRequest {
            payload,
            discussions,
        }))
    }

    async fn store_one(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        proj: &str,
        iid: u32,
        mr: &MergeRequest,
        summary: &mut FetchSummary,
    ) -> Result<()> {
        self.db
            .store_merge_request(tx, proj, iid, &mr.payload)
            .await?;
        summary.new_mrs += 1;
        self.db
            .store_discussions(
                tx,
                proj,
                iid,
                &mr.discussions,
                &IsoOffsetTimestamp::now_local(),
            )
            .await?;
        summary.new_discussions += mr.discussions.len();
        summary.pruned += self
            .db
            .prune_mr_discussions(tx, proj, iid, &mr.discussions)
            .await?;
        Ok(())
    }

    fn record_unchanged(&self, summary: &mut FetchSummary, count: usize) {
        summary.skipped_unchanged_mrs = count;
    }

    fn record_requests(&self, summary: &mut FetchSummary, requests: u64) {
        summary.requests = requests;
    }
}

/// The merge-request listing for one discovery scope, bounded by
/// `updated_at`. `reviewer` is not a `scope` GitLab takes: it is a
/// filter on the user's own id.
pub fn search_url(scope: &str, user_id: i64, bounds: &Bounds) -> String {
    let scope_param = if scope == "reviewer" {
        format!("reviewer_id={user_id}")
    } else {
        format!("scope={scope}")
    };
    let mut url = format!(
        "{BASE}/merge_requests?{scope_param}&state=all&per_page={PER_PAGE}&order_by=updated_at&sort=desc"
    );
    if let Some(lo) = &bounds.lo {
        url.push_str(&format!("&updated_after={}", urlencoding::encode(lo)));
    }
    if let Some(hi) = &bounds.hi {
        url.push_str(&format!("&updated_before={}", urlencoding::encode(hi)));
    }
    url
}

/// `at` as GitLab spells `updated_at`: UTC, to the millisecond, `Z`.
pub fn stamp(at: &IsoOffsetTimestamp) -> String {
    at.inner()
        .with_timezone(&Utc)
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
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
    fn an_mr_key_splits_back_into_project_and_iid() {
        assert_eq!(
            datalib_etl_forge_ingest_common::split_item_key(
                &mr_pk_recipe("starfleet/enterprise", 1701),
                <Gitlab<'_> as Forge>::SIGIL
            ),
            Some(("starfleet/enterprise".to_string(), 1701))
        );
    }

    #[test]
    fn parse_mr_ref_accepts_bang_form_and_url() {
        let (p, n) = parse_mr_ref("starfleet/enterprise!1701").unwrap();
        assert_eq!(p, "starfleet/enterprise");
        assert_eq!(n, 1701);
        let (p, n) =
            parse_mr_ref("https://gitlab.com/starfleet/enterprise/-/merge_requests/1701").unwrap();
        assert_eq!(p, "starfleet/enterprise");
        assert_eq!(n, 1701);
    }

    #[test]
    fn project_full_path_extracts_namespace() {
        assert_eq!(
            project_full_path_from_web_url(
                "https://gitlab.com/starfleet/enterprise/-/merge_requests/1701"
            ),
            Some("starfleet/enterprise".to_string())
        );
    }

    /// A listing is bounded by whole stamps, either end; the run's now
    /// is spelled as GitLab spells `updated_at` so the two sort
    /// together.
    #[test]
    fn a_listing_is_bounded_in_gitlabs_own_spelling() {
        let lo = "2369-04-12T00:00:00.000Z".to_string();
        let hi = "2369-04-15T00:00:00.000Z".to_string();
        let url = search_url(
            "created_by_me",
            7,
            &Bounds {
                lo: Some(lo),
                hi: Some(hi),
            },
        );
        assert!(
            url.ends_with(
                "&updated_after=2369-04-12T00%3A00%3A00.000Z&updated_before=2369-04-15T00%3A00%3A00.000Z"
            ),
            "{url}"
        );
        let open = search_url("reviewer", 7, &Bounds::default());
        assert!(!open.contains("updated_after") && !open.contains("updated_before"));
        let at = datalib_time::parse_strict("2369-04-15T02:00:00+02:00").unwrap();
        assert_eq!(stamp(&at), "2369-04-15T00:00:00.000Z");
    }
}
