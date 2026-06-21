//! Render-and-index-md: the per-source step that projects a provider's
//! extracted data into the universal presentable shape — `grid_rows`
//! sidecars + rendered markdown documents.
//!
//! Every provider implements one uniform interface, [`RenderAndIndexMd`],
//! and [`renderer_for`] maps a [`SourceConfig`] variant to its
//! implementation. The orchestrator (`render_and_index_md_source` in
//! `main.rs`) builds a [`RenderCtx`] once and calls `.run(..)` — there is
//! no per-provider branching left in the orchestrator itself.
//!
//! The adapters are thin: each owns only its provider-specific knobs
//! (period bucketing, e-mail outlink flavor, Perseus alignment pairs) and
//! delegates to the provider crate's `render_and_index_md` entry points.
//! The knobs are pulled out of `SourceConfig` once, in [`renderer_for`],
//! so the run path never re-inspects the config.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use frankweiler_core::config::{EmailOutlink, SourceConfig};
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::periodize::Period;
use frankweiler_etl::progress::Progress;
use frankweiler_obs::status_line;

/// Everything a render-and-index-md step needs that is common across
/// providers. Provider-specific knobs are captured by the adapter at
/// construction time (see [`renderer_for`]); they never appear here.
pub struct RenderCtx<'a> {
    /// Workspace root — the parent of the `rendered_md/` tree the step
    /// writes markdown + `.grid_rows.json` sidecars into.
    pub root: &'a Path,
    /// Data root, for providers that resolve their raw store at the
    /// canonical `<data_root>/raw/<name>` location rather than from
    /// `input_path`.
    pub data_root: &'a Path,
    /// Source name (`sources[].name` in config.yaml).
    pub name: &'a str,
    /// `src.resolved_input_path(data_root)` — the user-facing input (a
    /// raw store dir, an export dir, or a single `.vcf` / `.mbox`).
    pub input_path: &'a Path,
    /// Progress hook for the per-source bar.
    pub progress: &'a Progress,
    /// Per-markdown source fingerprints from the prior run, for providers
    /// whose incremental skip is fingerprint-driven (rather than
    /// render-cursor-driven).
    pub prior_fingerprints: &'a HashMap<String, String>,
}

/// One callback per rendered markdown — hands the document (its path +
/// row set) to the orchestrator's inline Load step.
pub type OnDoc<'a> = dyn FnMut(RenderedMarkdown) -> Result<()> + 'a;

/// A provider's render-and-index-md step. One implementation per data
/// source; [`renderer_for`] selects it.
pub trait RenderAndIndexMd {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()>;
}

/// Map a configured source to its render-and-index-md implementation,
/// extracting any provider-specific knobs from the config up front.
pub fn renderer_for(src: &SourceConfig) -> Result<Box<dyn RenderAndIndexMd>> {
    Ok(match src {
        SourceConfig::ClaudeApi { .. } | SourceConfig::ClaudeExport { .. } => Box::new(Anthropic),
        SourceConfig::ChatgptApi { .. } => Box::new(Chatgpt),
        SourceConfig::SlackApi { .. } => Box::new(Slack),
        SourceConfig::GithubApi { .. } => Box::new(Github),
        SourceConfig::GitlabApi { .. } => Box::new(Gitlab),
        SourceConfig::NotionApi { .. } => Box::new(Notion),
        SourceConfig::Beeper { sync, .. } => {
            let period = Period::from_config(sync.as_ref().and_then(|s| s.period.as_deref()))
                .context("parse beeper period")?;
            Box::new(Beeper { period })
        }
        SourceConfig::Carddav { sync, .. } => Box::new(Carddav {
            from_sync: sync.is_some(),
        }),
        SourceConfig::Linkedin { .. } => Box::new(Linkedin),
        SourceConfig::GoogleTakeout { .. } => Box::new(GoogleTakeout),
        SourceConfig::SmsBackupRestore { .. } => Box::new(SmsBackupRestore),
        SourceConfig::Email {
            sync,
            outlink_format,
            ..
        } => {
            use frankweiler_etl_email::render_and_index_md::render::OutlinkFormat;
            let outlink = outlink_format.map(|f| match f {
                EmailOutlink::Gmail => OutlinkFormat::Gmail,
                EmailOutlink::Fastmail => OutlinkFormat::Fastmail,
            });
            Box::new(Email {
                from_sync: sync.is_some(),
                outlink,
            })
        }
        SourceConfig::Perseus { sync, .. } => {
            let pairs: Vec<(String, String)> = sync
                .as_ref()
                .map(|s| {
                    s.alignment_pairs
                        .iter()
                        .map(|[a, b]| (a.clone(), b.clone()))
                        .collect()
                })
                .unwrap_or_default();
            Box::new(Perseus { pairs })
        }
        SourceConfig::SignalBackup { sync, .. } => {
            let period = Period::from_config(sync.as_ref().and_then(|s| s.period.as_deref()))
                .context("parse signal period")?;
            Box::new(Signal { period })
        }
        SourceConfig::WhatsAppBackup { .. } => Box::new(WhatsApp),
        SourceConfig::Yolink { .. } => Box::new(Skip {
            provider: "yolink",
            reason: "extract-only, no render path",
        }),
    })
}

