//! Program-A `DataProcessor`s for the `whatsapp_backup` source. WhatsApp
//! contributes an **extract** processor ([`WhatsappExtract`] — decrypts the
//! on-disk `msgstore.db.crypt15`, mirrors the curated `wa_*` tables into its
//! raw doltlite store) when `sync:` is present, plus an always-present
//! **translate** processor ([`WhatsappRender`]). [`plan`] builds the
//! [`SourcePlan`] the orchestrator drives.
//!
//! Storage ownership lives here, not in the orchestrator: [`WhatsappExtract`]
//! opens its own raw doltlite store, registers an opaque [`PoolCheckpoint`]
//! for interrupt-safety, and issues its own post-extract `dolt_commit`. The
//! orchestrator never sees a pool or a commit.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;

use frankweiler_etl::periodize::Period;
use frankweiler_etl::processor::{DataProcessor, PlanCommon, RunCtx, SourcePlan};
use frankweiler_etl::raw_store::PoolCheckpoint;
use frankweiler_etl_whatsapp_config::{WhatsAppSync, WhatsappConfig};

use crate::extract;

/// Build whatsapp's [`SourcePlan`]: always a translate processor (which bakes
/// in the raw path + name), plus an extract processor when `sync:` is present
/// (managed). `backup_dir` is required inside `sync:`, so a `sync:` block is
/// the only thing that distinguishes a managed extract source — there is no
/// translate-only `whatsapp_backup`; an absent `sync:` is an error.
pub fn plan(common: PlanCommon, config: WhatsappConfig) -> Result<SourcePlan> {
    let PlanCommon { name, raw_path, .. } = common;

    let mut plan = SourcePlan::new();
    plan.translate.push(Box::new(WhatsappRender {
        id: format!("whatsapp/{name}/translate"),
        raw_path: raw_path.clone(),
        name: name.clone(),
    }));

    // Extract requires `sync:` (which carries the required `backup_dir`). The
    // orchestrator's old `for_source` errored the same way when `sync` was
    // absent for a managed whatsapp source.
    let sync = config
        .sync
        .ok_or_else(|| anyhow!("whatsapp_backup source {name} missing sync.backup_dir"))?;
    plan.extract.push(Box::new(WhatsappExtract {
        id: format!("whatsapp/{name}/extract"),
        raw_path,
        sync,
    }));

    Ok(plan)
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
        // Clone the write pool BEFORE fetch borrows `&db`: the pool feeds both
        // the interrupt checkpoint and the final commit_and_close.
        let pool = db.pool().clone();
        ctx.register_checkpoint(
            &self.id,
            PoolCheckpoint::new(
                pool.clone(),
                format!("extract {}: interrupted (Ctrl-C)", ctx.name),
            ),
        );

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
        Ok(frankweiler_etl::raw_store::commit_and_close(pool, ctx.name, summary).await)
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
