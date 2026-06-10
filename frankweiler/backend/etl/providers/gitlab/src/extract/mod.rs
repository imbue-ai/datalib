//! GitLab downloader: identity + every MR the user authored / was
//! assigned to / was a reviewer on, plus all discussion notes. Writes a
//! single doltlite database at `<data_root>/raw/<name>.doltlite_db`;
//! see [`db`] for schema and [`frankweiler_etl::doltlite_raw`] for
//! design rationale.
//!
//! Port of `src/download/gitlab_web.py`. Two refinements vs Python:
//! - **Single-MR mode** (`--merge-request <project>!<iid>` or full URL).
//! - **Incremental sync state** lives in the DB itself (`sync_scope_state`
//!   table), narrowing each run via `updated_after`.

pub mod client;
pub mod db;
pub mod schema_raw;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use frankweiler_etl::extract_run::ExtractRun;
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
    /// Path to the doltlite database file. If the caller passes a
    /// legacy directory, it's rewritten to `<dir>.doltlite_db`.
    /// Ignored for opening when `db` is `Some`.
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
    pub progress: frankweiler_etl::progress::Progress,
    /// Cross-provider knobs (`--reset-and-redownload`, etc).
    pub control: frankweiler_etl::control::ExtractControl,
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
            progress: frankweiler_etl::progress::Progress::noop(),
            control: frankweiler_etl::control::ExtractControl::default(),
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

// `since_for_scope` was lifted to `frankweiler_etl::scope_state`
// alongside github's identical (modulo bugs) helper; this provider
// just re-exports the call site name so the rest of the module
// doesn't have to change.
use frankweiler_etl::scope_state::since_for_scope;

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
) -> Result<(Vec<DiscoveredMr>, HashMap<String, String>)> {
    // Per-(proj, iid) we keep the *latest* `updated_at` we saw across
    // scopes — search/scope/reviewer can each surface the same MR with
    // (in principle) different freshness; take the newest.
    let mut by_key: HashMap<(String, u32), String> = HashMap::new();
    let mut new_state: HashMap<String, String> = Default::default();
    for scope in scopes {
        let since = since_for_scope(state, scope, refresh_window_days, full);
        tracing::info!(scope, ?since, "searching MRs");
        let results = match search_mrs(client, scope, user_id, since.as_deref()).await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(scope, error = %e, "search failed; skipping scope");
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
            Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
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
    Ok((out, new_state))
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

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db_path = db_path_for(&opts.db_path);
    let _ = frankweiler_etl::latchkey::ensure_curl_shim();
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
    if opts.control.refetch_blobs {
        tracing::info!(event = "gitlab_refetch_blobs");
        frankweiler_etl::doltlite_raw::truncate_blob_refs(db.pool())
            .await
            .context("truncate blob_refs before refetch")?;
    }
    let run_config = json!({
        "scopes": opts.scopes,
        "refresh_window_days": opts.refresh_window_days,
        "max_mrs": opts.max_mrs,
        "targets": opts.targets,
        "full_sync": opts.full_sync,
    });
    let run = ExtractRun::start(db.pool(), &run_config).await?;

    let client = GitLabClient::new();
    let mut summary = FetchSummary::default();

    let work = async {
        let user_id = fetch_self(&client, &db).await?;

        let had_mrs = db.any_merge_requests().await?;
        let mr_keys: Vec<DiscoveredMr> = if !opts.targets.is_empty() {
            // Explicit targets: no listing call, no `updated_at` to
            // compare against — always fetch.
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
            let (keys, new_scope_state) = discover_mrs(
                &client,
                user_id,
                &opts.scopes,
                &state,
                opts.refresh_window_days,
                opts.full_sync || !had_mrs,
            )
            .await?;
            for (k, v) in &new_scope_state {
                db.upsert_scope_state(k, v).await?;
            }
            keys
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
    // `frankweiler_etl::scope_state` now that the implementation is
    // shared — gitlab just re-exports the helper.
}
