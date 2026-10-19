//! Program-A `DataProcessor`s for the `signal` source. A signal source
//! with a `backup` table contributes download + render; the
//! render processor is always present (renders whatever is in the raw
//! store). The source owns its raw store (open/commit/checkpoint); the
//! orchestrator only drives `run`.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_files::fingerprint_cache::{self, FingerprintCache};
use datalib_etl_signal_config::{SignalConfig, SignalSync};

use crate::ingest;

pub async fn migrate(raw_dir: &std::path::Path) -> anyhow::Result<()> {
    let db = ingest::RawDb::open(&datalib_etl::raw_layout::entities_db(raw_dir)).await?;
    db.close().await;
    Ok(())
}

pub fn plan_ingest(ctx: PlanContext, config: SignalConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let sync = config
        .backup
        .ok_or_else(|| anyhow!("signal source {name} missing `backup.path`"))?;
    if sync.period.is_some() {
        anyhow::bail!(
            "signal `backup.period` is a render knob — put `period` in the \
             render step's params instead"
        );
    }
    Ok(vec![Box::new(SignalIngest {
        id: format!("signal/{name}/download"),
        raw_path,
        sync,
    })])
}

/// Signal's download processor. Owns its raw doltlite store end to end: opens
/// it, registers an opaque interrupt-commit hook, reads every snapshot under
/// `backup.path` the store lacks, one commit each, closes.
struct SignalIngest {
    id: String,
    raw_path: PathBuf,
    sync: SignalSync,
}

#[async_trait]
impl DataProcessor for SignalIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        // Before the store: opening it seals a commit, and a store that
        // exists reads as published to the render and the migrate pass.
        let env_var = self
            .sync
            .aep_env_var
            .as_deref()
            .unwrap_or(ingest::DEFAULT_AEP_ENV);
        let aep = std::env::var(env_var).map_err(|_| {
            anyhow!(
                "${env_var} not set — pass the Signal AEP via that env var (sourced from .envrc.private etc.)"
            )
        })?;

        let entity_db = ingest::db_path_for(&self.raw_path);
        let db = ingest::RawDb::open(&entity_db).await?;
        let (pool, cas_pool) = (db.pool().clone(), db.cas().pool().clone());
        ctx.run_store(pool, Some(cas_pool), |_| async {
            let s = ingest::fetch(ingest::FetchOptions {
                db,
                cache: FingerprintCache::open(&fingerprint_cache::default_cache_path()?).await?,
                snapshot_root: self.sync.path(),
                // Default: `<snapshot_root>/files/XX/<name>` — the layout Signal
                // Android produces. Override via a future SignalSync knob if it
                // matters.
                files_root: None,
                aep,
                progress: ctx.progress.clone(),
                control: ctx.control.clone(),
            })
            .await?;
            Ok(format!(
                "backups_found={} backups_read={} recipients={} chats={} chat_items={} \
                 media_files={} snapshot={}",
                s.backups_found,
                s.backups_read.len(),
                s.recipients,
                s.chats,
                s.chat_items,
                s.media_files,
                s.snapshot,
            ))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl::control::DownloadControl;
    use datalib_etl::download_metrics::DownloadMetrics;
    use datalib_etl::progress::Progress;
    use datalib_obs::diagnostics::Diagnostics;

    /// A download with no passphrase fails and leaves no store: a store,
    /// even an empty one, reads as published, so the launch's migrate
    /// pass would report the step succeeded (#1251).
    #[tokio::test]
    async fn no_passphrase_leaves_no_store() {
        let td = tempfile::tempdir().unwrap();
        let step = SignalIngest {
            id: "signal/test/download".into(),
            raw_path: td.path().join("ingest"),
            sync: SignalSync {
                path: td.path().join("backup"),
                aep_env_var: Some("DATALIB_TEST_SIGNAL_AEP_NEVER_SET".into()),
                ..Default::default()
            },
        };
        let (progress, control) = (Progress::noop(), DownloadControl::default());
        let ctx = RunCtx::new(
            "test",
            td.path(),
            "2369-04-14T00:00:00+00:00",
            &progress,
            &control,
            DownloadMetrics::new(),
            Diagnostics::new(),
        );

        let err = step.run(&ctx).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("DATALIB_TEST_SIGNAL_AEP_NEVER_SET"),
            "{err:#}"
        );
        let db_path = ingest::db_path_for(&step.raw_path);
        assert!(!db_path.exists(), "{} was created", db_path.display());
    }
}
