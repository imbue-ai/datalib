//! GitLab downloader: identity + every MR the user authored / was
//! assigned to / was a reviewer on, plus all discussion notes. Event-store
//! JSONL under `<out_dir>/<entity>/{created,updated}/events.jsonl`.
//!
//! Port of `src/download/gitlab_web.py`. Two refinements vs Python:
//! - **Single-MR mode** (`--merge-request <project>!<iid>` or full URL).
//! - **Incremental sync state** at `<out>/sync_state.json` with per-scope
//!   `last_seen_at`, narrowing each run via `updated_after`.

pub mod client;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Duration as ChronoDuration, SecondsFormat, Utc};
use frankweiler_etl::event_store::{diff_and_save, load_latest_by_key, make_record};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use client::{auto_set_latchkey_curl, GitLabClient, GitLabError, BASE, PER_PAGE};

pub const ENTITY_SELF: &str = "self_identity";
pub const ENTITY_MR: &str = "merge_request";
pub const ENTITY_DISCUSSION: &str = "discussion";

/// Default discovery scopes. GitLab's `/merge_requests` (global) lets us
/// query "I authored", "I was assigned", and "I was a reviewer" — those
/// three union out to the same coverage as github's author/commenter/
/// mentions. GitLab doesn't expose "commenter:@me" cheaply over REST; for
/// pure @mention coverage we'd need `/todos?action=mentioned` (TODO).
pub const DEFAULT_SCOPES: &[&str] = &["created_by_me", "assigned_to_me", "reviewer"];