// ─────────────────────────────────────────────────────────────────────
// Chat-style, render-cursor-driven providers
// ─────────────────────────────────────────────────────────────────────

struct Anthropic;
impl RenderAndIndexMd for Anthropic {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_anthropic::render_and_index_md::{parse::parse, render::render_all};
        let cursor_path =
            frankweiler_etl::render_cursor::cursor_path(ctx.root, "anthropic", ctx.name);
        let cursor = frankweiler_etl::render_cursor::read(&cursor_path)
            .with_context(|| format!("read anthropic render cursor {}", cursor_path.display()))?;
        let parsed = parse(
            ctx.input_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("anthropic parse {}", ctx.input_path.display()))?;
        render_all(&parsed, ctx.root, ctx.name, ctx.progress, on_doc)
            .context("anthropic render_all")
            .map(|_| ())
    }
}

struct Chatgpt;
impl RenderAndIndexMd for Chatgpt {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_chatgpt::render_and_index_md::{parse::parse, render::render_all};
        let cursor_path =
            frankweiler_etl::render_cursor::cursor_path(ctx.root, "chatgpt", ctx.name);
        let cursor = frankweiler_etl::render_cursor::read(&cursor_path)
            .with_context(|| format!("read chatgpt render cursor {}", cursor_path.display()))?;
        let parsed = parse(
            ctx.input_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("chatgpt parse {}", ctx.input_path.display()))?;
        render_all(&parsed, ctx.root, ctx.name, ctx.progress, on_doc)
            .context("chatgpt render_all")
            .map(|_| ())
    }
}

struct Slack;
impl RenderAndIndexMd for Slack {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_slack::render_and_index_md::{parse::parse, render::render_all};
        let cursor_path = frankweiler_etl::render_cursor::cursor_path(ctx.root, "slack", ctx.name);
        let cursor = frankweiler_etl::render_cursor::read(&cursor_path)
            .with_context(|| format!("read slack render cursor {}", cursor_path.display()))?;
        let parsed = parse(
            ctx.input_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("slack parse {}", ctx.input_path.display()))?;
        render_all(&parsed, ctx.root, ctx.name, ctx.progress, on_doc)
            .context("slack render_all")
            .map(|_| ())
    }
}

struct Signal {
    period: Period,
}
impl RenderAndIndexMd for Signal {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_signal::render_and_index_md::{parse, render_all};
        let cursor_path = frankweiler_etl::render_cursor::cursor_path(ctx.root, "signal", ctx.name);
        let cursor = frankweiler_etl::render_cursor::read(&cursor_path)
            .with_context(|| format!("read signal render cursor {}", cursor_path.display()))?;
        let parsed = parse(
            ctx.input_path,
            self.period,
            ctx.name,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("signal parse {}", ctx.input_path.display()))?;
        render_all(&parsed, ctx.root, ctx.name, ctx.progress, on_doc)
            .context("signal render_all")
            .map(|_| ())
    }
}

// ─────────────────────────────────────────────────────────────────────
// Document-style, fingerprint-driven providers
// ─────────────────────────────────────────────────────────────────────

struct Github;
impl RenderAndIndexMd for Github {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_github::render_and_index_md::{parse_api_dir, render_github};
        let parsed = parse_api_dir(ctx.input_path)
            .with_context(|| format!("github parse {}", ctx.input_path.display()))?;
        render_github(
            &parsed,
            ctx.root,
            ctx.progress,
            ctx.prior_fingerprints,
            on_doc,
        )
        .context("render_github")
        .map(|_| ())
    }
}

