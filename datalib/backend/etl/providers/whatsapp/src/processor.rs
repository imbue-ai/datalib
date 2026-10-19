//! Program-A `DataProcessor` for the `whatsapp` source: decrypt the
//! on-disk `msgstore.db.crypt15`, mirror it through the shared SQLite
//! mirror engine, register `Media/` in the CAS. Present when `backup` is
//! configured; the render processor lives in `datalib_etl_whatsapp_render`.

use datalib_etl_files::fingerprint_cache::{self, FingerprintCache};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_whatsapp_config::{WhatsAppSync, WhatsappConfig};

use crate::ingest::{self, MirrorKnobs};

pub async fn migrate(raw_dir: &std::path::Path) -> anyhow::Result<()> {
    let db = ingest::RawDb::open(&datalib_etl::raw_layout::entities_db(raw_dir)).await?;
    db.close().await;
    Ok(())
}

pub fn plan_ingest(
    ctx: PlanContext,
    config: WhatsappConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let sync = config
        .backup
        .clone()
        .ok_or_else(|| anyhow!("whatsapp source {name} missing `backup.path`"))?;
    let knobs = MirrorKnobs {
        include_tables: config.include_tables.clone(),
        exclude_tables: config.effective_excluded_tables(),
        exclude_columns: config.exclude_columns.clone(),
        gc: config.gc,
    };
    Ok(vec![Box::new(WhatsappIngest {
        id: format!("whatsapp/{name}/download"),
        raw_path,
        sync,
        knobs,
    })])
}

/// Owns its raw doltlite store end to end: open, register the interrupt
/// hook, fetch, commit + close via `run_store`.
struct WhatsappIngest {
    id: String,
    raw_path: PathBuf,
    sync: WhatsAppSync,
    knobs: MirrorKnobs,
}

#[async_trait]
impl DataProcessor for WhatsappIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        // Before the store: opening it seals a commit, and a store that
        // exists reads as published to the render and the migrate pass.
        let env_var = self
            .sync
            .key_env_var
            .as_deref()
            .unwrap_or("WHATSAPP_BACKUP_DECRYPTION_KEY");
        let key_hex = std::env::var(env_var)
            .with_context(|| format!("read WhatsApp root key from env var `{env_var}`"))?;
        let root_key = datalib_whatsapp_backup::decode_hex_key(&key_hex)?;

        let db_path = datalib_etl::doltlite_raw::db_path_for(&self.raw_path);
        let db = ingest::RawDb::open(&db_path).await?;
        let (pool, cas_pool) = (db.pool().clone(), db.cas().pool().clone());
        ctx.run_store(pool, Some(cas_pool), |_| async {
            let cache = FingerprintCache::open(&fingerprint_cache::default_cache_path()?).await?;
            let summary = ingest::fetch(
                &self.sync.path(),
                &root_key,
                &db,
                &cache,
                &self.knobs,
                ctx.progress,
            )
            .await?;
            Ok(summary.summary())
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

    /// A download with no key fails and leaves no store: a store, even an
    /// empty one, reads as published, so the render ran on it and the
    /// launch's migrate pass reported the step succeeded (#1251).
    #[tokio::test]
    async fn no_key_leaves_no_store() {
        let td = tempfile::tempdir().unwrap();
        let ingest = WhatsappIngest {
            id: "whatsapp/test/download".into(),
            raw_path: td.path().join("ingest"),
            sync: WhatsAppSync {
                path: td.path().join("backup"),
                key_env_var: Some("DATALIB_TEST_WHATSAPP_KEY_NEVER_SET".into()),
            },
            knobs: MirrorKnobs::everything(),
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

        let err = ingest.run(&ctx).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("DATALIB_TEST_WHATSAPP_KEY_NEVER_SET"),
            "{err:#}"
        );
        let db_path = datalib_etl::doltlite_raw::db_path_for(&ingest.raw_path);
        assert!(!db_path.exists(), "{} was created", db_path.display());
    }
}
