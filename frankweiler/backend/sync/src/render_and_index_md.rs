//! Render-and-index-md: the per-source step that projects a provider's
//! extracted data into the universal presentable shape — `grid_rows`
//! sidecars + rendered markdown documents.
//!
//! Every provider implements one uniform interface, [`RenderAndIndexMd`].
//! [`renderer_for`] selects the implementation **by the source's `type`
//! string** and hands it the source's config as an **opaque YAML stanza**
//! ([`serde_yaml::Value`]). The orchestrator never inspects a provider's
//! config fields — each provider deserializes the knobs it cares about
//! out of its own stanza (`Beeper`/`Signal` read `sync.period`, `Email`
//! reads `outlink_format`, `Perseus` reads `sync.alignment_pairs`). This
//! is the "config lives with the step, orchestrator forwards an opaque
//! subtree" direction from issue #23's comment thread: the registry has
//! no dependency on `frankweiler_core::config` at all.
//!
//! The tradeoff vs. matching a typed `SourceConfig` enum is that an
//! unknown `type` is a runtime error here rather than a compile error —
//! deliberate, per the opacity decision.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_yaml::Value;

use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::periodize::Period;
use frankweiler_etl::progress::Progress;
use frankweiler_obs::status_line;

/// Everything a render-and-index-md step needs that is common across
/// providers. Provider-specific knobs are parsed by the adapter from its
/// opaque stanza (see [`renderer_for`]); they never appear here.
pub struct RenderCtx<'a> {
    /// Workspace root — the parent of the `rendered_md/` tree the step
    /// writes markdown + `.grid_rows.json` sidecars into.
    pub root: &'a Path,
    /// Source name (`sources[].name` in config.yaml).
    pub name: &'a str,
    /// `src.resolved_raw_path(data_root)` — the source's raw store
    /// directory (holds `entities.doltlite_db`, `blobs.doltlite_db`, and
    /// the `events/` tape; see [`frankweiler_etl::raw_layout`]). The same
    /// directory extract wrote, now read back. Every provider reads here;
    /// none needs the original `input_path` export location.
    pub raw_path: &'a Path,
    /// Progress hook for the per-source bar.
    pub progress: &'a Progress,
    /// Per-markdown source fingerprints from the prior run, for providers
    /// whose incremental skip is fingerprint-driven (rather than
    /// render-cursor-driven).
    pub prior_fingerprints: &'a HashMap<String, String>,
}

/// One callback per rendered markdown — hands the document (its path +
/// row set) to the orchestrator's inline Load step.
///
/// `Send` so the same callback type flows into a Program-A translate
/// `DataProcessor`'s `RunCtx` (whose `run` future is `Send`). The
/// orchestrator's Load closure is already `Send` (it's moved into a
/// `spawn_blocking` task), so this is a no-op widening for every caller.
pub type OnDoc<'a> = dyn FnMut(RenderedMarkdown) -> Result<()> + Send + 'a;

/// A provider's render-and-index-md step. One implementation per data
/// source; [`renderer_for`] selects it.
pub trait RenderAndIndexMd {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()>;
}

/// Select a render-and-index-md implementation by the source's `type`
/// string, handing each provider its config as an opaque YAML stanza to
/// parse for itself. The orchestrator forwards `stanza` without looking
/// inside it.
pub fn renderer_for(type_str: &str, stanza: &Value) -> Result<Box<dyn RenderAndIndexMd>> {
    Ok(match type_str {
        "claude_api" | "claude_export" => Box::new(Anthropic),
        "chatgpt_api" => Box::new(Chatgpt),
        "slack_api" => Box::new(Slack),
        "github_api" => Box::new(Github),
        "gitlab_api" => Box::new(Gitlab),
        "notion_api" => Box::new(Notion),
        "beeper" => Box::new(Beeper::from_stanza(stanza)?),
        "carddav" => Box::new(Carddav),
        "linkedin" => Box::new(Linkedin),
        "google_takeout" => Box::new(GoogleTakeout),
        "sms_backup_restore" => Box::new(SmsBackupRestore),
        "email" => Box::new(Email::from_stanza(stanza)?),
        "perseus" => Box::new(Perseus::from_stanza(stanza)?),
        "signal_backup" => Box::new(Signal::from_stanza(stanza)?),
        "whatsapp_backup" => Box::new(WhatsApp),
        "yolink" => Box::new(Skip {
            provider: "yolink",
            reason: "extract-only, no render path",
        }),
        other => {
            return Err(anyhow!(
                "no render-and-index-md step registered for source type `{other}`"
            ))
        }
    })
}