struct Gitlab;
impl RenderAndIndexMd for Gitlab {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_gitlab::render_and_index_md::{parse_api_dir, render_gitlab};
        let parsed = parse_api_dir(ctx.input_path)
            .with_context(|| format!("gitlab parse {}", ctx.input_path.display()))?;
        render_gitlab(
            &parsed,
            ctx.root,
            ctx.progress,
            ctx.prior_fingerprints,
            on_doc,
        )
        .context("render_gitlab")
        .map(|_| ())
    }
}

struct Notion;
impl RenderAndIndexMd for Notion {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_notion::render_and_index_md::{
            parse_api_dir, render::render_notion_official,
        };
        let parsed = parse_api_dir(ctx.input_path)
            .with_context(|| format!("notion parse {}", ctx.input_path.display()))?;
        render_notion_official(
            &parsed,
            ctx.root,
            ctx.progress,
            ctx.prior_fingerprints,
            on_doc,
        )
        .context("render_notion_official")
        .map(|_| ())
    }
}

struct Beeper {
    period: Period,
}
impl RenderAndIndexMd for Beeper {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_beeper::render_and_index_md::render_all;
        let parsed =
            frankweiler_etl_beeper::render_and_index_md::parse::parse(ctx.input_path, self.period)
                .with_context(|| format!("beeper parse {}", ctx.input_path.display()))?;
        let raw_db_path = frankweiler_etl::doltlite_raw::db_path_for(ctx.input_path);
        render_all(
            &parsed,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            on_doc,
            &raw_db_path,
        )
        .context("beeper render_all")
        .map(|_| ())
    }
}

struct Perseus {
    pairs: Vec<(String, String)>,
}
impl RenderAndIndexMd for Perseus {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_perseus::render_and_index_md::{align, parse, render};
        let parsed = parse::parse(ctx.input_path)
            .with_context(|| format!("perseus parse {}", ctx.input_path.display()))?;
        // Within-section sentence alignment is opt-in per edition pair via
        // `sync.alignment_pairs` (captured as `self.pairs`). Each pair
        // loads the Ancient-Greek-BERT encoder (async: hf-hub fetch +
        // model load) and aligns multi-sentence sections — the dominant
        // cost, hence opt-in. With no pairs this is a cheap no-op. The
        // async aligner is bridged into the sync phase with
        // `Handle::current().block_on`, same as the per-doc apply path.
        let alignments = tokio::runtime::Handle::current()
            .block_on(align::align_all(&parsed, &self.pairs))
            .context("perseus align_all")?;
        render::render_all(
            &parsed,
            &alignments,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            on_doc,
        )
        .context("perseus render_all")
        .map(|_| ())
    }
}

struct WhatsApp;
impl RenderAndIndexMd for WhatsApp {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_whatsapp::render_and_index_md::{parse, render_all};
        // WhatsApp doesn't expose a `period` knob on its sync block today —
        // default to month bucketing, same as signal.
        let period = Period::from_config(None).context("default whatsapp period")?;
        let parsed = parse(ctx.input_path, period, ctx.name)
            .with_context(|| format!("whatsapp parse {}", ctx.input_path.display()))?;
        render_all(
            &parsed.chats,
            &parsed.blobs_by_chat,
            ctx.input_path,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            on_doc,
        )
        .context("whatsapp render_all")
        .map(|_| ())
    }
}

// ─────────────────────────────────────────────────────────────────────
// Raw-store-rooted providers (input lives at <data_root>/raw/<name>)
// ─────────────────────────────────────────────────────────────────────

struct Linkedin;
impl RenderAndIndexMd for Linkedin {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        // `input_path` is the export dir; the raw store (where extract
        // wrote the message tables) is the canonical
        // `<data_root>/raw/<name>` location. Every message-shaped feed
        // (DMs + AI-coach transcripts) renders.
        let raw_dir = ctx.data_root.join("raw").join(ctx.name);
        frankweiler_etl_linkedin::render::render(
            &raw_dir,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            on_doc,
        )
        .context("linkedin render")?;
        // Connections render as first-class contacts via the shared
        // contact renderer (sibling of the chat path above).
        frankweiler_etl_linkedin::connections::render_connections(
            &raw_dir,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            on_doc,
        )
        .context("linkedin connections render")?;
        // Your own posts (Shares) and the comments you left, grouped one
        // chat-style thread per post, with linkouts back to linkedin.com.
        frankweiler_etl_linkedin::posts::render_posts(
            &raw_dir,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            on_doc,
        )
        .context("linkedin posts render")
    }
}

