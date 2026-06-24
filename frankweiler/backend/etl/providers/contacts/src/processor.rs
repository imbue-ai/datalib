//! Program-A `DataProcessor`s for the carddav source. Carddav contributes
//! an **extract** processor ([`CarddavExtract`] — live CardDAV server sync
//! or file-backed `.vcf` ingest, chosen by config) and a **translate**
//! processor ([`CarddavRender`]). [`plan`] builds the [`SourcePlan`] the
//! orchestrator drives, owning every carddav-specific decision (which
//! extract mode) so the orchestrator destructures nothing.
//!
//! Storage ownership lives here, not in the orchestrator: [`CarddavExtract`]
//! opens its own raw doltlite store, registers an opaque [`PoolCheckpoint`]
//! for interrupt-safety, and issues its own post-extract `dolt_commit`. The
//! orchestrator never sees a pool or a commit.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanCommon, RunCtx, SourcePlan};

use frankweiler_etl_carddav_config::{CarddavConfig, CarddavSync};

use crate::extract;

/// Build carddav's [`SourcePlan`]: always a translate processor, plus an
/// extract processor (server mode when `sync:` is present, else file mode
/// ingesting `.vcf` exports under `input_path`). The provider owns every
/// carddav-specific decision; the orchestrator passes only the
/// envelope-level [`PlanCommon`].
pub fn plan(common: PlanCommon, config: CarddavConfig) -> Result<SourcePlan> {
    let PlanCommon {
        name,
        raw_path,
        input_path,
        ..
    } = common;

    let mut plan = SourcePlan::new();

    // Translate is always present (renders whatever is in the raw store).
    plan.translate.push(Box::new(CarddavRender {
        id: format!("carddav/{name}/translate"),
        raw_path: raw_path.clone(),
        name: name.clone(),
    }));

    // Extract mode: `sync:` present → live CardDAV server; absent →
    // file mode (`.vcf` tree under input_path, no account override).
    let mode = match config.sync {
        Some(sync) => ExtractMode::Server(sync),
        None => ExtractMode::File {
            input_path,
            account_id_override: None,
        },
    };

    plan.extract.push(Box::new(CarddavExtract {
        id: format!("carddav/{name}/extract"),
        raw_path,
        mode,
    }));

    Ok(plan)
}

/// Which extract path carddav takes for this source.
enum ExtractMode {
    /// Live CardDAV server sync.
    Server(CarddavSync),
    /// File-backed `.vcf` ingest (e.g. a Google/Fastmail export).
    File {
        input_path: PathBuf,
        account_id_override: Option<String>,
    },
}

/// Carddav's extract processor. Owns its raw doltlite store end to end.
pub struct CarddavExtract {
    id: String,
    raw_path: PathBuf,
    mode: ExtractMode,
}

#[async_trait]
impl DataProcessor for CarddavExtract {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        // The source owns the store: open it, hand the orchestrator only an
        // opaque interrupt-commit hook, do the work, commit, close.
        let entity_db = extract::db_path_for(&self.raw_path);
        let db = extract::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;

        let summary = match &self.mode {
            ExtractMode::Server(sync) => {
                let s = extract::fetch(extract::FetchOptions {
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
            ExtractMode::File {
                input_path,
                account_id_override,
            } => {
                let s = extract::vcf_dir::fetch(extract::vcf_dir::FetchOptions {
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

        // The source's post-extract commit + pool close (uniform across
        // providers); keeps the old `{stats} commit={h}` summary suffix.
        Ok(session.finish(ctx, summary).await)
    }
}

/// Carddav's translate processor — reads the raw store and emits one
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
        use crate::render_and_index_md::{parse, render};

        let db_path = extract::db_path_for(&self.raw_path);
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
