//! Program-A `DataProcessor` for the `whatsapp` source: decrypt the
//! on-disk `msgstore.db.crypt15`, mirror it through the shared SQLite
//! mirror engine, register `Media/` in the CAS. Present when `backup` is
//! configured; the render processor lives in `datalib_etl_whatsapp_render`.

use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_whatsapp_config::{WhatsAppSync, WhatsappConfig};

use crate::ingest::{self, MirrorKnobs};

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
/// hook, fetch, commit + close via `session.finish`.
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
        let db_path = datalib_etl::doltlite_raw::db_path_for(&self.raw_path);
        let db = ingest::RawDb::open(&db_path).await?;
        // Open the session (snapshot + interrupt hook) BEFORE fetch borrows
        // `&db`: it captures the write pool the commit + report run against.
        let session = ctx.open_store(db.pool().clone(), db_path).await;

        let env_var = self
            .sync
            .key_env_var
            .clone()
            .unwrap_or_else(|| "WHATSAPP_BACKUP_DECRYPTION_KEY".to_string());
        let key_hex = std::env::var(&env_var)
            .with_context(|| format!("read WhatsApp root key from env var `{env_var}`"));
        let root_key = key_hex.and_then(|h| datalib_whatsapp_backup::decode_hex_key(&h))?;

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
        Ok(session.finish(ctx, summary.summary()).await)
    }
}
