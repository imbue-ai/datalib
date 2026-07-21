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
use frankweiler_etl::processor::{DataProcessor, PlanContext};
use frankweiler_source_common::{Defaults, DownloadParams};

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
    "linkedin",
    "notion_api",
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
                // FRANKWEILER_HTTP_PLAYBACK env (set by `download
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
            frankweiler_etl_anthropic_config::AnthropicConfig,
            frankweiler_etl_anthropic_config::AnthropicRenderConfig,
            frankweiler_etl_anthropic,
            "claude_api"
        ),
        "claude_export" => arm!(
            frankweiler_etl_anthropic_config::AnthropicConfig,
            frankweiler_etl_anthropic_config::AnthropicRenderConfig,
            frankweiler_etl_anthropic,
            "claude_export"
        ),
        "chatgpt_api" => arm!(
            frankweiler_etl_chatgpt_config::ChatgptConfig,
            frankweiler_etl_chatgpt_config::ChatgptRenderConfig,
            frankweiler_etl_chatgpt,
            "chatgpt_api"
        ),
        "slack_api" => arm!(
            frankweiler_etl_slack_config::SlackConfig,
            frankweiler_etl_slack_config::SlackRenderConfig,
            frankweiler_etl_slack,
            "slack_api"
        ),
        "github_api" => arm!(
            frankweiler_etl_github_config::GithubConfig,
            frankweiler_etl_github_config::GithubRenderConfig,
            frankweiler_etl_github,
            "github_api"
        ),
        "gitlab_api" => arm!(
            frankweiler_etl_gitlab_config::GitlabConfig,
            frankweiler_etl_gitlab_config::GitlabRenderConfig,
            frankweiler_etl_gitlab,
            "gitlab_api"
        ),
        "notion_api" => arm!(
            frankweiler_etl_notion_config::NotionConfig,
            frankweiler_etl_notion_config::NotionRenderConfig,
            frankweiler_etl_notion,
            "notion_api"
        ),
        "email" => arm!(
            frankweiler_etl_email_config::EmailConfig,
            frankweiler_etl_email_config::EmailRenderConfig,
            frankweiler_etl_email,
            "email"
        ),
        "beeper" => arm!(
            frankweiler_etl_beeper_config::BeeperConfig,
            frankweiler_etl_beeper_config::BeeperRenderConfig,
            frankweiler_etl_beeper,
            "beeper"
        ),
        "carddav" => arm!(
            frankweiler_etl_carddav_config::CarddavConfig,
            frankweiler_etl_carddav_config::CarddavRenderConfig,
            frankweiler_etl_contacts,
            "carddav"
        ),
        "linkedin" => arm!(
            frankweiler_etl_linkedin_config::LinkedinConfig,
            frankweiler_etl_linkedin_config::LinkedinRenderConfig,
            frankweiler_etl_linkedin,
            "linkedin"
        ),
        "google_takeout" => arm!(
            frankweiler_etl_google_takeout_config::GoogleTakeoutConfig,
            frankweiler_etl_google_takeout_config::GoogleTakeoutRenderConfig,
            frankweiler_etl_google_takeout,
            "google_takeout"
        ),
        "perseus" => arm!(
            frankweiler_etl_perseus_config::PerseusConfig,
            frankweiler_etl_perseus_config::PerseusRenderConfig,
            frankweiler_etl_perseus,
            "perseus"
        ),
        "yolink" => arm!(
            frankweiler_etl_yolink_config::YolinkConfig,
            frankweiler_etl_yolink_config::YolinkRenderConfig,
            frankweiler_etl_yolink,
            "yolink"
        ),
        "signal_backup" => arm!(
            frankweiler_etl_signal_config::SignalConfig,
            frankweiler_etl_signal_config::SignalRenderConfig,
            frankweiler_etl_signal,
            "signal_backup"
        ),
        "whatsapp_backup" => arm!(
            frankweiler_etl_whatsapp_config::WhatsappConfig,
            frankweiler_etl_whatsapp_config::WhatsappRenderConfig,
            frankweiler_etl_whatsapp,
            "whatsapp_backup"
        ),
        "sms_backup_restore" => arm!(
            frankweiler_etl_sms_backup_restore_config::SmsBackupRestoreConfig,
            frankweiler_etl_sms_backup_restore_config::SmsBackupRestoreRenderConfig,
            frankweiler_etl_sms_backup_restore,
            "sms_backup_restore"
        ),
        "fsindex" => arm!(
            frankweiler_etl_fsindex_config::FsindexConfig,
            frankweiler_etl_fsindex_config::FsindexRenderConfig,
            frankweiler_etl_fsindex,
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
