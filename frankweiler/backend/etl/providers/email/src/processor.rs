//! Program A `DataProcessor`s for the email source.
//!
//! Email contributes an **extract** processor ([`EmailExtract`] — JMAP live
//! sync or file-backed mbox, chosen by config) and a **translate** processor
//! ([`EmailRender`]). [`plan`] builds the [`SourcePlan`] the orchestrator
//! drives, owning every email-specific decision (which extract mode, whether
//! an mbox is present, the outlink flavor) so the orchestrator destructures
//! nothing.
//!
//! Storage ownership lives here, not in the orchestrator: [`EmailExtract`]
//! opens its own raw doltlite store, registers an opaque [`PoolCheckpoint`]
//! for interrupt-safety, and issues its own post-extract `dolt_commit`. The
//! orchestrator never sees a pool or a commit. (The per-source *report* is
//! still assembled orchestrator-side for now — tracked in issue #37.)

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanCommon, RunCtx, SourcePlan};
use frankweiler_etl::raw_store::PoolCheckpoint;

use frankweiler_etl_email_config::{EmailConfig, EmailOutlink, EmailSync, MboxSync};

use crate::extract;
use crate::render_and_index_md::render::OutlinkFormat;

/// Build email's [`SourcePlan`]: always a translate processor, plus an extract
/// processor when the source is managed (a `sync:` block, or an `.mbox` at
/// `input_path`). The provider owns every email-specific decision; the
/// orchestrator passes only the envelope-level [`PlanCommon`].
pub fn plan(common: PlanCommon, config: EmailConfig) -> Result<SourcePlan> {
    let PlanCommon {
        name,
        raw_path,
        input_path,
        blob_size_limit_bytes,
        ..
    } = common;

    let outlink = config.outlink_format.map(outlink_format);

    let mut plan = SourcePlan::new();

    // Translate is always present (renders whatever is in the raw store).
    plan.translate.push(Box::new(EmailRender {
        id: format!("email/{name}/translate"),
        raw_path: raw_path.clone(),
        name: name.clone(),
        outlink,
    }));

    // Extract present iff managed: `sync:` → JMAP; else an `.mbox` → mbox mode.
    let mode = match &config.sync {
        Some(sync) => Some(ExtractMode::Jmap(sync.clone())),
        None => {
            if is_mbox_input(&input_path) {
                let mbox = config.mbox.clone().unwrap_or_default();
                Some(ExtractMode::Mbox {
                    input_path: input_path.clone(),
                    account_config: mbox,
                })
            } else if input_path_is_set_but_no_mbox(&input_path) {
                // A `sync:`-less email source whose `input_path` exists but
                // holds no `.mbox` is a config error — same as the old
                // orchestrator path.
                return Err(anyhow!(
                    "email source {name} has no sync: block and no .mbox found under {}",
                    input_path.display()
                ));
            } else {
                None
            }
        }
    };

    if let Some(mode) = mode {
        plan.extract.push(Box::new(EmailExtract {
            id: format!("email/{name}/extract"),
            raw_path,
            mode,
            blob_size_limit_bytes,
        }));
    }

    Ok(plan)
}

fn outlink_format(f: EmailOutlink) -> OutlinkFormat {
    match f {
        EmailOutlink::Gmail => OutlinkFormat::Gmail,
        EmailOutlink::Fastmail => OutlinkFormat::Fastmail,
    }
}

/// Which extract path email takes for this source.
enum ExtractMode {
    /// Live JMAP server sync.
    Jmap(EmailSync),
    /// File-backed `.mbox` ingest (e.g. a Google Takeout export).
    Mbox {
        input_path: PathBuf,
        account_config: MboxSync,
    },
}

/// Email's extract processor. Owns its raw doltlite store end to end.
pub struct EmailExtract {
    id: String,
    raw_path: PathBuf,
    mode: ExtractMode,
    blob_size_limit_bytes: Option<u64>,
}

#[async_trait]
impl DataProcessor for EmailExtract {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        // The source owns the store: open it, hand the orchestrator only an
        // opaque interrupt-commit hook, do the work, commit, close. No pool or
        // `dolt_commit` ever crosses back to the orchestrator.
        let entity_db = extract::db_path_for(&self.raw_path);
        let db = extract::RawDb::open(&entity_db).await?;
        let pool = db.pool().clone();
        ctx.register_checkpoint(
            &self.id,
            PoolCheckpoint::new(
                pool.clone(),
                format!("extract {}: interrupted (Ctrl-C)", ctx.name),
            ),
        );

