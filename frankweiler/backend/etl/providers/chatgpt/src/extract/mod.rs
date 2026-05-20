//! ChatGPT downloader entry point. Port of `src/download/chatgpt_web.py`.
//!
//! On-disk layout (matches the Python downloader byte-for-byte):
//!
//! ```text
//! <out>/
//!   me.json                       # current user profile
//!   conversations.json            # combined paginated listing index
//!   conversations/<id>.json       # per-conversation full tree
//! ```
//!
//! Each per-conversation JSON gets two synthetic keys stamped in by us:
//! `_fetched_at` (provenance) and `_listing_update_time` (so the next
//! run can do a string-equality skip check against the listing
//! endpoint, which returns ISO-8601 strings while the detail endpoint
//! returns Unix-epoch floats).
//!
//! Auth + Cloudflare clearance is delegated to `latchkey curl` with
//! `LATCHKEY_CURL=/path/to/curl_impersonate-chrome` — no Rust-side TLS
//! fingerprinting code. See the crate's `EXTRACT.md`.

pub mod api;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Local;
use serde_json::Value;
use tokio::time::sleep;
use tracing::{info, info_span, instrument, warn, Instrument};

use api::{ChatGPTClient, ChatGPTError};

/// Inter-fetch sleep. ChatGPT doesn't appear to throttle us at any
/// polite rate; 100ms keeps us from looking like a tight loop without
/// doubling per-conv latency on top of ~400ms GETs.
pub const SLEEP_BETWEEN: Duration = Duration::from_millis(100);
pub const PAGE_SIZE: usize = 100;

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    pub out_dir: PathBuf,
    pub max_pages: Option<usize>,
    pub limit: Option<usize>,
    pub sleep_between: Duration,
    /// When non-empty, fetch only these conversation ids and write
    /// each to `<out>/conversations/<id>.json`. Skips the paginated
    /// listing walk; `me.json` is still fetched (cheap, captures
    /// account id).
    pub conv_uuids: Vec<String>,
    /// Override the `_fetched_at` provenance stamp. When `None`, the
    /// extractor uses `Local::now()`. The sync orchestrator passes its
    /// `--now` value here so deterministic builds get a stable stamp.
    pub fetched_at: Option<String>,
    pub progress: frankweiler_etl::progress::Progress,
}

#[derive(Debug, Default)]
pub struct FetchSummary {
    pub fetched: usize,
    pub skipped: usize,
    pub errors: usize,
    pub listing: usize,
    pub requests: u64,
    pub network_seconds: f64,
}