#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub out_dir: PathBuf,
    pub scopes: Vec<String>,
    pub refresh_window_days: u32,
    pub max_mrs: Option<usize>,
    pub single_mr: Option<(String, u32)>, // (project_full_path, iid)
    pub full_sync: bool,
    pub sleep_between: Duration,
    pub progress: frankweiler_etl::progress::Progress,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            out_dir: PathBuf::new(),
            scopes: DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect(),
            refresh_window_days: 30,
            max_mrs: None,
            single_mr: None,
            full_sync: false,
            sleep_between: Duration::ZERO,
            progress: frankweiler_etl::progress::Progress::noop(),
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FetchSummary {
    pub new_mrs: usize,
    pub upd_mrs: usize,
    pub new_discussions: usize,
    pub upd_discussions: usize,
    pub requests: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncState {
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
    // GitLab `updated_after` takes ISO 8601.
    Some(since.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn key_self(rec: &Value) -> String {
    rec.get("user_id")
        .and_then(|v| v.as_i64())
        .map(|n| n.to_string())
        .unwrap_or_default()
}
fn key_mr(rec: &Value) -> String {
    format!(
        "{}!{}",
        rec.get("project_full_path")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        rec.get("mr_iid").and_then(|v| v.as_i64()).unwrap_or(0)
    )
}
fn key_discussion(rec: &Value) -> String {
    format!(
        "{}!{}#{}",
        rec.get("project_full_path")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        rec.get("mr_iid").and_then(|v| v.as_i64()).unwrap_or(0),
        rec.get("discussion_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
    )
}

fn project_full_path_from_web_url(web_url: &str) -> Option<String> {
    // e.g. https://gitlab.com/generally-intelligent/generally_intelligent/-/merge_requests/7643
    let rest = web_url.strip_prefix("https://gitlab.com/")?;
    let (path, _) = rest.split_once("/-/")?;
    Some(path.to_string())
}

fn make_mr_record(data: &Value) -> Option<Value> {
    let proj = data
        .get("web_url")
        .and_then(|v| v.as_str())
        .and_then(project_full_path_from_web_url)?;
    let iid = data.get("iid").and_then(|v| v.as_u64())?;
    let mut k = Map::new();
    k.insert("project_full_path".into(), Value::String(proj));
    k.insert("mr_iid".into(), Value::from(iid));
    k.insert(
        "project_id".into(),
        data.get("project_id").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "web_url".into(),
        data.get("web_url").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "state".into(),
        data.get("state").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "merged_at".into(),
        data.get("merged_at").cloned().unwrap_or(Value::Null),
    );
    let diff_refs = data.get("diff_refs").cloned().unwrap_or(Value::Null);
    k.insert(
        "head_sha".into(),
        diff_refs.get("head_sha").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "base_sha".into(),
        diff_refs.get("base_sha").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "start_sha".into(),
        diff_refs.get("start_sha").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "source_branch".into(),
        data.get("source_branch").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "target_branch".into(),
        data.get("target_branch").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "updated_at".into(),
        data.get("updated_at").cloned().unwrap_or(Value::Null),
    );
    Some(make_record(k, data.clone()))
}

fn make_discussion_record(proj: &str, iid: u32, d: &Value) -> Value {
    let mut k = Map::new();
    k.insert("project_full_path".into(), Value::String(proj.into()));
    k.insert("mr_iid".into(), Value::from(iid));
    k.insert(
        "discussion_id".into(),
        d.get("id").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "individual_note".into(),
        d.get("individual_note").cloned().unwrap_or(Value::Null),
    );
    // Surface the most recent note timestamp so diff_and_save can detect updates.
    let max_updated = d
        .get("notes")
        .and_then(|n| n.as_array())
        .and_then(|arr| {
            arr.iter()
                .filter_map(|n| n.get("updated_at").and_then(|v| v.as_str()))
                .max()
                .map(|s| Value::String(s.to_string()))
        })
        .unwrap_or(Value::Null);
    k.insert("max_note_updated_at".into(), max_updated);
    make_record(k, d.clone())
}

async fn fetch_self(client: &GitLabClient, out_dir: &Path) -> Result<i64> {
    let (data, _) = client.get(&format!("{BASE}/user")).await?;
    let obj = data.as_object().context("/user returned non-object")?;
    let mut k = Map::new();
    k.insert(
        "user_id".into(),
        obj.get("id").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "username".into(),
        obj.get("username").cloned().unwrap_or(Value::Null),
    );
    k.insert(
        "web_url".into(),
        obj.get("web_url").cloned().unwrap_or(Value::Null),
    );
    let rec = make_record(k, data.clone());
    let existing = load_latest_by_key(out_dir, ENTITY_SELF, key_self)?;
    diff_and_save(out_dir, ENTITY_SELF, &[rec], &existing, key_self)?;
    Ok(obj.get("id").and_then(|v| v.as_i64()).unwrap_or(0))
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
    state: &SyncState,
    refresh_window_days: u32,
    full: bool,
) -> Result<(Vec<(String, u32)>, HashMap<String, String>)> {
    let mut seen: std::collections::BTreeSet<(String, u32)> = Default::default();
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
            if iid > 0 {
                seen.insert((proj, iid as u32));
            }
        }
        new_state.insert(
            scope.clone(),
            Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        );
        tracing::info!(scope, count = results.len(), "scope done");
    }
    Ok((seen.into_iter().collect(), new_state))
}

async fn fetch_one_mr(
    client: &GitLabClient,
    out_dir: &Path,
    proj: &str,
    iid: u32,
    existing_mrs: &mut HashMap<String, Value>,
    existing_disc: &mut HashMap<String, Value>,
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
    let Some(mr_rec) = make_mr_record(&mr_data) else {
        tracing::error!(proj, iid, "MR record missing project/iid");
        return Ok(());
    };
    let counts = diff_and_save(
        out_dir,
        ENTITY_MR,
        std::slice::from_ref(&mr_rec),
        existing_mrs,
        key_mr,
    )?;
    summary.new_mrs += counts.new;
    summary.upd_mrs += counts.updated;
    existing_mrs.insert(key_mr(&mr_rec), mr_rec);

    let disc_url =
        format!("{BASE}/projects/{pid}/merge_requests/{iid}/discussions?per_page={PER_PAGE}");
    let discussions = client.paginate(&disc_url).await.unwrap_or_default();
    let disc_recs: Vec<Value> = discussions
        .iter()
        .map(|d| make_discussion_record(proj, iid, d))
        .collect();
    if !disc_recs.is_empty() {
        let counts = diff_and_save(
            out_dir,
            ENTITY_DISCUSSION,
            &disc_recs,
            existing_disc,
            key_discussion,
        )?;
        summary.new_discussions += counts.new;
        summary.upd_discussions += counts.updated;
        for r in &disc_recs {
            existing_disc.insert(key_discussion(r), r.clone());
        }
    }
    Ok(())
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    std::fs::create_dir_all(&opts.out_dir)
        .with_context(|| format!("create {}", opts.out_dir.display()))?;
    auto_set_latchkey_curl();
    let client = GitLabClient::new();
    let mut summary = FetchSummary::default();

    let user_id = fetch_self(&client, &opts.out_dir).await?;

    let mut existing_mrs = load_latest_by_key(&opts.out_dir, ENTITY_MR, key_mr)?;
    let mut existing_disc = load_latest_by_key(&opts.out_dir, ENTITY_DISCUSSION, key_discussion)?;

    let mr_keys: Vec<(String, u32)> = if let Some(s) = &opts.single_mr {
        vec![s.clone()]
    } else {
        let state = load_sync_state(&opts.out_dir);
        let (keys, new_scope_state) = discover_mrs(
            &client,
            user_id,
            &opts.scopes,
            &state,
            opts.refresh_window_days,
            opts.full_sync || existing_mrs.is_empty(),
        )
        .await?;
        let mut merged = state;
        for (k, v) in new_scope_state {
            merged.scopes.insert(k, v);
        }
        save_sync_state(&opts.out_dir, &merged)?;
        keys
    };
    let mr_keys: Vec<(String, u32)> = if let Some(cap) = opts.max_mrs {
        mr_keys.into_iter().take(cap).collect()
    } else {
        mr_keys
    };
    tracing::info!(count = mr_keys.len(), "MRs to fetch");

    opts.progress.set_length(Some(mr_keys.len() as u64));
    for (proj, iid) in &mr_keys {
        opts.progress.inc(1);
        opts.progress.set_message(&format!("{proj}!{iid}"));
        if let Err(e) = fetch_one_mr(
            &client,
            &opts.out_dir,
            proj,
            *iid,
            &mut existing_mrs,
            &mut existing_disc,
            &mut summary,
        )
        .await
        {
            tracing::error!(proj, iid, error = %e, "MR fetch failed; skipping");
        }
        if opts.sleep_between > Duration::ZERO {
            tokio::time::sleep(opts.sleep_between).await;
        }
    }

    summary.requests = client.request_count();
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
}