struct GoogleTakeout;
impl RenderAndIndexMd for GoogleTakeout {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        // Only the Google Chat feed renders; the other feeds stay
        // queryable in the raw store at `<data_root>/raw/<name>`.
        let raw_dir = ctx.data_root.join("raw").join(ctx.name);
        frankweiler_etl_google_takeout::render_and_index_md::render(
            &raw_dir,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            on_doc,
        )
        .context("google_takeout render")
    }
}

struct SmsBackupRestore;
impl RenderAndIndexMd for SmsBackupRestore {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        // Texts + calls render as one chat per phone number, from the raw
        // store at `<data_root>/raw/<name>`.
        let raw_dir = ctx.data_root.join("raw").join(ctx.name);
        frankweiler_etl_sms_backup_restore::render_and_index_md::render(
            &raw_dir,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            on_doc,
        )
        .context("sms_backup_restore render")
    }
}

// ─────────────────────────────────────────────────────────────────────
// Providers with a db_dir that depends on live-sync vs. file mode
// ─────────────────────────────────────────────────────────────────────

struct Carddav {
    /// Live CardDAV sync lands its data at `input_path`; the vcf-file
    /// mode lands it at `<data_root>/raw/<name>` instead.
    from_sync: bool,
}
impl RenderAndIndexMd for Carddav {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_contacts::extract::db_path_for as carddav_db_path_for;
        use frankweiler_etl_contacts::render_and_index_md::{parse, render};
        let db_dir = if self.from_sync {
            ctx.input_path.to_path_buf()
        } else {
            ctx.data_root.join("raw").join(ctx.name)
        };
        let db_path = carddav_db_path_for(&db_dir);
        let parsed = parse::parse(&db_path)
            .with_context(|| format!("carddav parse {}", db_path.display()))?;
        render::render_all(
            &parsed,
            ctx.root,
            ctx.name,
            ctx.progress,
            ctx.prior_fingerprints,
            on_doc,
        )
        .context("carddav render_all")
        .map(|_| ())
    }
}

struct Email {
    from_sync: bool,
    outlink: Option<frankweiler_etl_email::render_and_index_md::render::OutlinkFormat>,
}
impl RenderAndIndexMd for Email {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_email::extract::db_path_for as jmap_db_path_for;
        use frankweiler_etl_email::render_and_index_md::parse::parse;
        use frankweiler_etl_email::render_and_index_md::render::render_all;

        let db_dir = if self.from_sync {
            ctx.input_path.to_path_buf()
        } else {
            ctx.data_root.join("raw").join(ctx.name)
        };
        let db = jmap_db_path_for(&db_dir);
        if !db.exists() {
            status_line!(
                "[render_and_index_md] {} (email): no raw db at {} — skipping",
                ctx.name,
                db.display(),
            );
            return Ok(());
        }
        // Two-phase parse driven by `dolt_diff_<table>`: phase 1 asks
        // doltlite which threads changed since the render cursor's commit;
        // phase 2 loads only those threads. `prior_fingerprints` is
        // ignored here — the cursor is the single source of truth.
        let cursor_path = frankweiler_etl::render_cursor::cursor_path(ctx.root, "email", ctx.name);
        let cursor = frankweiler_etl::render_cursor::read(&cursor_path)
            .with_context(|| format!("read email render cursor {}", cursor_path.display()))?;
        let parsed = parse(&db, cursor.as_ref().map(|c| c.last_rendered_hash.as_str()))
            .with_context(|| format!("email parse {}", db.display()))?;
        render_all(
            &parsed,
            ctx.root,
            ctx.name,
            self.outlink,
            ctx.progress,
            on_doc,
        )
        .context("email render_all")
        .map(|_| ())
    }
}

// ─────────────────────────────────────────────────────────────────────
// Extract-only providers: nothing to render
// ─────────────────────────────────────────────────────────────────────

struct Skip {
    provider: &'static str,
    reason: &'static str,
}
impl RenderAndIndexMd for Skip {
    fn run(&self, ctx: &RenderCtx, _on_doc: &mut OnDoc) -> Result<()> {
        status_line!(
            "[render_and_index_md] {} ({}): skipped ({})",
            ctx.name,
            self.provider,
            self.reason
        );
        Ok(())
    }
}
