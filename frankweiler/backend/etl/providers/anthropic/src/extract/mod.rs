//! Anthropic (claude.ai) downloader entry point. Port of
//! `src/download/claude_web.py`.
//!
//! Writes into a single doltlite database file
//! (`<data_root>/raw/<name>.doltlite_db`). Conversations are stored as
//! the **raw** `/api/...` payload — the export-shape normalization
//! used to happen here at fetch time, but now lives in `translate`
//! so the raw store stays as close to the wire as possible.

pub mod api;
pub mod db;
pub mod normalize;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use frankweiler_etl::blobs::safe_filename;
use frankweiler_etl::latchkey::latchkey_tokio_command;
use serde_json::{json, Value};
use tokio::time::sleep;
use tracing::{info, info_span, instrument, warn, Instrument};

pub use api::{ClaudeClient, ClaudeError};
pub use db::{block_on_load_all, db_path_for, BlobBytes, LoadedConversation, LoadedRaw, RawDb};

pub const SLEEP_BETWEEN: Duration = Duration::from_millis(400);
pub const DEFAULT_OVERLAP: usize = 3;
const ATTACH_FILE_TIMEOUT: Duration = Duration::from_secs(600);
const CLAUDE_ORIGIN: &str = "https://claude.ai";

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    /// Path to the doltlite database file. Legacy directories are
    /// rewritten to `<dir>.doltlite_db`.
    pub db_path: PathBuf,
    /// Path to a bulk-export directory (`users.json` and friends). If
    /// set and the DB is missing users, we pre-seed them from here.
    pub export_dir: Option<PathBuf>,
    pub overlap: usize,
    pub sleep_between: Duration,
    /// When non-empty, fetch only these conversation UUIDs. The
    /// listing walk is skipped entirely.
    pub conv_uuids: Vec<String>,
    pub progress: frankweiler_etl::progress::Progress,
    /// Cross-provider knobs (`--reset-and-redownload`, etc).
    pub control: frankweiler_etl::control::ExtractControl,
}

#[derive(Debug, Default)]
pub struct FetchSummary {
    pub fetched: usize,
    pub skipped: usize,
    pub forbidden_orgs: usize,
    pub errors: usize,
    pub total: usize,
    pub new_blobs: usize,
    pub skipped_blobs: usize,
    pub failed_blobs: usize,
    pub requests: u64,
    pub network_seconds: f64,
}

