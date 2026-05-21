//! Anthropic downloader entry point. Port of
//! `src/download/claude_web.py`.
//!
//! TODO(anthropic-incremental): unlike the Slack split (download →
//! render → load with a `source_fingerprint` stamped on date-stamped
//! outputs that skip re-write when unchanged), anthropic's extract
//! still overwrites `conversations.json` and `users.json` wholesale on
//! every run. The `/api/account` call is at least gated on
//! users.json's existence, but the conversation index is a full merge +
//! rewrite. Worth revisiting when we port anthropic to the same
//! binary-split pattern.
//!
//! On-disk layout matches the Python downloader so the existing
//! fixture / translator path is reused unchanged:
//!
//! ```text
//! <out>/
//!   conversations.json    # array of conversations in export shape
//!   users.json            # copied from --export-dir if present
//! ```
//!
//! Per-conversation JSON inside `conversations.json` is coerced into
//! the export shape via [`normalize::normalize_to_export_shape`].

pub mod api;
pub mod normalize;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::time::sleep;
use tracing::{info, info_span, instrument, warn, Instrument};

use api::{download_files_for_conversation, ClaudeClient, ClaudeError};

pub const SLEEP_BETWEEN: Duration = Duration::from_millis(400);
pub const DEFAULT_OVERLAP: usize = 3;

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    pub out_dir: PathBuf,
    pub export_dir: Option<PathBuf>,
    pub overlap: usize,
    pub sleep_between: Duration,
    /// When non-empty, fetch only these conversation UUIDs and merge
    /// them into the existing cache. The listing/priority walk is
    /// skipped entirely.
    pub conv_uuids: Vec<String>,
    pub progress: frankweiler_etl::progress::Progress,
}

#[derive(Debug, Default)]
pub struct FetchSummary {
    pub fetched: usize,
    pub skipped: usize,
    pub forbidden_orgs: usize,
    pub errors: usize,
    pub total: usize,
    pub requests: u64,
    pub network_seconds: f64,
}

