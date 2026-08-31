//! `(source type, phase)` → provider dispatch.
//!
//! With per-provider step types (`download slack_api`,
//! `render slack_api`, …) the params carry no `type:` tag — the
//! nested subcommand names the provider, and the `--params` subtree
//! deserializes straight into that provider's own **per-phase**
//! config struct: the full `<P>Config` for download (normalized like
//! the old `Config::normalize` — fold built-in defaults, resolve
//! paths, validate), the slim `<P>RenderConfig` for render (just the
//! `RenderCommon` envelope plus any render knobs — `deny_unknown`, so
//! each step's params carry only what that wave reads). Each arm then
//! calls the provider's per-wave entry point
//! (`plan_download` / `plan_render`).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use datalib_etl::processor::{DataProcessor, PlanContext};
use datalib_source_common::{Defaults, DownloadParams};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Download,
    Render,
}

/// A normalized, planned source: the per-wave processors plus the
/// resolved envelope facts the step driver needs.
pub struct PlannedSource {
    pub name: String,
    pub type_str: &'static str,
    /// Resolved raw-store dir (`<data_root>/<name>/raw` unless
    /// overridden via `common.raw_path`).
    pub raw_path: PathBuf,
    /// Resolved rate-limit give-up bounds for the download wave.
    pub download_params: DownloadParams,
    pub processors: Vec<Box<dyn DataProcessor>>,
}

impl std::fmt::Debug for PlannedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlannedSource")
            .field("name", &self.name)
            .field("type_str", &self.type_str)
            .field("raw_path", &self.raw_path)
            .field("processors", &self.processors.len())
            .finish_non_exhaustive()
    }
}

impl PlannedSource {
    /// The canonical data-root-relative path of a phase's output
    /// (`<name>/raw`, `<name>/rendered_md`). `None` when the resolved
    /// path was overridden away from the canonical layout — then no
    /// output claims are made and the scheduler hashes whatever the
    /// config declared.
    pub fn canonical_rel(&self, data_root: &Path, phase_dir: &str) -> Option<String> {
        let rel = format!("{}/{}", self.name, phase_dir);
        if phase_dir == "raw" && self.raw_path != data_root.join(&rel) {
            return None;
        }
        Some(rel)
    }
}

/// All source type strings, mirroring the old `SourceConfig` wire
/// discriminators. Kept sorted for the error message.
pub const SOURCE_TYPES: &[&str] = &[
    "beeper",
    "carddav",
    "chatgpt_api",
    "claude_api",
    "claude_export",
    "email",
    "fsindex",
    "github_api",
    "gitlab_api",
    "google_takeout",
    "lightroom",
    "linkedin",
    "notion_api",
    "pdf",
    "perseus",
    "signal_backup",
    "slack_api",
    "sms_backup_restore",
    "whatsapp_backup",
    "yolink",
];

