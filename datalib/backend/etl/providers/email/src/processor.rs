//! Program A `DataProcessor`s for the email source.

use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::http::LatchkeySettings;
use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};

use datalib_etl_email_config::{EmailConfig, EmailGmailApi, EmailLiveMode, EmailSync, MboxSync};

use crate::download;

/// Download wave: present iff managed — a live block (`sync:` for JMAP,
/// `gmail_api:` for the Gmail REST API) selects a server mode; else an
/// `.mbox` under input_path → mbox mode.
pub fn plan_download(ctx: PlanContext, config: EmailConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    if config.outlink_format.is_some() || !config.only_render_labels.is_empty() {
        anyhow::bail!(
            "email `outlink_format` / `only_render_labels` are render knobs — \
             put them in the render step's params instead"
        );
    }
    let raw_path = config.common.raw_path().to_path_buf();
    let input_path = config.common.input_or_raw_path().to_path_buf();
    let blob_size_limit_bytes = config.common.blob_size_limit_bytes;
    let latchkey = config.latchkey_settings.clone();

    // Live-server modes are declared explicitly and are mutually
    // exclusive (`live_mode` enforces that); the file-backed mbox mode is
    // the fallback, chosen by probing the filesystem — which is why it
    // isn't a `live_mode` variant.
    let mode = match config.live_mode()? {
        Some(EmailLiveMode::Jmap(sync)) => Some(ExtractMode::Jmap(sync.clone())),
        Some(EmailLiveMode::GmailApi(gmail)) => Some(ExtractMode::GmailApi(gmail.clone())),
        None => {
            if is_mbox_input(&input_path) {
                let mbox = config.mbox.clone().unwrap_or_default();
                Some(ExtractMode::Mbox {
                    input_path: input_path.clone(),
                    account_config: mbox,
                })
            } else if input_path_is_set_but_no_mbox(&input_path) {
                // A `sync:`-less email source whose `input_path` exists but
                // holds no `.mbox` is a config error — same as the old
                // orchestrator path.
                return Err(anyhow!(
                    "email source {name} declares no download mode (`sync` for JMAP, \
                     `gmail_api` for the Gmail REST API) and no .mbox was found under {}",
                    input_path.display()
                ));
            } else {
                None
            }
        }
    };

    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(mode) = mode {
        procs.push(Box::new(EmailDownload {
            id: format!("email/{name}/download"),
            raw_path,
            mode,
            blob_size_limit_bytes,
            latchkey,
            only_extract_labels: config.only_extract_labels.clone(),
        }));
    }
    Ok(procs)
}

/// Which download path email takes for this source.
enum ExtractMode {
    /// Live JMAP server sync.
    Jmap(EmailSync),
    /// Gmail REST API sync.
    GmailApi(EmailGmailApi),
    /// File-backed `.mbox` ingest (e.g. a Google Takeout export).
    Mbox {
        input_path: PathBuf,
        account_config: MboxSync,
    },
}

/// Email's download processor. Owns its raw doltlite store end to end.
pub struct EmailDownload {
    id: String,
    raw_path: PathBuf,
    mode: ExtractMode,
    blob_size_limit_bytes: Option<u64>,
    /// Which latchkey identity to authenticate as, forwarded whole from
    /// the source's `latchkey_settings:` block. Both live modes use it
    /// (JMAP's `fastmail`, Gmail's `google-gmail`); the mbox mode makes
    /// no requests and ignores it.
    latchkey: LatchkeySettings,
    /// Full mailbox label paths to limit extraction to (empty = every
    /// mailbox). Applies to both JMAP and mbox modes.
    only_extract_labels: Vec<String>,
}

#[async_trait]
impl DataProcessor for EmailDownload {
    fn id(&self) -> &str {
        &self.id
    }