#[instrument(skip_all, fields(out = %opts.out_dir.display()))]
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let out_dir = opts.out_dir.clone();
    std::fs::create_dir_all(&out_dir).with_context(|| format!("mkdir {}", out_dir.display()))?;
    let out_conv_path = out_dir.join("conversations.json");
    let out_users_path = out_dir.join("users.json");

    let existing_export: HashMap<String, Value> = opts
        .export_dir
        .as_deref()
        .map(|p| load_conv_index(&p.join("conversations.json")))
        .transpose()?
        .unwrap_or_default();
    let existing_api: HashMap<String, Value> = load_conv_index(&out_conv_path)?;
    info!(
        event = "anthropic_existing",
        export = existing_export.len(),
        api = existing_api.len()
    );

    // Overlap: N most-recently-updated export conversations get refetched
    // for cross-check vs. the live API.
    let mut export_sorted: Vec<&Value> = existing_export.values().collect();
    export_sorted.sort_by(|a, b| {
        let ka = a.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
        kb.cmp(ka)
    });
    let overlap_uuids: HashSet<String> = export_sorted
        .iter()
        .take(opts.overlap)
        .filter_map(|c| c.get("uuid").and_then(|v| v.as_str()).map(String::from))
        .collect();
    info!(
        event = "anthropic_overlap",
        count = overlap_uuids.len(),
        of = opts.overlap
    );

    // users.json: copy from export verbatim if it exists and we don't
    // already have one. Preserves account_uuid for shape coercion.
    if let Some(export_dir) = opts.export_dir.as_deref() {
        let src = export_dir.join("users.json");
        if src.exists() && !out_users_path.exists() {
            std::fs::copy(&src, &out_users_path).with_context(|| {
                format!("copy {} -> {}", src.display(), out_users_path.display())
            })?;
        }
    }

    let mut client = ClaudeClient::new();

    // If we still don't have a users.json (no --export-dir, or its export
    // didn't ship one), synthesize one from `GET /api/account`. The
    // translator hard-requires users.json, and this is what claude.ai's
    // own web app uses to identify the current user.
    if !out_users_path.exists() {
        match client.current_account().await {
            Ok(acct) => {
                let entry = pick_user_fields(&acct);
                write_json(&out_users_path, &Value::Array(vec![entry]))?;
                info!(event = "anthropic_users_json_synthesized");
            }
            Err(e) => {
                warn!(
                    event = "anthropic_current_account_failed",
                    error = %e,
                    note = "could not fetch /api/account — users.json will be absent"
                );
            }
        }
    }

    let account_uuid = account_uuid_from_users(opts.export_dir.as_deref())
        .or_else(|| account_uuid_from_users(Some(out_dir.as_path())));
    if account_uuid.is_none() {
        warn!(
            event = "anthropic_no_account_uuid",
            note = "users.json missing — conversation.account.uuid will be empty"
        );
    }
    let orgs = client
        .list_orgs()
        .await
        .map_err(|e| anyhow::anyhow!("list orgs: {e}"))?;
    info!(event = "anthropic_orgs", count = orgs.len());

    let mut merged: HashMap<String, Value> = existing_api.clone();
    let mut summary = FetchSummary::default();

    if !opts.conv_uuids.is_empty() {
        opts.progress.set_length(Some(opts.conv_uuids.len() as u64));
        for raw in &opts.conv_uuids {
            opts.progress.inc(1);
            opts.progress.set_message(raw);
            // Accept either a bare UUID or a `https://claude.ai/chat/<uuid>`
            // URL. Normalize as late as possible so the user's original
            // input survives in logs and progress messages.
            let target = frankweiler_etl::ids::normalize_id_token(raw);
            fetch_single(
                &mut client,
                &orgs,
                &target,
                account_uuid.as_deref(),
                &out_dir.join("media"),
                &mut merged,
                &mut summary,
            )
            .await?;
        }
    } else {
        opts.progress.set_message("listing orgs");
        for org in &orgs {
            let Some(org_uuid) = org.get("uuid").and_then(|v| v.as_str()) else {
                continue;
            };
            let org_name = org
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&org_uuid[..org_uuid.len().min(8)])
                .to_string();

            let listing = match client
                .list_conversations(org_uuid)
                .instrument(info_span!("anthropic_org_listing", org = %org_name))
                .await
            {
                Ok(l) => l,
                Err(ClaudeError::Forbidden(_)) => {
                    info!(
                        event = "anthropic_org_forbidden",
                        org = %org_name,
                        note = "no chat permission for this org"
                    );
                    summary.forbidden_orgs += 1;
                    continue;
                }
                Err(e) => return Err(anyhow::anyhow!("list conversations for {org_name}: {e}")),
            };
            info!(
                event = "anthropic_org_listing_count",
                org = %org_name,
                count = listing.len()
            );
            sleep(SLEEP_BETWEEN).await;

            // Plan: classify each listing item, sort so genuinely-new
            // conversations come first.
            let mut plan: Vec<(u8, &Value)> = Vec::new();
            for item in &listing {
                let Some(uuid) = item.get("uuid").and_then(|v| v.as_str()) else {
                    continue;
                };
                let api_updated = item
                    .get("updated_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let in_export = existing_export.get(uuid);
                let in_api = existing_api.get(uuid);
                let priority: Option<u8> = if in_export.is_none() && in_api.is_none() {
                    Some(0) // new
                } else if overlap_uuids.contains(uuid) {
                    Some(1) // overlap
                } else if let Some(api_prev) = in_api {
                    if api_prev
                        .get("updated_at")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        != api_updated
                    {
                        Some(2) // updated
                    } else {
                        None
                    }
                } else if let Some(exp_prev) = in_export {
                    if exp_prev
                        .get("updated_at")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        != api_updated
                    {
                        Some(3) // export-stale
                    } else {
                        None
                    }
                } else {
                    None
                };
                match priority {
                    Some(p) => plan.push((p, item)),
                    None => summary.skipped += 1,
                }
            }
            plan.sort_by_key(|(p, _)| *p);

            opts.progress.set_length(Some(plan.len() as u64));
            for (_why, item) in plan {
                let Some(uuid) = item.get("uuid").and_then(|v| v.as_str()) else {
                    continue;
                };
                opts.progress.inc(1);
                opts.progress.set_message(&format!("{org_name} {uuid}"));
                match client.get_conversation(org_uuid, uuid).await {
                    Ok(full) => {
                        let media_dir = out_dir.join("media");
                        let _ = download_files_for_conversation(&full, &media_dir).await;
                        let normalized = normalize::normalize_to_export_shape(
                            full,
                            account_uuid.as_deref(),
                            org_uuid,
                        );
                        merged.insert(uuid.to_string(), normalized);
                        summary.fetched += 1;
                        if opts.sleep_between > Duration::ZERO {
                            sleep(opts.sleep_between).await;
                        }
                    }
                    Err(e) => {
                        warn!(event = "anthropic_fetch_error", uuid = uuid, error = %e);
                        summary.errors += 1;
                    }
                }
            }
        }
    }

    let mut sorted: Vec<Value> = merged.into_values().collect();
    sorted.sort_by(|a, b| {
        let ka = a.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
        kb.cmp(ka)
    });
    summary.total = sorted.len();
    write_json(&out_conv_path, &Value::Array(sorted))?;
    summary.requests = client.requests;
    summary.network_seconds = client.network_seconds;
    Ok(summary)
}