        let summary = match &self.mode {
            ExtractMode::Jmap(sync) => {
                let s = extract::fetch(extract::FetchOptions {
                    db_path: self.raw_path.clone(),
                    db: Some(db),
                    hostname: sync.hostname.clone(),
                    account_id: sync.account_id.clone(),
                    full_resync: sync.full_resync,
                    only_mailbox_ids: sync.only_mailbox_ids.clone(),
                    blob_size_limit_bytes: self.blob_size_limit_bytes,
                    progress: ctx.progress.clone(),
                    control: ctx.control.clone(),
                })
                .await?;
                format!(
                    "mailboxes={} emails={} destroyed={} threads={} blobs(dl={} oversize={} err={})",
                    s.mailboxes_upserted,
                    s.emails_upserted,
                    s.emails_destroyed,
                    s.threads_upserted,
                    s.blobs_downloaded,
                    s.blobs_oversize,
                    s.blobs_errored,
                )
            }
            ExtractMode::Mbox {
                input_path,
                account_config,
            } => {
                let s = extract::mbox::fetch(extract::mbox::FetchOptions {
                    db_path: self.raw_path.clone(),
                    db: Some(db),
                    input_path: input_path.clone(),
                    account_id_override: account_config.account_id.clone(),
                    account_config: extract::mbox::MboxAccountConfig {
                        account_id: account_config.account_id.clone(),
                        display_name: account_config.display_name.clone(),
                        email_address: account_config.email_address.clone(),
                        is_personal: account_config.is_personal,
                    },
                    blob_size_limit_bytes: self.blob_size_limit_bytes,
                    progress: ctx.progress.clone(),
                    control: ctx.control.clone(),
                })
                .await?;
                format!(
                    "mailboxes={} threads={} emails={} blobs(stored={} skipped={} oversize={}) parse_errors={}",
                    s.mailboxes_upserted,
                    s.threads_upserted,
                    s.emails_upserted,
                    s.blobs_stored,
                    s.blobs_skipped,
                    s.blobs_oversize,
                    s.parse_errors,
                )
            }
        };

        // The source's post-extract commit + pool close (uniform across
        // providers); keeps the old `{stats} commit={h}` summary suffix.
        Ok(frankweiler_etl::raw_store::commit_and_close(pool, ctx.name, summary).await)
    }
}

/// Email's translate processor — reads the raw store and emits one rendered
/// markdown per thread through the fused-Load callback.
pub struct EmailRender {
    id: String,
    raw_path: PathBuf,
    name: String,
    outlink: Option<OutlinkFormat>,
}

#[async_trait]
impl DataProcessor for EmailRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render_and_index_md::parse::parse;
        use crate::render_and_index_md::render::render_all;

        let db = extract::db_path_for(&self.raw_path);
        if !db.exists() {
            tracing::info!(
                source = %self.name,
                db = %db.display(),
                "email render: no raw db — skipping",
            );
            return Ok("skipped (no raw db)".into());
        }

        // Two-phase parse driven by the render cursor's commit, identical to
        // the old registry path; `prior_fingerprints` is intentionally unused
        // for email (the cursor is the single source of truth).
        let cursor_path =
            frankweiler_etl::render_cursor::cursor_path(ctx.root, "email", &self.name);
        let cursor = frankweiler_etl::render_cursor::read(&cursor_path)?;
        let parsed = parse(&db, cursor.as_ref().map(|c| c.last_rendered_hash.as_str()))?;

        let mut on_doc = |md| ctx.emit_doc(md);
        render_all(
            &parsed,
            ctx.root,
            &self.name,
            self.outlink,
            ctx.progress,
            &mut on_doc,
        )?;
        Ok("rendered".into())
    }
}

/// True when `input` looks like an mbox drop: a single `.mbox` file or a
/// directory containing at least one. (Provider-owned copy of the
/// orchestrator's old `is_mbox_input`.)
fn is_mbox_input(input: &Path) -> bool {
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

/// Whether `input_path` points at something on disk (so "no mbox here" is a
/// real error) vs. the default raw-store fallback (which isn't an export).
fn input_path_is_set_but_no_mbox(input: &Path) -> bool {
    input.exists()
}
