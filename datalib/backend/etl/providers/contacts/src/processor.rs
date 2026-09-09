//! Program-A `DataProcessor`s for the carddav source. Carddav contributes
//! an **download** processor ([`CarddavDownload`] — live CardDAV server sync
//! or file-backed `.vcf` ingest, chosen by config) and a **render**
//! processor ([`CarddavRender`]). [`plan_download`] / [`plan_render`] build the
//! per-wave processors the orchestrator drives, owning every carddav-specific decision (which
//! download mode) so the orchestrator destructures nothing.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use datalib_etl::http::LatchkeySettings;
use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};

use datalib_etl_carddav_config::{CarddavConfig, CarddavSync};

use crate::download;

/// Download wave: always present. `sync:` present → live CardDAV
/// server; absent → file mode (`.vcf` tree under input_path, no
/// account override).
pub fn plan_download(
    ctx: PlanContext,
    config: CarddavConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let input_path = config.common.input_or_raw_path().to_path_buf();
    let latchkey = config.latchkey_settings.clone();
    let mode = match config.sync {
        Some(sync) => DownloadMode::Server(sync),
        None => DownloadMode::File {
            input_path,
            account_id_override: None,
        },
    };
    Ok(vec![Box::new(CarddavDownload {
        id: format!("carddav/{name}/download"),
        raw_path,
        mode,
        latchkey,
    })])
}

/// Which download path carddav takes for this source.
enum DownloadMode {
    /// Live CardDAV server sync.
    Server(CarddavSync),
    /// File-backed `.vcf` ingest (e.g. a Google/Fastmail export).
    File {
        input_path: PathBuf,
        account_id_override: Option<String>,
    },
}

/// Carddav's download processor. Owns its raw doltlite store end to end.
pub struct CarddavDownload {
    id: String,
    raw_path: PathBuf,
    mode: DownloadMode,
    /// Which latchkey identity to authenticate as, forwarded whole from
    /// the source's `latchkey_settings:` block. Unused by the file-backed
    /// `.vcf` mode, which makes no requests.
    latchkey: LatchkeySettings,
}

#[async_trait]
impl DataProcessor for CarddavDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        // The source owns the store: open it, hand the orchestrator only an
        // opaque interrupt-commit hook, do the work, commit, close.
        let entity_db = download::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;

        let summary = match &self.mode {
            DownloadMode::Server(sync) => {
                let s = download::fetch(download::FetchOptions {
                    db,
                    server_url: sync.server_url.clone(),
                    addressbooks: sync.addressbooks.clone(),
                    latchkey: self.latchkey.clone(),
                    progress: ctx.progress.clone(),
                    control: ctx.control.clone(),
                })
                .await?;
                format!(
                    "addressbooks={} new={} updated={} deleted={} errors={} requests={}",
                    s.addressbooks,
                    s.contacts_new,
                    s.contacts_updated,
                    s.contacts_deleted,
                    s.errors,
                    s.requests,
                )
            }
            DownloadMode::File {
                input_path,
                account_id_override,
            } => {
                let s = download::vcf_dir::fetch(download::vcf_dir::FetchOptions {
                    db,
                    input_path: input_path.clone(),
                    cache: FingerprintCache::open(&fingerprint_cache::default_cache_path()?)
                        .await?,
                    account_id_override: account_id_override.clone(),
                    progress: ctx.progress.clone(),
                    control: ctx.control.clone(),
                })
                .await?;
                format!(
                    "addressbooks={} new={} updated={} files_skipped={} errors={}",
                    s.addressbooks, s.contacts_new, s.contacts_updated, s.files_skipped, s.errors,
                )
            }
        };

        // The source's post-download commit + pool close (uniform across
        // providers); keeps the old `{stats} commit={h}` summary suffix.
        Ok(session.finish(ctx, summary).await)
    }
}