#[instrument(skip_all, fields(db = %opts.db_path.display()))]
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db_path = db_path_for(&opts.db_path);
    let _ = frankweiler_etl::latchkey::ensure_curl_shim();
    let db = RawDb::open(&db_path)
        .await
        .with_context(|| format!("open raw db {}", db_path.display()))?;

    if opts.control.reset_and_redownload {
        info!(event = "anthropic_reset_and_redownload");
        db.reset().await.context("reset raw db before redownload")?;
    }

    let run_config = json!({
        "overlap": opts.overlap,
        "conv_uuids": opts.conv_uuids,
    });
    let run_id = db.start_run(&run_config).await?;
    let mut client = ClaudeClient::new();
    let mut summary = FetchSummary::default();

    let work = async {
        // users.json from the bulk export carries the account.uuid we
        // need on every conversation. If the DB doesn't have any user
        // yet, try to pull it from the export dir before falling back
        // to `/api/account`.
        if !db.has_any_user().await.unwrap_or(false) {
            if let Some(export_dir) = opts.export_dir.as_deref() {
                ingest_export_users(&db, export_dir)
                    .await
                    .unwrap_or_else(|e| {
                        warn!(event = "anthropic_export_users_failed", error = %e);
                    });
            }
        }
        if !db.has_any_user().await.unwrap_or(false) {
            match client.current_account().await {
                Ok(acct) => {
                    let entry = pick_user_fields(&acct);
                    if let Err(e) = db.upsert_user(&entry).await {
                        warn!(event = "anthropic_synthesize_user_failed", error = %e);
                    } else {
                        info!(event = "anthropic_users_synthesized");
                    }
                }
                Err(e) => warn!(
                    event = "anthropic_current_account_failed",
                    error = %e,
                    note = "users will be empty"
                ),
            }
        }

        let orgs = client
            .list_orgs()
            .await
            .map_err(|e| anyhow::anyhow!("list orgs: {e}"))?;
        info!(event = "anthropic_orgs", count = orgs.len());
        if let Err(e) = db.upsert_orgs(&orgs).await {
            warn!(event = "anthropic_orgs_upsert_failed", error = %e);
        }

        if !opts.conv_uuids.is_empty() {
            opts.progress.set_length(Some(opts.conv_uuids.len() as u64));
            for raw in &opts.conv_uuids {
                opts.progress.inc(1);
                opts.progress.set_message(raw);
                let target = frankweiler_etl::ids::normalize_id_token(raw);
                fetch_single(&mut client, &db, &orgs, &target, &mut summary).await?;
            }
            return Ok::<(), anyhow::Error>(());
        }

        // Walk each org's listing, pre-seed conversations, prioritize
        // missing-then-stale, fetch detail.
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

            let listing_refs: Vec<(&str, &Value)> = listing.iter().map(|c| (org_uuid, c)).collect();
            db.pre_seed_conversations(&listing_refs).await?;

            // Plan: classify each listing item against current DB state.
            let states = db.conversation_states().await?;
            let mut missing: Vec<&Value> = Vec::new();
            let mut stale: Vec<&Value> = Vec::new();
            // Apply overlap by re-fetching the top-N most-recently-updated.
            let mut overlap_force: HashSet<String> = HashSet::new();
            {
                let mut sorted: Vec<&Value> = listing.iter().collect();
                sorted.sort_by(|a, b| {
                    let ka = a.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
                    let kb = b.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
                    kb.cmp(ka)
                });
                for c in sorted.iter().take(opts.overlap) {
                    if let Some(u) = c.get("uuid").and_then(|v| v.as_str()) {
                        overlap_force.insert(u.into());
                    }
                }
            }
            let mut up_to_date: usize = 0;
            for item in &listing {
                let Some(uuid) = item.get("uuid").and_then(|v| v.as_str()) else {
                    continue;
                };
                let api_updated = item
                    .get("updated_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match states.get(uuid) {
                    Some(s) if s.has_payload && !overlap_force.contains(uuid) => {
                        if s.updated_at.as_deref().unwrap_or("") == api_updated {
                            up_to_date += 1;
                        } else {
                            stale.push(item);
                        }
                    }
                    _ => missing.push(item),
                }
            }
            info!(
                event = "anthropic_priority_split",
                missing = missing.len(),
                stale = stale.len(),
                up_to_date = up_to_date,
            );
            summary.skipped += up_to_date;

            let ordered: Vec<&Value> = missing.into_iter().chain(stale).collect();
            opts.progress.set_length(Some(ordered.len() as u64));
            for item in ordered {
                let Some(uuid) = item.get("uuid").and_then(|v| v.as_str()) else {
                    continue;
                };
                opts.progress.inc(1);
                opts.progress.set_message(&format!("{org_name} {uuid}"));
                match client.get_conversation(org_uuid, uuid).await {
                    Ok(full) => {
                        save_conversation(&db, org_uuid, uuid, &full).await?;
                        summary.fetched += 1;
                        fetch_files_for(&db, &full, uuid, &mut summary).await;
                        if opts.sleep_between > Duration::ZERO {
                            sleep(opts.sleep_between).await;
                        }
                    }
                    Err(e) => {
                        warn!(event = "anthropic_fetch_error", uuid = uuid, error = %e);
                        let _ = db.record_conversation_error(uuid, &e.to_string()).await;
                        summary.errors += 1;
                    }
                }
            }
        }
        Ok(())
    };

    let result = work.await;
    summary.total = summary.fetched + summary.skipped;
    summary.requests = client.requests;
    summary.network_seconds = client.network_seconds;
    let summary_json = json!({
        "fetched": summary.fetched,
        "skipped": summary.skipped,
        "forbidden_orgs": summary.forbidden_orgs,
        "errors": summary.errors,
        "total": summary.total,
        "new_blobs": summary.new_blobs,
        "skipped_blobs": summary.skipped_blobs,
        "failed_blobs": summary.failed_blobs,
        "requests": summary.requests,
        "error": result.as_ref().err().map(|e| e.to_string()),
    });
    let status = if result.is_ok() { "ok" } else { "error" };
    let _ = db.finish_run(run_id, status, &summary_json).await;
    result?;
    Ok(summary)
}

