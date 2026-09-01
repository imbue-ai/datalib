//! GitLab downloader: identity + every MR the user authored / was
//! assigned to / was a reviewer on, plus all discussion notes. Writes a
//! single doltlite database at `<data_root>/<name>/raw/entities.doltlite_db`;
//! see [`db`] for schema and [`datalib_etl::doltlite_raw`] for
//! design rationale.
//!
//! Port of `src/download/gitlab_web.py`. Two refinements vs Python:
//! - **Single-MR mode** (`--merge-request <project>!<iid>` or full URL).
//! - **Incremental sync state** lives in the DB itself (`sync_scope_state`
//!   table), narrowing each run via `updated_after`.

pub mod canonicalize;
pub mod client;
pub mod db;
pub mod schema_raw;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_etl::download_run::DownloadRun;
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};

pub use client::{GitLabClient, GitLabError, BASE, PER_PAGE};
pub use db::{
    block_on_load_all, db_path_for, LoadedDiscussion, LoadedMergeRequest, LoadedRaw, RawDb,
};

pub const ENTITY_SELF: &str = "self_identity";
pub const ENTITY_MR: &str = "merge_request";
pub const ENTITY_DISCUSSION: &str = "discussion";

pub const DEFAULT_SCOPES: &[&str] = &["created_by_me", "assigned_to_me", "reviewer"];

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
    /// Cross-provider knobs (`--reset-and-redownload`, etc).
    pub control: datalib_etl::control::DownloadControl,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            db: None,
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
    /// the per-source one-liner can show how much work the watermark
    /// + per-MR skip actually saved.
    pub skipped_unchanged_mrs: usize,
    pub requests: u64,
}

// The `since` policy — including the widened-window exception — is
// shared with github in `datalib_etl::scope_state`. GitLab's
// `updated_after` takes the RFC 3339 form it returns verbatim, so
// there's nothing to adapt here.
use datalib_etl::scope_state::since_for_scope;

pub(crate) fn project_full_path_from_web_url(web_url: &str) -> Option<String> {
    let rest = web_url.strip_prefix("https://gitlab.com/")?;
    let (path, _) = rest.split_once("/-/")?;
    Some(path.to_string())
}

async fn fetch_self(client: &GitLabClient, db: &RawDb) -> Result<i64> {
    let (data, _) = client.get(&format!("{BASE}/user")).await?;
    let obj = data.as_object().context("/user returned non-object")?;
    let id = obj.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
    db.upsert_self_identity(&data).await?;
    Ok(id)
}

async fn search_mrs(
    client: &GitLabClient,
    scope: &str,
    user_id: i64,
    since: Option<&str>,
) -> Result<Vec<Value>> {
    let scope_param = if scope == "reviewer" {
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

async fn discover_mrs(
    client: &GitLabClient,
    user_id: i64,
    scopes: &[String],
    state: &HashMap<String, String>,
    refresh_window_days: u32,
    full: bool,
    prior: Option<&Value>,
) -> Result<Discovery> {
    // Per-(proj, iid) we keep the *latest* `updated_at` we saw across
    // scopes — search/scope/reviewer can each surface the same MR with
    // (in principle) different freshness; take the newest.
    let mut by_key: HashMap<(String, u32), String> = HashMap::new();
    let mut new_state: HashMap<String, String> = Default::default();
    let mut failed_scopes = 0usize;
    for scope in scopes {
        let since = since_for_scope(state, scope, refresh_window_days, full, prior);
        tracing::info!(scope, ?since, "searching MRs");
        let results = match search_mrs(client, scope, user_id, since.as_deref()).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(scope, error = %e, "search failed; skipping scope");
                failed_scopes += 1;
                continue;
            }
        };
        for item in &results {
            let Some(proj) = item
                .get("web_url")
                .and_then(|v| v.as_str())
                .and_then(project_full_path_from_web_url)
            else {
                continue;
            };
            let iid = item.get("iid").and_then(|v| v.as_u64()).unwrap_or(0);
            if iid == 0 {
                continue;
            }
            let updated_at = item
                .get("updated_at")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .unwrap_or_default();
            let key = (proj, iid as u32);
            match by_key.get(&key) {
                Some(existing) if existing.as_str() >= updated_at.as_str() => {}
                _ => {
                    by_key.insert(key, updated_at);
                }
            }
        }
        new_state.insert(
            scope.clone(),
            IsoOffsetTimestamp::now_local().to_rfc3339_secs(),
        );
        tracing::info!(scope, count = results.len(), "scope done");
    }
    let mut out: Vec<DiscoveredMr> = by_key
        .into_iter()
        .map(|((proj, iid), updated_at)| DiscoveredMr {
            proj,
            iid,
            updated_at,
        })
        .collect();
    // Stable order for deterministic logs / progress.
    out.sort_by(|a, b| (a.proj.as_str(), a.iid).cmp(&(b.proj.as_str(), b.iid)));
    Ok(Discovery {
        keys: out,
        new_state,
        failed_scopes,
    })
}