// ─────────────────────────────────────────────────────────────────────
// Opaque-stanza config fragments
//
// Each fragment names only the keys its provider cares about and ignores
// everything else in the source's YAML (no `deny_unknown_fields`). These
// are the only place the provider's config *shape* is known — moving an
// adapter into its own crate later moves its fragment with it.
// ─────────────────────────────────────────────────────────────────────

/// The `sync.period` knob shared by the period-bucketing chat providers
/// (beeper, signal).
#[derive(Deserialize, Default)]
struct PeriodStanza {
    #[serde(default)]
    sync: Option<SyncPeriod>,
}
#[derive(Deserialize, Default)]
struct SyncPeriod {
    #[serde(default)]
    period: Option<String>,
}
impl PeriodStanza {
    fn parse(stanza: &Value) -> Result<Period> {
        let s: PeriodStanza =
            serde_yaml::from_value(stanza.clone()).context("parse period config")?;
        Period::from_config(s.sync.as_ref().and_then(|p| p.period.as_deref()))
            .context("parse period")
    }
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
            ctx.raw_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("anthropic parse {}", ctx.raw_path.display()))?;
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
            ctx.raw_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("chatgpt parse {}", ctx.raw_path.display()))?;
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
            ctx.raw_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("slack parse {}", ctx.raw_path.display()))?;
        render_all(&parsed, ctx.root, ctx.name, ctx.progress, on_doc)
            .context("slack render_all")
            .map(|_| ())
    }
}

