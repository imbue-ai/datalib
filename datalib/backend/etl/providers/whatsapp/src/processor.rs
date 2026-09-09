//! Program-A `DataProcessor`s for the `whatsapp_backup` source. WhatsApp
//! contributes an **download** processor ([`WhatsappDownload`] — decrypts the
//! on-disk `msgstore.db.crypt15`, mirrors the curated `wa_*` tables into its
//! raw doltlite store) when `sync:` is present, plus an always-present
//! **render** processor ([`WhatsappRender`]). [`plan_download`] /
//! [`plan_render`] build the per-wave processors the orchestrator drives.

use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_whatsapp_config::{WhatsAppSync, WhatsappConfig};

use crate::download;

pub fn plan_download(
    ctx: PlanContext,
    config: WhatsappConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let sync = config
        .sync
        .ok_or_else(|| anyhow!("whatsapp_backup source {name} missing sync.backup_dir"))?;
    Ok(vec![Box::new(WhatsappDownload {
        id: format!("whatsapp/{name}/download"),
        raw_path,
        sync,
    })])
}

/// WhatsApp's download processor. Owns its raw doltlite store end to end.
struct WhatsappDownload {
    id: String,
    raw_path: PathBuf,
    sync: WhatsAppSync,
}

#[async_trait]
impl DataProcessor for WhatsappDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let db_path = datalib_etl::doltlite_raw::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&db_path).await?;
        // Open the session (snapshot + interrupt hook) BEFORE fetch borrows
        // `&db`: it captures the write pool the commit + report run against.
        let session = ctx.open_store(db.pool().clone(), db_path).await;

        // Read the hex root key from the configured env var (default
        // WHATSAPP_BACKUP_DECRYPTION_KEY), decode it, then decrypt + mirror.
        let env_var = self
            .sync
            .key_env_var
            .clone()
            .unwrap_or_else(|| "WHATSAPP_BACKUP_DECRYPTION_KEY".to_string());
        let key_hex = std::env::var(&env_var)
            .with_context(|| format!("read WhatsApp root key from env var `{env_var}`"));
        let root_key = key_hex.and_then(|h| datalib_whatsapp_backup::decode_hex_key(&h))?;

        let cache = FingerprintCache::open(&fingerprint_cache::default_cache_path()?).await?;
        let s = download::fetch(&self.sync.backup_dir, &root_key, &db, &cache).await?;
        let summary = format!(
            "jids={} chats={} messages={} message_text={} message_media={} \
             reactions={} media_files={}",
            s.jids,
            s.chats,
            s.messages,
            s.message_text,
            s.message_media,
            s.message_add_on_reaction,
            s.media_files,
        );
        Ok(session.finish(ctx, summary).await)
    }
}
