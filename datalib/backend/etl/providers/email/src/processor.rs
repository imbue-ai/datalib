//! Program A `DataProcessor`s for the email source.

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_web::http::LatchkeySettings;

use datalib_etl_email_config::{EmailConfig, EmailGmailApi, EmailLiveMode, EmailSync, MboxSync};
use std::path::PathBuf;

use crate::ingest;

/// Ingest wave: a live table (`jmap`, `gmail`) selects a server
/// mode; `mbox` reads the `.mbox` at its `path`.
pub fn plan_ingest(ctx: PlanContext, config: EmailConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    if config.outlink_format.is_some() || !config.only_render_labels.is_empty() {
        anyhow::bail!(
            "email `outlink_format` / `only_render_labels` are render knobs — \
             put them in the render step's params instead"
        );
    }
    let raw_path = config.common.raw_path().to_path_buf();
    let blob_size_limit_bytes = config.common.blob_size_limit_bytes;
    let latchkey = config.latchkey_settings.clone();

    // Live-server modes are mutually exclusive (`live_mode` enforces
    // that, and `validate` refuses one beside `mbox`).
    let mode = match config.live_mode()? {
        Some(EmailLiveMode::Jmap(sync)) => Some(ExtractMode::Jmap(sync.clone())),
        Some(EmailLiveMode::GmailApi(gmail)) => Some(ExtractMode::GmailApi(gmail.clone())),
        // Planning never touches the filesystem — `datalib-dag --check`
        // and the schema tests run where the data is not — so whether the
        // path really is an mbox is checked when the download runs.
        None => config.mbox.clone().map(|mbox| ExtractMode::Mbox {
            input_path: mbox.path(),
            account_config: mbox,
        }),
    };

    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(mode) = mode {
        procs.push(Box::new(EmailIngest {
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
pub struct EmailIngest {
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
impl DataProcessor for EmailIngest {
    fn id(&self) -> &str {
        &self.id
    }

    /// JMAP (Fastmail) and the Gmail API seal after each batch of
    /// fetched messages and each batch of `.eml` bodies. mbox does not —
    /// no seam is wired — so it commits once at the end, which costs
    /// latency and never correctness: a consumer that gets no checkpoints
    /// simply does one pass when the download finishes.
    fn streams_output(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let db = ingest::RawDb::open(&ingest::db_path_for(&self.raw_path)).await?;
        let (pool, cas_pool) = (db.pool().clone(), db.cas().pool().clone());
        ctx.run_store(pool, Some(cas_pool), |sealer| async {
            Ok(match &self.mode {
                ExtractMode::Jmap(sync) => {
                    let s = ingest::fetch(ingest::FetchOptions {
                        db,
                        sealer: Some(sealer),
                        hostname: sync.hostname.clone(),
                        latchkey: self.latchkey.clone(),
                        account_id: sync.account_id.clone(),
                        full_resync: sync.full_resync,
                        only_mailbox_labels: self.only_extract_labels.clone(),
                        blob_size_limit_bytes: self.blob_size_limit_bytes,
                        blob_download_concurrency: sync.blob_download_concurrency,
                        blob_flush_count: None,
                        blob_flush_bytes: None,
                        progress: ctx.progress.clone(),
                        control: ctx.control.clone(),
                    })
                    .await?;
                    format!(
                        "mailboxes={} emails={} destroyed={} blobs(dl={} oversize={} err={})",
                        s.mailboxes_upserted,
                        s.emails_upserted,
                        s.emails_destroyed,
                        s.blobs_downloaded,
                        s.blobs_oversize,
                        s.blobs_errored,
                    )
                }
                ExtractMode::GmailApi(gmail) => {
                    let s = ingest::gmail_api::fetch(ingest::gmail_api::FetchOptions {
                        db,
                        sealer: Some(sealer),
                        config: gmail.clone(),
                        latchkey: self.latchkey.clone(),
                        only_labels: self.only_extract_labels.clone(),
                        blob_size_limit_bytes: self.blob_size_limit_bytes,
                        flush_batch: None,
                        progress: ctx.progress.clone(),
                        control: ctx.control.clone(),
                    })
                    .await?;
                    format!(
                        "mailboxes={} emails={} destroyed={} \
                         blobs(stored={} skipped={} oversize={}) filtered={} \
                         quota_units={} walked=[{}] budget_exhausted={}",
                        s.mailboxes_upserted,
                        s.emails_upserted,
                        s.emails_destroyed,
                        s.blobs_stored,
                        s.blobs_skipped,
                        s.blobs_oversize,
                        s.messages_filtered,
                        s.quota_units_spent,
                        s.walked.join(", "),
                        s.budget_exhausted,
                    )
                }
                ExtractMode::Mbox {
                    input_path,
                    account_config,
                } => {
                    if !is_mbox_input(input_path) {
                        return Err(anyhow!(
                            "`mbox.path` is {} — expected a .mbox file, or a directory holding one",
                            input_path.display()
                        ));
                    }
                    let s = ingest::mbox::fetch(ingest::mbox::FetchOptions {
                        db,
                        input_path: input_path.clone(),
                        account_config: ingest::mbox::MboxAccountConfig {
                            account_id: account_config.account_id.clone(),
                            display_name: account_config.display_name.clone(),
                            email_address: account_config.email_address.clone(),
                            is_personal: account_config.is_personal,
                        },
                        only_labels: self.only_extract_labels.clone(),
                        progress: ctx.progress.clone(),
                        control: ctx.control.clone(),
                    })
                    .await?;
                    format!(
                        "mailboxes={} threads={} emails={} removed={} blobs(stored={} skipped={}) parse_errors={}",
                        s.mailboxes_upserted,
                        s.threads_upserted,
                        s.emails_upserted,
                        s.emails_removed,
                        s.blobs_stored,
                        s.blobs_skipped,
                        s.parse_errors,
                    )
                }
            })
        })
        .await
    }
}

fn is_mbox_input(input: &PathBuf) -> bool {
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