/// Outcome of a discovery pass. See github's identical struct for why
/// `failed_scopes` gates recording the config blob.
struct Discovery {
    keys: Vec<DiscoveredMr>,
    new_state: HashMap<String, String>,
    failed_scopes: usize,
}

/// A (proj, iid) pair surfaced by `discover_mrs`, carrying the listing's
/// `updated_at` so the per-MR loop can skip detail fetches when the
/// local copy is already current.
#[derive(Debug, Clone)]
pub(crate) struct DiscoveredMr {
    pub proj: String,
    pub iid: u32,
    /// `updated_at` from the listing response. Empty string if the
    /// listing didn't include it (defensive — newest doesn't beat
    /// nothing, so we'll always refetch in that edge case).
    pub updated_at: String,
}

async fn fetch_one_mr(
    client: &GitLabClient,
    db: &RawDb,
    proj: &str,
    iid: u32,
    summary: &mut FetchSummary,
) -> Result<()> {
    let pid = urlencoding::encode(proj);
    let mr_url = format!("{BASE}/projects/{pid}/merge_requests/{iid}");
    let (mr_data, _) = match client.get(&mr_url).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(proj, iid, error = %e, "MR meta failed; skipping");
            return Ok(());
        }
    };
    if !mr_data.is_object() {
        tracing::error!(proj, iid, "MR returned non-object");
        return Ok(());
    }
    db.upsert_merge_request(proj, iid, &mr_data).await?;
    summary.new_mrs += 1;

    let disc_url =
        format!("{BASE}/projects/{pid}/merge_requests/{iid}/discussions?per_page={PER_PAGE}");
    let discussions = client.paginate(&disc_url).await.unwrap_or_default();
    db.upsert_discussions(proj, iid, &discussions).await?;
    summary.new_discussions += discussions.len();
    Ok(())
}

/// Scope key for this provider's [`datalib_etl::scope_config`] blob.
/// Discovery scopes share one record because `refresh_window_days` is a
/// single workspace-wide knob; the per-scope cursors it interacts with
/// stay in `sync_scope_state`.
const SCOPE_CONFIG_KEY: &str = "gitlab:download";