async fn fetch_single(
    client: &mut ClaudeClient,
    db: &RawDb,
    orgs: &[Value],
    conv_uuid: &str,
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
                save_conversation(db, org_uuid, conv_uuid, &full).await?;
                summary.fetched += 1;
                info!(
                    event = "anthropic_fetch_single_ok",
                    uuid = conv_uuid,
                    org = %org_name
                );
                fetch_files_for(db, &full, conv_uuid, summary).await;
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
                let _ = db
                    .record_conversation_error(conv_uuid, &e.to_string())
                    .await;
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

async fn save_conversation(db: &RawDb, org_uuid: &str, uuid: &str, full: &Value) -> Result<()> {
    let payload = serde_json::to_string(full).context("serialize conversation")?;
    let name = full.get("name").and_then(|v| v.as_str()).map(String::from);
    let updated_at = full
        .get("updated_at")
        .and_then(|v| v.as_str())
        .map(String::from);
    db.upsert_conversation_detail(&db::ConversationDetail {
        id: uuid.to_string(),
        org_uuid: org_uuid.to_string(),
        name,
        updated_at,
        payload,
    })
    .await
}

/// Pull `users.json` entries from an existing bulk-export directory
/// into the DB. Best-effort: missing file is fine.
async fn ingest_export_users(db: &RawDb, export_dir: &Path) -> Result<()> {
    let path = export_dir.join("users.json");
    if !path.exists() {
        return Ok(());
    }
    let txt = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let v: Value =
        serde_json::from_str(&txt).with_context(|| format!("parse {}", path.display()))?;
    if let Some(arr) = v.as_array() {
        if let Err(e) = db.upsert_users(arr).await {
            warn!(event = "anthropic_users_upsert_failed", error = %e);
        }
    }
    Ok(())
}

fn pick_user_fields(acct: &Value) -> Value {
    let mut obj = serde_json::Map::new();
    for key in ["uuid", "email_address", "full_name"] {
        if let Some(v) = acct.get(key) {
            obj.insert(key.into(), v.clone());
        }
    }
    Value::Object(obj)
}

/// Walk a conversation tree's `chat_messages[*].files[]` and
/// download every unique attachment into the doltlite `blobs` table.
/// Skips files we already have bytes for.
async fn fetch_files_for(db: &RawDb, conv: &Value, conv_uuid: &str, summary: &mut FetchSummary) {
    let messages = match conv.get("chat_messages").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return,
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut targets: Vec<Value> = Vec::new();
    for msg in messages {
        if let Some(files) = msg.get("files").and_then(|v| v.as_array()) {
            for f in files {
                if let Some(id) = f.get("file_uuid").and_then(|v| v.as_str()) {
                    if seen.insert(id.to_string()) {
                        targets.push(f.clone());
                    }
                }
            }
        }
    }
    for f in &targets {
        let Some(file_uuid) = f.get("file_uuid").and_then(|v| v.as_str()) else {
            continue;
        };
        if db.blob_exists(file_uuid).await.unwrap_or(false) {
            summary.skipped_blobs += 1;
            continue;
        }
        match download_one_file(db, f, conv_uuid).await {
            Ok(true) => summary.new_blobs += 1,
            Ok(false) => summary.failed_blobs += 1,
            Err(e) => {
                warn!(event = "anthropic_media_unexpected_err", file_uuid = %file_uuid, error = %e);
                let _ = db
                    .record_blob_error(file_uuid, conv_uuid, "file", &e.to_string())
                    .await;
                summary.failed_blobs += 1;
            }
        }
    }
}

async fn download_one_file(db: &RawDb, file_obj: &Value, conv_uuid: &str) -> Result<bool> {
    let Some(file_uuid) = file_obj.get("file_uuid").and_then(|v| v.as_str()) else {
        return Ok(false);
    };
    let preview_path = file_obj
        .get("preview_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            file_obj
                .get("document_asset")
                .and_then(|d| d.get("url"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        });
    let preview_path = match preview_path {
        Some(p) => p,
        None => {
            warn!(
                event = "anthropic_media_no_preview_url",
                file_uuid = file_uuid
            );
            let _ = db
                .record_blob_error(file_uuid, conv_uuid, "file", "no preview_url")
                .await;
            return Ok(false);
        }
    };
    let url = if preview_path.starts_with("http") {
        preview_path.to_string()
    } else {
        format!("{CLAUDE_ORIGIN}{preview_path}")
    };
    let name = file_obj.get("file_name").and_then(|v| v.as_str());
    let _safe = safe_filename(name, file_uuid); // sanity-check the input shape
    let mime = file_obj
        .get("file_kind")
        .and_then(|v| v.as_str())
        .or_else(|| file_obj.get("mime_type").and_then(|v| v.as_str()));

    let tmp = tempfile::NamedTempFile::new().context("create blob tempfile")?;
    let mut cmd = latchkey_tokio_command();
    cmd.arg("curl")
        .arg("-fSL")
        .arg("-o")
        .arg(tmp.path())
        .arg(&url);
    let proc = tokio::time::timeout(ATTACH_FILE_TIMEOUT, cmd.output())
        .await
        .context("file curl timed out")?
        .context("file curl spawn failed")?;
    if !proc.status.success() {
        let stderr_full = String::from_utf8_lossy(&proc.stderr).into_owned();
        let tail: String = stderr_full
            .chars()
            .rev()
            .take(200)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        warn!(
            event = "anthropic_media_failed",
            file_uuid = file_uuid,
            exit = proc.status.code().unwrap_or(-1),
            stderr = %tail.trim(),
        );
        let _ = db
            .record_blob_error(file_uuid, conv_uuid, "file", tail.trim())
            .await;
        return Ok(false);
    }
    let bytes = fs::read(tmp.path()).with_context(|| format!("read tempfile for {file_uuid}"))?;
    db.upsert_blob_bytes(
        file_uuid,
        "file",
        conv_uuid,
        "file",
        mime,
        &bytes,
        Some(&url),
    )
    .await?;
    Ok(true)
}