async fn fetch_single(
    client: &mut ClaudeClient,
    orgs: &[Value],
    conv_uuid: &str,
    account_uuid: Option<&str>,
    media_dir: &Path,
    merged: &mut HashMap<String, Value>,
    summary: &mut FetchSummary,
) -> Result<()> {
    for org in orgs {
        let Some(org_uuid) = org.get("uuid").and_then(|v| v.as_str()) else {
            continue;
        };
        let org_name = org
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&org_uuid[..org_uuid.len().min(8)])
            .to_string();
        match client.get_conversation(org_uuid, conv_uuid).await {
            Ok(full) => {
                let _ = download_files_for_conversation(&full, media_dir).await;
                let normalized = normalize::normalize_to_export_shape(full, account_uuid, org_uuid);
                merged.insert(conv_uuid.to_string(), normalized);
                summary.fetched += 1;
                info!(
                    event = "anthropic_fetch_single_ok",
                    uuid = conv_uuid,
                    org = %org_name
                );
                return Ok(());
            }
            Err(ClaudeError::Forbidden(_)) => {
                info!(
                    event = "anthropic_fetch_single_forbidden",
                    uuid = conv_uuid,
                    org = %org_name
                );
                continue;
            }
            Err(ClaudeError::Permanent(msg)) if msg.contains("HTTP 404") => {
                info!(
                    event = "anthropic_fetch_single_not_in_org",
                    uuid = conv_uuid,
                    org = %org_name
                );
                continue;
            }
            Err(e) => {
                warn!(event = "anthropic_fetch_error", uuid = conv_uuid, error = %e);
                summary.errors += 1;
                return Err(anyhow::anyhow!("fetch {conv_uuid}: {e}"));
            }
        }
    }
    Err(anyhow::anyhow!(
        "conversation {conv_uuid} not found in any of {} org(s)",
        orgs.len()
    ))
}

fn load_conv_index(path: &Path) -> Result<HashMap<String, Value>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    let arr = v.as_array().cloned().unwrap_or_default();
    let mut out = HashMap::new();
    for c in arr {
        if let Some(uuid) = c.get("uuid").and_then(|v| v.as_str()) {
            out.insert(uuid.to_string(), c);
        }
    }
    Ok(out)
}

/// Pull just the fields the translator (and our parse step) reads off a
/// user record: `uuid`, `email_address`, `full_name`. `/api/account` also
/// returns memberships + settings, which we drop — they're orthogonal to
/// the accounts table and would bloat users.json.
fn pick_user_fields(acct: &Value) -> Value {
    let mut obj = serde_json::Map::new();
    for key in ["uuid", "email_address", "full_name"] {
        if let Some(v) = acct.get(key) {
            obj.insert(key.into(), v.clone());
        }
    }
    Value::Object(obj)
}

fn account_uuid_from_users(dir: Option<&Path>) -> Option<String> {
    let dir = dir?;
    let p = dir.join("users.json");
    if !p.exists() {
        return None;
    }
    let text = std::fs::read_to_string(&p).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let arr = v.as_array()?;
    arr.first()?
        .get("uuid")
        .and_then(|v| v.as_str())
        .map(String::from)
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(value)
        .with_context(|| format!("serialize {}", path.display()))?;
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}