pub fn plan(
    step_type: &str,
    phase: Phase,
    name: &str,
    source: serde_json::Value,
    data_root: &Path,
) -> Result<PlannedSource> {
    macro_rules! arm {
        ($cfgty:ty, $rcfgty:ty, $provider:ident, $tstr:expr) => {{
            let ctx = PlanContext {
                name: name.to_string(),
                // Playback redirection goes through the
                // DATALIB_HTTP_PLAYBACK env (set by `download
                // --playback-root`), not per-plan.
                playback_root: None,
            };
            match phase {
                Phase::Download => {
                    let mut cfg: $cfgty = serde_json::from_value(source).with_context(|| {
                        format!("parse --params as a {} download config", $tstr)
                    })?;
                    // No global `defaults:` stanza in DAG mode (each step
                    // is self-contained): fold the built-in defaults only.
                    cfg.common.fold_defaults(&Defaults::default());
                    cfg.common.resolve_paths(data_root, name);
                    cfg.validate()
                        .with_context(|| format!("source {name:?} (type={})", $tstr))?;
                    let raw_path = cfg.common.raw_path().to_path_buf();
                    let download_params = cfg.common.download_params.clone();
                    PlannedSource {
                        name: name.to_string(),
                        type_str: $tstr,
                        raw_path,
                        download_params,
                        processors: $provider::processor::plan_download(ctx, cfg)?,
                    }
                }
                Phase::Render => {
                    // Per-phase params split: render deserializes its own
                    // slim config (deny_unknown_fields, so download-shaped
                    // params on a render step fail loudly). No defaults to
                    // fold — render carries no cross-source knobs.
                    let mut cfg: $rcfgty = serde_json::from_value(source)
                        .with_context(|| format!("parse --params as a {} render config", $tstr))?;
                    cfg.common.resolve_paths(data_root, name);
                    let raw_path = cfg.common.raw_path().to_path_buf();
                    PlannedSource {
                        name: name.to_string(),
                        type_str: $tstr,
                        raw_path,
                        // Rate-limit bounds are download-only machinery.
                        download_params: Default::default(),
                        processors: $provider::processor::plan_render(ctx, cfg)?,
                    }
                }
            }
        }};
    }

    Ok(match step_type {
        "claude_api" => arm!(
            datalib_etl_anthropic_config::AnthropicConfig,
            datalib_etl_anthropic_config::AnthropicRenderConfig,
            datalib_etl_anthropic,
            "claude_api"
        ),
        "claude_export" => arm!(
            datalib_etl_anthropic_config::AnthropicConfig,
            datalib_etl_anthropic_config::AnthropicRenderConfig,
            datalib_etl_anthropic,
            "claude_export"
        ),
        "chatgpt_api" => arm!(
            datalib_etl_chatgpt_config::ChatgptConfig,
            datalib_etl_chatgpt_config::ChatgptRenderConfig,
            datalib_etl_chatgpt,
            "chatgpt_api"
        ),
        "slack_api" => arm!(
            datalib_etl_slack_config::SlackConfig,
            datalib_etl_slack_config::SlackRenderConfig,
            datalib_etl_slack,
            "slack_api"
        ),
        "github_api" => arm!(
            datalib_etl_github_config::GithubConfig,
            datalib_etl_github_config::GithubRenderConfig,
            datalib_etl_github,
            "github_api"
        ),
        "gitlab_api" => arm!(
            datalib_etl_gitlab_config::GitlabConfig,
            datalib_etl_gitlab_config::GitlabRenderConfig,
            datalib_etl_gitlab,
            "gitlab_api"
        ),
        "notion_api" => arm!(
            datalib_etl_notion_config::NotionConfig,
            datalib_etl_notion_config::NotionRenderConfig,
            datalib_etl_notion,
            "notion_api"
        ),
        "email" => arm!(
            datalib_etl_email_config::EmailConfig,
            datalib_etl_email_config::EmailRenderConfig,
            datalib_etl_email,
            "email"
        ),
        "beeper" => arm!(
            datalib_etl_beeper_config::BeeperConfig,
            datalib_etl_beeper_config::BeeperRenderConfig,
            datalib_etl_beeper,
            "beeper"
        ),
        "carddav" => arm!(
            datalib_etl_carddav_config::CarddavConfig,
            datalib_etl_carddav_config::CarddavRenderConfig,
            datalib_etl_contacts,
            "carddav"
        ),
        "linkedin" => arm!(
            datalib_etl_linkedin_config::LinkedinConfig,
            datalib_etl_linkedin_config::LinkedinRenderConfig,
            datalib_etl_linkedin,
            "linkedin"
        ),
        "google_takeout" => arm!(
            datalib_etl_google_takeout_config::GoogleTakeoutConfig,
            datalib_etl_google_takeout_config::GoogleTakeoutRenderConfig,
            datalib_etl_google_takeout,
            "google_takeout"
        ),
        "pdf" => arm!(
            datalib_etl_pdf_config::PdfConfig,
            datalib_etl_pdf_config::PdfRenderConfig,
            datalib_etl_pdf,
            "pdf"
        ),
        "perseus" => arm!(
            datalib_etl_perseus_config::PerseusConfig,
            datalib_etl_perseus_config::PerseusRenderConfig,
            datalib_etl_perseus,
            "perseus"
        ),
        "yolink" => arm!(
            datalib_etl_yolink_config::YolinkConfig,
            datalib_etl_yolink_config::YolinkRenderConfig,
            datalib_etl_yolink,
            "yolink"
        ),
        "signal_backup" => arm!(
            datalib_etl_signal_config::SignalConfig,
            datalib_etl_signal_config::SignalRenderConfig,
            datalib_etl_signal,
            "signal_backup"
        ),
        "whatsapp_backup" => arm!(
            datalib_etl_whatsapp_config::WhatsappConfig,
            datalib_etl_whatsapp_config::WhatsappRenderConfig,
            datalib_etl_whatsapp,
            "whatsapp_backup"
        ),
        "sms_backup_restore" => arm!(
            datalib_etl_sms_backup_restore_config::SmsBackupRestoreConfig,
            datalib_etl_sms_backup_restore_config::SmsBackupRestoreRenderConfig,
            datalib_etl_sms_backup_restore,
            "sms_backup_restore"
        ),
        "lightroom" => arm!(
            datalib_etl_lightroom_config::LightroomConfig,
            datalib_etl_lightroom_config::LightroomRenderConfig,
            datalib_etl_lightroom,
            "lightroom"
        ),
        "fsindex" => arm!(
            datalib_etl_fsindex_config::FsindexConfig,
            datalib_etl_fsindex_config::FsindexRenderConfig,
            datalib_etl_fsindex,
            "fsindex"
        ),
        other => bail!(
            "unknown source type {other:?}; known types: {}",
            SOURCE_TYPES.join(", ")
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_slack_download_and_render_from_phase_params() {
        let td = tempfile::tempdir().unwrap();
        let source: serde_json::Value = serde_json::json!({
            "sync": {"media": true, "channels": ["chat-qi"], "since": "2026-06-15"}
        });
        let dl = plan(
            "slack_api",
            Phase::Download,
            "slack",
            source.clone(),
            td.path(),
        )
        .unwrap();
        assert_eq!(dl.type_str, "slack_api");
        assert_eq!(dl.raw_path, td.path().join("slack/raw"));
        assert_eq!(dl.processors.len(), 1);
        assert_eq!(
            dl.canonical_rel(td.path(), "raw").as_deref(),
            Some("slack/raw")
        );

        // Render params are phase-specific: slack render needs none.
        let rn = plan(
            "slack_api",
            Phase::Render,
            "slack",
            serde_json::json!({}),
            td.path(),
        )
        .unwrap();
        assert_eq!(rn.processors.len(), 1);

        // Download-shaped params on a render step fail loudly instead
        // of being silently ignored.
        let err = plan("slack_api", Phase::Render, "slack", source, td.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("render config"), "{err}");
    }

    /// Every slack config written before `dms` existed must keep
    /// parsing, and must keep meaning "no direct messages". The struct
    /// is `deny_unknown_fields`, so this is really two guarantees: the
    /// old shape still deserializes, and the new field defaults off
    /// rather than opting an existing mirror into DMs on upgrade.
    #[test]
    fn slack_config_without_dms_still_parses_and_leaves_dms_off() {
        let cfg: datalib_etl_slack_config::SlackConfig = serde_json::from_value(
            serde_json::json!({"sync": {"media": true, "channels": ["chat-qi"]}}),
        )
        .expect("a pre-dms config must still parse");
        let sync = cfg.sync.expect("sync");
        assert!(!sync.dms, "an upgrade must not start mirroring DMs");
        assert!(sync.dm_users.is_none());
    }

    /// The one combination the provider refuses, refused where the
    /// step actually reads its params — `plan` is what calls
    /// `validate`, and a rule that isn't wired into it is not enforced.
    #[test]
    fn slack_dm_users_without_dms_fails_at_plan_time() {
        let td = tempfile::tempdir().unwrap();
        let err = plan(
            "slack_api",
            Phase::Download,
            "slack",
            serde_json::json!({"sync": {"dm_users": ["@riker"]}}),
            td.path(),
        )
        .unwrap_err();
        // `{:#}` walks the cause chain, which is what `main.rs` prints
        // (one line per `e.chain()` entry) — the bare `to_string()` is
        // only the outermost "source ... (type=slack_api)" context.
        let err = format!("{err:#}");
        assert!(err.contains("dm_users"), "{err}");
        assert!(err.contains("dms = true"), "{err}");

        // …and is accepted with the switch on.
        plan(
            "slack_api",
            Phase::Download,
            "slack",
            serde_json::json!({"sync": {"dms": true, "dm_users": ["@riker"]}}),
            td.path(),
        )
        .expect("dms = true with an allowlist is the supported shape");
    }

    #[test]
    fn render_knobs_are_rejected_on_download_and_read_on_render() {
        let td = tempfile::tempdir().unwrap();
        // `period` used to live in `sync:`; the download planner points
        // at its new home on the render step.
        let err = plan(
            "beeper",
            Phase::Download,
            "beeper",
            serde_json::json!({"sync": {"sources": ["signal"], "period": "day"}}),
            td.path(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("render step's params"), "{err}");

        let rn = plan(
            "beeper",
            Phase::Render,
            "beeper",
            serde_json::json!({"period": "day"}),
            td.path(),
        )
        .unwrap();
        assert_eq!(rn.processors.len(), 1);
    }

    #[test]
    fn download_without_sync_plans_empty_for_api_sources() {
        let td = tempfile::tempdir().unwrap();
        let dl = plan(
            "claude_export",
            Phase::Download,
            "claude",
            serde_json::json!({}),
            td.path(),
        )
        .unwrap();
        assert!(dl.processors.is_empty(), "claude_export is render-only");
    }

    #[test]
    fn unknown_type_lists_known_ones() {
        let td = tempfile::tempdir().unwrap();
        let err = plan(
            "carrier_pigeon",
            Phase::Download,
            "x",
            serde_json::json!({}),
            td.path(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("slack_api"), "{err}");
    }

    #[test]
    fn overridden_raw_path_gets_no_canonical_claim() {
        let td = tempfile::tempdir().unwrap();
        let dl = plan(
            "github_api",
            Phase::Download,
            "gh",
            serde_json::json!({"common": {"raw_path": "/mnt/big/gh-raw"}, "sync": {}}),
            td.path(),
        )
        .unwrap();
        assert_eq!(dl.canonical_rel(td.path(), "raw"), None);
    }
}
