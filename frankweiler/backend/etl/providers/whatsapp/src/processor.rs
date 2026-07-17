//! Program-A `DataProcessor`s for the `whatsapp_backup` source. WhatsApp
//! contributes an **extract** processor ([`WhatsappExtract`] — decrypts the
//! on-disk `msgstore.db.crypt15`, mirrors the curated `wa_*` tables into its
//! raw doltlite store) when `sync:` is present, plus an always-present
//! **translate** processor ([`WhatsappRender`]). [`plan_download`] /
//! [`plan_render`] build the per-wave processors the orchestrator drives.
//!
//! Storage ownership lives here, not in the orchestrator: [`WhatsappExtract`]
//! opens its own raw doltlite store (via `RawStoreSession`), registers an opaque [`Checkpoint`]
//! for interrupt-safety, and issues its own post-extract `dolt_commit`. The
//! orchestrator never sees a pool or a commit.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;

use frankweiler_etl::periodize::Period;
use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx};
use frankweiler_etl_whatsapp_config::{WhatsAppSync, WhatsappConfig};

use crate::extract;

/// Download wave. Extract requires `sync:` (which carries the required
/// `backup_dir`); error exactly as the old orchestrator did.
pub fn plan_download(
    ctx: PlanContext,
    config: WhatsappConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let sync = config
        .sync
        .ok_or_else(|| anyhow!("whatsapp_backup source {name} missing sync.backup_dir"))?;
    Ok(vec![Box::new(WhatsappExtract {
        id: format!("whatsapp/{name}/extract"),
        raw_path,
        sync,
    })])
}

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: WhatsappConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(WhatsappRender {
        id: format!("whatsapp/{name}/translate"),
        raw_path,
        name,
    })])
}

/// WhatsApp's extract processor. Owns its raw doltlite store end to end.
struct WhatsappExtract {
    id: String,
    raw_path: PathBuf,
    sync: WhatsAppSync,
}

#[async_trait]
impl DataProcessor for WhatsappExtract {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let db_path = frankweiler_etl::doltlite_raw::db_path_for(&self.raw_path);
        let db = extract::RawDb::open(&db_path).await?;
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
        let root_key = key_hex.and_then(|h| frankweiler_whatsapp_backup::decode_hex_key(&h))?;

        let s = extract::fetch(&self.sync.backup_dir, &root_key, &db).await?;
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

/// WhatsApp's translate processor — reads the raw store and emits rendered
/// markdown through the fused-Load callback.
struct WhatsappRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl DataProcessor for WhatsappRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render_and_index_md::{parse, render_all};
        // WhatsApp doesn't expose a `period` knob on its sync block today —
        // default to month bucketing, same as signal.
        let period = Period::from_config(None).context("default whatsapp period")?;
        let parsed = parse(&self.raw_path, period, &self.name)
            .with_context(|| format!("whatsapp parse {}", self.raw_path.display()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        render_all(
            &parsed.chats,
            &parsed.blobs_by_chat,
            &self.raw_path,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("whatsapp render_all")?;
        Ok("rendered".into())
    }
}