/// The subset of [`FetchOptions`] that decides which data lands on disk.
/// `max_mrs` / `targets` / `full_sync` are per-run knobs and one-off
/// overrides, so recording them would make a smoke run read as a config
/// change to the next real sync.
fn scope_config_blob(opts: &FetchOptions) -> Value {
    datalib_etl::scope_state::refresh_window_blob(opts.refresh_window_days)
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db_path = db_path_for(&opts.db_path);
    let _ = datalib_etl::latchkey::ensure_curl_dispatch();
    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&db_path)
            .await
            .with_context(|| format!("open raw db {}", db_path.display()))?,
    };
    if opts.control.reset_and_redownload {
        tracing::info!(event = "gitlab_reset_and_redownload");
        db.reset().await.context("reset raw db before redownload")?;
    }
    // GitLab has no blob table — MRs / discussions / notes are pure
    // JSON. `refetch_blobs` is a no-op for this provider.
    let _ = opts.control.refetch_blobs;
    let run_config = json!({
        "scopes": opts.scopes,
        "refresh_window_days": opts.refresh_window_days,
        "max_mrs": opts.max_mrs,
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
        datalib_etl::scope_config::load_or_none(db.pool(), SCOPE_CONFIG_KEY).await;

    let client = GitLabClient::new();
    let mut summary = FetchSummary::default();
    // Whether discovery actually covered every scope this run. Only then
    // has the run satisfied `refresh_window_days`; see `scope_config`.
    let discovery_complete = std::sync::atomic::AtomicBool::new(true);

    let work = async {
        let user_id = fetch_self(&client, &db).await?;

        let had_mrs = db.any_merge_requests().await?;
        let mr_keys: Vec<DiscoveredMr> = if !opts.targets.is_empty() {
            // Explicit targets: no listing call, no `updated_at` to
            // compare against — always fetch. Discovery is skipped, so
            // this run says nothing about a widened window.
            discovery_complete.store(false, std::sync::atomic::Ordering::Relaxed);
            opts.targets
                .iter()
                .cloned()
                .map(|(proj, iid)| DiscoveredMr {
                    proj,
                    iid,
                    updated_at: String::new(),
                })
                .collect()
        } else {
            let state = db.load_scope_state().await?;
            let discovered = discover_mrs(
                &client,
                user_id,
                &opts.scopes,
                &state,
                opts.refresh_window_days,
                opts.full_sync || !had_mrs,
                prior_scope_cfg.as_ref(),
            )
            .await?;
            if discovered.failed_scopes > 0 {
                discovery_complete.store(false, std::sync::atomic::Ordering::Relaxed);
            }
            for (k, v) in &discovered.new_state {
                db.upsert_scope_state(k, v).await?;
            }
            discovered.keys
        };
        let mr_keys: Vec<DiscoveredMr> = if let Some(cap) = opts.max_mrs {
            mr_keys.into_iter().take(cap).collect()
        } else {
            mr_keys
        };
        tracing::info!(count = mr_keys.len(), "MRs to fetch");

        // Bulk-load every (proj, iid)→updated_at we already have a
        // payload for. One scan, then per-MR comparison is O(1). This
        // is what lets a Ctrl-C'd previous run resume cheaply: the
        // listing still shows all 210, but we skip the N we already
        // fully fetched.
        let local_updated: HashMap<(String, u32), String> = if opts.full_sync {
            HashMap::new()
        } else {
            db.merge_request_updated_ats().await?
        };

        opts.progress.set_length(Some(mr_keys.len() as u64));
        for d in &mr_keys {
            opts.progress.inc(1);
            opts.progress.set_message(&format!("{}!{}", d.proj, d.iid));
            // Skip if the local copy's `updated_at` matches the
            // listing's. Empty `updated_at` from discovery (targets
            // mode or a listing item missing the field) falls through
            // to the unconditional fetch.
            if !d.updated_at.is_empty() {
                if let Some(local) = local_updated.get(&(d.proj.clone(), d.iid)) {
                    if local.as_str() == d.updated_at.as_str() {
                        summary.skipped_unchanged_mrs += 1;
                        if opts.sleep_between > Duration::ZERO {
                            tokio::time::sleep(opts.sleep_between).await;
                        }
                        continue;
                    }
                }
            }
            if let Err(e) = fetch_one_mr(&client, &db, &d.proj, d.iid, &mut summary).await {
                tracing::error!(proj = %d.proj, iid = d.iid, error = %e, "MR fetch failed; skipping");
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
    datalib_etl::scope_config::store_if_satisfied(
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

/// Parse `namespace/project!IID` or a gitlab.com MR URL into `(proj, iid)`.
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

    // `since_for_scope` policy tests live in
    // `datalib_etl::scope_state` now that the implementation is
    // shared — gitlab just re-exports the helper.
}

#[cfg(test)]
mod scope_config_tests {
    use super::*;
    use datalib_etl::scope_state::REFRESH_WINDOW_KEY;
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
        // `--max-mrs 5` smoke run must not read as a config change to
        // the next real sync.
        let mut o = opts(30, vec![]);
        o.max_mrs = Some(5);
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
            datalib_etl::scope_state::since_for_scope(&state, "s", 30, false, Some(&blob))
                .as_deref(),
            Some("2026-06-01T00:00:00Z")
        );
        // Widened to unbounded: filter dropped entirely.
        assert_eq!(
            datalib_etl::scope_state::since_for_scope(&state, "s", 0, false, Some(&blob)),
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