    /// Seals after each flushed batch — the email rows, their id mapping and
    /// their blob bytes land together, so a consumer never sees a message
    /// naming bytes it cannot resolve. Deletions come from
    /// `prune_to_enumeration`, which runs after the walk and only when the
    /// walk was authoritative over the whole mailbox, so between seals the
    /// store is the previous snapshot plus what this run has mirrored.
    ///
    /// JMAP (Fastmail) and the Gmail API both seal, at their respective
    /// batch boundaries. mbox does not — no seam is wired — so it commits
    /// once at the end, which costs latency and never correctness: a
    /// consumer that gets no checkpoints simply does one pass when the
    /// download finishes.
    fn streams_output(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        // The source owns the store: open it, hand the orchestrator only an
        // opaque interrupt-commit hook, do the work, commit, close. No pool or
        // `dolt_commit` ever crosses back to the orchestrator.
        let entity_db = download::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;

        let summary = match &self.mode {
            ExtractMode::Jmap(sync) => {
                let s = download::fetch(download::FetchOptions {
                    db,
                    sealer: Some(session.sealer()),
                    hostname: sync.hostname.clone(),
                    latchkey: self.latchkey.clone(),
                    account_id: sync.account_id.clone(),
                    full_resync: sync.full_resync,
                    only_mailbox_labels: self.only_extract_labels.clone(),
                    blob_size_limit_bytes: self.blob_size_limit_bytes,
                    blob_download_concurrency: sync.blob_download_concurrency,
                    progress: ctx.progress.clone(),
                    control: ctx.control.clone(),
                })
                .await?;
                format!(
                    "mailboxes={} emails={} destroyed={} threads={} blobs(dl={} oversize={} err={})",
                    s.mailboxes_upserted,
                    s.emails_upserted,
                    s.emails_destroyed,
                    s.threads_upserted,
                    s.blobs_downloaded,
                    s.blobs_oversize,
                    s.blobs_errored,
                )
            }
            ExtractMode::GmailApi(gmail) => {
                let s = download::gmail_api::fetch(download::gmail_api::FetchOptions {
                    db,
                    sealer: Some(session.sealer()),
                    config: gmail.clone(),
                    latchkey: self.latchkey.clone(),
                    only_labels: self.only_extract_labels.clone(),
                    blob_size_limit_bytes: self.blob_size_limit_bytes,
                    progress: ctx.progress.clone(),
                    control: ctx.control.clone(),
                })
                .await?;
                format!(
                    "mailboxes={} threads={} emails={} destroyed={} \
                     blobs(stored={} skipped={} oversize={}) filtered={} \
                     quota_units={} full_sync={} budget_exhausted={}",
                    s.mailboxes_upserted,
                    s.threads_upserted,
                    s.emails_upserted,
                    s.emails_destroyed,
                    s.blobs_stored,
                    s.blobs_skipped,
                    s.blobs_oversize,
                    s.messages_filtered,
                    s.quota_units_spent,
                    s.full_sync,
                    s.budget_exhausted,
                )
            }
            ExtractMode::Mbox {
                input_path,
                account_config,
            } => {
                let s = download::mbox::fetch(download::mbox::FetchOptions {
                    cache: FingerprintCache::open(&fingerprint_cache::default_cache_path()?)
                        .await?,
                    db,
                    input_path: input_path.clone(),
                    account_id_override: account_config.account_id.clone(),
                    account_config: download::mbox::MboxAccountConfig {
                        account_id: account_config.account_id.clone(),
                        display_name: account_config.display_name.clone(),
                        email_address: account_config.email_address.clone(),
                        is_personal: account_config.is_personal,
                    },
                    only_labels: self.only_extract_labels.clone(),
                    blob_size_limit_bytes: self.blob_size_limit_bytes,
                    progress: ctx.progress.clone(),
                    control: ctx.control.clone(),
                })
                .await?;
                format!(
                    "mailboxes={} threads={} emails={} blobs(stored={} skipped={} oversize={}) parse_errors={}",
                    s.mailboxes_upserted,
                    s.threads_upserted,
                    s.emails_upserted,
                    s.blobs_stored,
                    s.blobs_skipped,
                    s.blobs_oversize,
                    s.parse_errors,
                )
            }
        };

        // The source's post-download commit + pool close (uniform across
        // providers); keeps the old `{stats} commit={h}` summary suffix.
        Ok(session.finish(ctx, summary).await)
    }
}

fn is_mbox_input(input: &Path) -> bool {
    if input.is_file() {
        return input.extension().and_then(|s| s.to_str()) == Some("mbox");
    }
    let Ok(entries) = std::fs::read_dir(input) else {
        return false;
    };
    entries.flatten().any(|e| {
        let p = e.path();
        p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("mbox")
    })
}

fn input_path_is_set_but_no_mbox(input: &Path) -> bool {
    input.exists()
}