#[instrument(skip_all, fields(out = %opts.out_dir.display()))]
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let out_dir = opts.out_dir.clone();
    std::fs::create_dir_all(&out_dir).with_context(|| format!("mkdir {}", out_dir.display()))?;
    let convs_dir = out_dir.join("conversations");
    std::fs::create_dir_all(&convs_dir)
        .with_context(|| format!("mkdir {}", convs_dir.display()))?;
    let index_path = out_dir.join("conversations.json");
    let me_path = out_dir.join("me.json");
    let started_at = opts
        .fetched_at
        .clone()
        .unwrap_or_else(|| Local::now().to_rfc3339());

    let mut client = ChatGPTClient::new();

    let me = client
        .me()
        .await
        .map_err(|e| anyhow::anyhow!("fetch /me: {e}"))?;
    write_json(&me_path, &me)?;
    info!(
        event = "chatgpt_me",
        email = me.get("email").and_then(|v| v.as_str()).unwrap_or(""),
        id = me.get("id").and_then(|v| v.as_str()).unwrap_or(""),
    );

    if !opts.conv_uuids.is_empty() {
        let mut summary = FetchSummary::default();
        opts.progress.set_length(Some(opts.conv_uuids.len() as u64));
        for raw in &opts.conv_uuids {
            opts.progress.inc(1);
            opts.progress.set_message(raw);
            // Accept either a bare id or a `https://chatgpt.com/c/<id>`
            // URL. Normalized at the last moment so logs/progress show
            // the user's literal input.
            let target = frankweiler_etl::ids::normalize_id_token(raw);
            let cache_path = convs_dir.join(format!("{target}.json"));
            match client.get_conversation(&target).await {
                Ok(mut full) => {
                    if let Some(obj) = full.as_object_mut() {
                        obj.insert("_fetched_at".into(), Value::String(started_at.clone()));
                    }
                    write_json(&cache_path, &full)?;
                    summary.fetched += 1;
                    info!(event = "chatgpt_fetch_single_ok", raw = raw, id = %target);
                }
                Err(e) => {
                    warn!(event = "chatgpt_fetch_error", raw = raw, id = %target, error = %e);
                    return Err(anyhow::anyhow!("fetch {raw}: {e}"));
                }
            }
        }
        summary.requests = client.requests;
        summary.network_seconds = client.network_seconds;
        return Ok(summary);
    }

    opts.progress.set_message("listing conversations");
    let listing = list_all_conversations(&mut client, opts.max_pages, &opts.progress)
        .instrument(info_span!("chatgpt_list"))
        .await?;
    info!(event = "chatgpt_listing", convs = listing.len());

    // Prioritize missing conversations so a 429 budget gets spent
    // moving forward rather than revalidating cache hits.
    let (missing, present): (Vec<_>, Vec<_>) =
        listing
            .iter()
            .partition(|item| match item.get("id").and_then(|v| v.as_str()) {
                Some(id) => !convs_dir.join(format!("{id}.json")).exists(),
                None => false,
            });
    info!(
        event = "chatgpt_priority_split",
        missing = missing.len(),
        present = present.len()
    );

    let mut summary = FetchSummary {
        listing: listing.len(),
        ..Default::default()
    };

    let ordered: Vec<&Value> = missing.into_iter().chain(present).collect();
    opts.progress.set_length(Some(ordered.len() as u64));
    for item in ordered {
        opts.progress.inc(1);
        if let Some(limit) = opts.limit {
            if summary.fetched + summary.errors >= limit {
                info!(event = "chatgpt_limit_reached", limit = limit);
                break;
            }
        }
        let Some(cid) = item.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        opts.progress.set_message(cid);
        let api_update = item.get("update_time").cloned().unwrap_or(Value::Null);
        let cache_path = convs_dir.join(format!("{cid}.json"));
        if let Some(cached) = read_json(&cache_path)? {
            if cached
                .get("_listing_update_time")
                .cloned()
                .unwrap_or(Value::Null)
                == api_update
            {
                summary.skipped += 1;
                continue;
            }
        }

        match client.get_conversation(cid).await {
            Ok(mut full) => {
                if let Some(obj) = full.as_object_mut() {
                    obj.insert("_fetched_at".into(), Value::String(started_at.clone()));
                    obj.insert("_listing_update_time".into(), api_update);
                }
                write_json(&cache_path, &full)?;
                summary.fetched += 1;
                if opts.sleep_between > Duration::ZERO {
                    sleep(opts.sleep_between).await;
                }
            }
            Err(ChatGPTError::RateLimited { path, waited_secs }) => {
                warn!(
                    event = "chatgpt_rate_limit_giveup",
                    path = %path,
                    waited_secs = waited_secs,
                    fetched = summary.fetched,
                );
                break;
            }
            Err(ChatGPTError::Permanent(msg)) => {
                warn!(event = "chatgpt_fetch_error", cid = cid, error = %msg);
                summary.errors += 1;
            }
        }
    }

    write_json(&index_path, &Value::Array(listing))?;

    summary.requests = client.requests;
    summary.network_seconds = client.network_seconds;
    Ok(summary)
}

#[instrument(skip(client))]
async fn list_all_conversations(
    client: &mut ChatGPTClient,
    max_pages: Option<usize>,
    progress: &frankweiler_etl::progress::Progress,
) -> Result<Vec<Value>> {
    let mut items: Vec<Value> = Vec::new();
    let mut offset = 0usize;
    let mut pages = 0usize;
    loop {
        let page = client
            .list_conversations_page(offset, PAGE_SIZE)
            .await
            .map_err(|e| anyhow::anyhow!("list page offset={offset}: {e}"))?;
        let page_items: Vec<Value> = page
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let total = page.get("total").and_then(|v| v.as_u64());
        info!(
            event = "chatgpt_listing_page",
            offset = offset,
            got = page_items.len(),
            total = total.unwrap_or(0),
            cum = items.len() + page_items.len(),
        );
        let got = page_items.len();
        items.extend(page_items);
        offset += got;
        pages += 1;
        progress.set_message(&format!("listing page {pages}, {} convs", items.len()));
        if got == 0 {
            break;
        }
        if let Some(t) = total {
            if offset as u64 >= t {
                break;
            }
        }
        if let Some(cap) = max_pages {
            if pages >= cap {
                info!(event = "chatgpt_listing_capped", max_pages = cap);
                break;
            }
        }
        sleep(SLEEP_BETWEEN).await;
    }
    Ok(items)
}

fn read_json(path: &Path) -> Result<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(Some(v))
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    }
    // Match Python's `json.dumps(indent=2, ensure_ascii=False)` shape.
    let text = serde_json::to_string_pretty(value)
        .with_context(|| format!("serialize {}", path.display()))?;
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}
