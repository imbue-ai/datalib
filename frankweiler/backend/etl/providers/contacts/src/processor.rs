//! Program-A `DataProcessor`s for the carddav source. Carddav contributes
//! an **download** processor ([`CarddavDownload`] — live CardDAV server sync
//! or file-backed `.vcf` ingest, chosen by config) and a **render**
//! processor ([`CarddavRender`]). [`plan_download`] / [`plan_render`] build the
//! per-wave processors the orchestrator drives, owning every carddav-specific decision (which
//! download mode) so the orchestrator destructures nothing.
//!
//! Storage ownership lives here, not in the orchestrator: [`CarddavDownload`]
//! opens its own raw doltlite store, registers an opaque [`PoolCheckpoint`]
//! for interrupt-safety, and issues its own post-download `dolt_commit`. The
//! orchestrator never sees a pool or a commit.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx};

use frankweiler_etl_carddav_config::CarddavRenderConfig;
use frankweiler_etl_carddav_config::{CarddavConfig, CarddavSync};

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
    })])
}

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: CarddavRenderConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(CarddavRender {
        id: format!("carddav/{name}/render"),
        raw_path,
        name,
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
                    db_path: self.raw_path.clone(),
                    db: Some(db),
                    server_url: sync.server_url.clone(),
                    addressbooks: sync.addressbooks.clone(),
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
                    db_path: self.raw_path.clone(),
                    db: Some(db),
                    input_path: input_path.clone(),
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

/// Carddav's render processor — reads the raw store and emits one
/// rendered markdown per contact through the fused-Load callback.
pub struct CarddavRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl DataProcessor for CarddavRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render::{parse, render};

        let db_path = download::db_path_for(&self.raw_path);
        let parsed = parse::parse(&db_path)
            .with_context(|| format!("carddav parse {}", db_path.display()))?;

        let mut on_doc = |md| ctx.emit_doc(md);
        render::render_all(
            &parsed,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("carddav render_all")?;
        Ok("rendered".into())
    }
}