struct Signal {
    period: Period,
}
impl Signal {
    fn from_stanza(stanza: &Value) -> Result<Self> {
        Ok(Signal {
            period: PeriodStanza::parse(stanza).context("signal period")?,
        })
    }
}
impl RenderAndIndexMd for Signal {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_signal::render_and_index_md::{parse, render_all};
        let cursor_path = frankweiler_etl::render_cursor::cursor_path(ctx.root, "signal", ctx.name);
        let cursor = frankweiler_etl::render_cursor::read(&cursor_path)
            .with_context(|| format!("read signal render cursor {}", cursor_path.display()))?;
        let parsed = parse(
            ctx.raw_path,
            self.period,
            ctx.name,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("signal parse {}", ctx.raw_path.display()))?;
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
        let parsed = parse_api_dir(ctx.raw_path)
            .with_context(|| format!("github parse {}", ctx.raw_path.display()))?;
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
        let parsed = parse_api_dir(ctx.raw_path)
            .with_context(|| format!("gitlab parse {}", ctx.raw_path.display()))?;
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
        let parsed = parse_api_dir(ctx.raw_path)
            .with_context(|| format!("notion parse {}", ctx.raw_path.display()))?;
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
impl Beeper {
    fn from_stanza(stanza: &Value) -> Result<Self> {
        Ok(Beeper {
            period: PeriodStanza::parse(stanza).context("beeper period")?,
        })
    }
}
impl RenderAndIndexMd for Beeper {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_beeper::render_and_index_md::render_all;
        let parsed =
            frankweiler_etl_beeper::render_and_index_md::parse::parse(ctx.raw_path, self.period)
                .with_context(|| format!("beeper parse {}", ctx.raw_path.display()))?;
        let raw_db_path = frankweiler_etl::doltlite_raw::db_path_for(ctx.raw_path);
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
impl Perseus {
    fn from_stanza(stanza: &Value) -> Result<Self> {
        #[derive(Deserialize, Default)]
        struct PerseusStanza {
            #[serde(default)]
            sync: Option<PerseusSync>,
        }
        #[derive(Deserialize, Default)]
        struct PerseusSync {
            #[serde(default)]
            alignment_pairs: Vec<[String; 2]>,
        }
        let s: PerseusStanza =
            serde_yaml::from_value(stanza.clone()).context("parse perseus config")?;
        let pairs = s
            .sync
            .map(|sync| {
                sync.alignment_pairs
                    .into_iter()
                    .map(|[a, b]| (a, b))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Perseus { pairs })
    }
}
impl RenderAndIndexMd for Perseus {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_perseus::render_and_index_md::{align, parse, render};
        let parsed = parse::parse(ctx.raw_path)
            .with_context(|| format!("perseus parse {}", ctx.raw_path.display()))?;
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
        let parsed = parse(ctx.raw_path, period, ctx.name)
            .with_context(|| format!("whatsapp parse {}", ctx.raw_path.display()))?;
        render_all(
            &parsed.chats,
            &parsed.blobs_by_chat,
            ctx.raw_path,
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
// File-backed providers (data came from an export; render reads the raw
// store we wrote, `ctx.raw_path`, ignoring the original export location)
// ─────────────────────────────────────────────────────────────────────

struct Linkedin;
impl RenderAndIndexMd for Linkedin {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        // Every message-shaped feed (DMs + AI-coach transcripts) renders.
        frankweiler_etl_linkedin::render::render(
            ctx.raw_path,
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
            ctx.raw_path,
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
            ctx.raw_path,
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
        // queryable in the raw store.
        frankweiler_etl_google_takeout::render_and_index_md::render(
            ctx.raw_path,
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
        // Texts + calls render as one chat per phone number.
        frankweiler_etl_sms_backup_restore::render_and_index_md::render(
            ctx.raw_path,
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
// Contacts / email — read the raw store; live-sync vs. file mode no
// longer affects the path (both land at `ctx.raw_path`)
// ─────────────────────────────────────────────────────────────────────

struct Carddav;
impl RenderAndIndexMd for Carddav {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_contacts::extract::db_path_for as carddav_db_path_for;
        use frankweiler_etl_contacts::render_and_index_md::{parse, render};
        let db_path = carddav_db_path_for(ctx.raw_path);
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

/// The e-mail "open in webmail" link flavor, parsed straight from the
/// opaque stanza so this registry doesn't depend on the config crate's
/// `EmailOutlink`. Mirrors the YAML values (`gmail` / `fastmail`).
#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum OutlinkFlavor {
    Gmail,
    Fastmail,
}

struct Email {
    outlink: Option<frankweiler_etl_email::render_and_index_md::render::OutlinkFormat>,
    /// Render-time label filter (full POSIX-like mailbox paths). Empty =
    /// render every thread. Separate from the extract-time
    /// `only_extract_labels`, so a giant inbox can be extracted in full
    /// but rendered down to a subset.
    only_render_labels: Vec<String>,
}
impl Email {
    fn from_stanza(stanza: &Value) -> Result<Self> {
        use frankweiler_etl_email::render_and_index_md::render::OutlinkFormat;
        #[derive(Deserialize, Default)]
        struct EmailStanza {
            #[serde(default)]
            outlink_format: Option<OutlinkFlavor>,
            #[serde(default)]
            only_render_labels: Vec<String>,
        }
        let s: EmailStanza =
            serde_yaml::from_value(stanza.clone()).context("parse email config")?;
        let outlink = s.outlink_format.map(|f| match f {
            OutlinkFlavor::Gmail => OutlinkFormat::Gmail,
            OutlinkFlavor::Fastmail => OutlinkFormat::Fastmail,
        });
        Ok(Email {
            outlink,
            only_render_labels: s.only_render_labels,
        })
    }
}
impl RenderAndIndexMd for Email {
    fn run(&self, ctx: &RenderCtx, on_doc: &mut OnDoc) -> Result<()> {
        use frankweiler_etl_email::extract::db_path_for as jmap_db_path_for;
        use frankweiler_etl_email::render_and_index_md::parse::parse;
        use frankweiler_etl_email::render_and_index_md::render::render_all;

        let db = jmap_db_path_for(ctx.raw_path);
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
            &self.only_render_labels,
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

#[cfg(test)]
mod tests {
    use super::*;
    use frankweiler_etl_email::render_and_index_md::render::OutlinkFormat;

    fn yaml(s: &str) -> Value {
        serde_yaml::from_str(s).expect("valid yaml stanza")
    }

    #[test]
    fn period_reads_sync_period_with_month_default() {
        // Explicit period under the sync block.
        let v = yaml("sync:\n  sources: [signal]\n  period: year\n");
        assert_eq!(PeriodStanza::parse(&v).unwrap(), Period::Year);
        // sync block present but no period → default Month.
        let v = yaml("sync:\n  sources: [signal]\n");
        assert_eq!(PeriodStanza::parse(&v).unwrap(), Period::Month);
        // No sync block at all (e.g. file-mode) → default Month, no error.
        let v = yaml("name: wa\ntype: whatsapp_backup\n");
        assert_eq!(PeriodStanza::parse(&v).unwrap(), Period::Month);
    }

    #[test]
    fn email_reads_outlink() {
        let e = Email::from_stanza(&yaml("sync:\n  host: x\noutlink_format: gmail\n")).unwrap();
        assert!(matches!(e.outlink, Some(OutlinkFormat::Gmail)));

        let e = Email::from_stanza(&yaml("outlink_format: fastmail\n")).unwrap();
        assert!(matches!(e.outlink, Some(OutlinkFormat::Fastmail)));

        // mbox / file mode: no outlink.
        let e = Email::from_stanza(&yaml("input_path: /tmp/mail.mbox\n")).unwrap();
        assert!(e.outlink.is_none());
    }

    #[test]
    fn perseus_reads_alignment_pairs() {
        let p =
            Perseus::from_stanza(&yaml("sync:\n  alignment_pairs:\n    - [grc, eng]\n")).unwrap();
        assert_eq!(p.pairs, vec![("grc".to_string(), "eng".to_string())]);
        // The default example shape (`sync: { files: [] }`, no pairs) → none.
        let p = Perseus::from_stanza(&yaml("sync:\n  files: []\n")).unwrap();
        assert!(p.pairs.is_empty());
    }

    #[test]
    fn unknown_type_is_a_runtime_error() {
        assert!(renderer_for("not_a_real_source", &Value::Null).is_err());
    }

    /// Locks the load-bearing assumption of this whole step: the orchestrator
    /// hands us `serde_yaml::to_value(&SourceConfig)`, and a provider must be
    /// able to recover its knob from that serialized form. Uses a *non-default*
    /// period (`year`) so the assertion can't pass by falling back to `Month`.
    #[test]
    fn typed_config_roundtrips_through_to_value() {
        use frankweiler_core::config::Config;
        let cfg: Config = serde_yaml::from_str(concat!(
            "data_root: /tmp/x\n",
            "sources:\n",
            "  - name: beeper\n",
            "    type: beeper\n",
            "    sync:\n",
            "      sources: [signal]\n",
            "      period: year\n",
        ))
        .expect("parse config");
        let src = &cfg.sources[0];
        assert_eq!(src.type_str(), "beeper");
        let stanza = serde_yaml::to_value(src).expect("serialize source");
        assert_eq!(PeriodStanza::parse(&stanza).unwrap(), Period::Year);
        assert!(renderer_for(src.type_str(), &stanza).is_ok());
    }

    #[test]
    fn every_registered_type_dispatches() {
        // Knobless + skip providers ignore the stanza entirely.
        for t in [
            "claude_api",
            "claude_export",
            "chatgpt_api",
            "slack_api",
            "github_api",
            "gitlab_api",
            "notion_api",
            "linkedin",
            "google_takeout",
            "sms_backup_restore",
            "whatsapp_backup",
            "yolink",
        ] {
            assert!(
                renderer_for(t, &Value::Null).is_ok(),
                "type {t} should dispatch"
            );
        }
        // Knob providers accept an empty mapping — every knob is optional.
        let empty = yaml("{}");
        for t in ["beeper", "signal_backup", "carddav", "email", "perseus"] {
            assert!(renderer_for(t, &empty).is_ok(), "type {t} should dispatch");
        }
    }
}
