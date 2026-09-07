//! `(source type, phase)` → provider dispatch.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::processor::{DataProcessor, PlanContext};
use datalib_source_common::{Defaults, DownloadParams};

use crate::source_type::SourceType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Download,
    Render,
}

/// A normalized, planned source: the per-wave processors plus the
/// resolved envelope facts the step driver needs.
pub struct PlannedSource {
    pub name: String,
    pub source_type: SourceType,
    /// Resolved raw-store dir (`<data_root>/<name>/raw` unless
    /// overridden via `common.raw_path`).
    pub raw_path: PathBuf,
    /// Resolved rate-limit give-up bounds for the download wave.
    pub download_params: DownloadParams,
    /// `common.always_clear_before_ingest`, resolved. Download wave only —
    /// render rewrites its own tree already.
    pub always_clear_before_ingest: bool,
    pub processors: Vec<Box<dyn DataProcessor>>,
}

impl std::fmt::Debug for PlannedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlannedSource")
            .field("name", &self.name)
            .field("source_type", &self.source_type)
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

pub fn plan(
    step_type: &str,
    phase: Phase,
    name: &str,
    source: serde_json::Value,
    data_root: &Path,
) -> Result<PlannedSource> {
    // Declared before `arm!`: a `macro_rules!` body only sees bindings
    // that exist at its definition site.
    let source_type = SourceType::parse(step_type).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown source type {step_type:?}; known types: {}",
            SourceType::known_list()
        )
    })?;

    macro_rules! arm {
        // The usual shape: a provider's two waves are its
        // `plan_download` / `plan_render` pair.
        ($cfgty:ty, $rcfgty:ty, $provider:ident) => {
            arm!($cfgty, $rcfgty, $provider, plan_download, plan_render)
        };
        // …and the shape for a provider serving more than one source
        // type, which needs a different entry point per type. Only
        // claude does: `claude_api` walks the live API,
        // `claude_export` ingests an export off disk, and they share
        // one `plan_render`.
        ($cfgty:ty, $rcfgty:ty, $provider:ident, $dl:ident, $rn:ident) => {{
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
                        format!("parse --params as a {source_type} download config")
                    })?;
                    // No global `defaults:` stanza in DAG mode (each step
                    // is self-contained): fold the built-in defaults only.
                    cfg.common.fold_defaults(&Defaults::default());
                    cfg.common.resolve_paths(data_root, name);
                    cfg.validate()
                        .with_context(|| format!("source {name:?} (type={source_type})"))?;
                    let raw_path = cfg.common.raw_path().to_path_buf();
                    let download_params = cfg.common.download_params.clone();
                    let always_clear_before_ingest = cfg.common.always_clear_before_ingest;
                    PlannedSource {
                        name: name.to_string(),
                        source_type,
                        raw_path,
                        download_params,
                        always_clear_before_ingest,
                        processors: $provider::processor::$dl(ctx, cfg)?,
                    }
                }
                Phase::Render => {
                    // Per-phase params split: render deserializes its own
                    // slim config (deny_unknown_fields, so download-shaped
                    // params on a render step fail loudly). No defaults to
                    // fold — render carries no cross-source knobs.
                    let mut cfg: $rcfgty = serde_json::from_value(source).with_context(|| {
                        format!("parse --params as a {source_type} render config")
                    })?;
                    cfg.common.resolve_paths(data_root, name);
                    let raw_path = cfg.common.raw_path().to_path_buf();
                    PlannedSource {
                        name: name.to_string(),
                        source_type,
                        raw_path,
                        // Rate-limit bounds are download-only machinery.
                        download_params: Default::default(),
                        always_clear_before_ingest: false,
                        processors: $provider::processor::$rn(ctx, cfg)?,
                    }
                }
            }
        }};
    }

    // Exhaustive on purpose: a `SourceType` variant with no arm here is
    // a compile error, which is the whole reason the type exists.
    Ok(match source_type {
        SourceType::ClaudeApi => arm!(
            datalib_etl_claude_config::ClaudeConfig,
            datalib_etl_claude_config::ClaudeRenderConfig,
            datalib_etl_claude
        ),
        SourceType::ClaudeExport => arm!(
            datalib_etl_claude_config::ClaudeExportConfig,
            datalib_etl_claude_config::ClaudeExportRenderConfig,
            datalib_etl_claude,
            plan_export_download,
            plan_render
        ),
        SourceType::ChatgptApi => arm!(
            datalib_etl_chatgpt_config::ChatgptConfig,
            datalib_etl_chatgpt_config::ChatgptRenderConfig,
            datalib_etl_chatgpt
        ),
        SourceType::SlackApi => arm!(
            datalib_etl_slack_config::SlackConfig,
            datalib_etl_slack_config::SlackRenderConfig,
            datalib_etl_slack
        ),
        SourceType::GithubApi => arm!(
            datalib_etl_github_config::GithubConfig,
            datalib_etl_github_config::GithubRenderConfig,
            datalib_etl_github
        ),
        SourceType::GitlabApi => arm!(
            datalib_etl_gitlab_config::GitlabConfig,
            datalib_etl_gitlab_config::GitlabRenderConfig,
            datalib_etl_gitlab
        ),
        SourceType::NotionApi => arm!(
            datalib_etl_notion_config::NotionConfig,
            datalib_etl_notion_config::NotionRenderConfig,
            datalib_etl_notion
        ),
        SourceType::Email => arm!(
            datalib_etl_email_config::EmailConfig,
            datalib_etl_email_config::EmailRenderConfig,
            datalib_etl_email
        ),
        SourceType::Beeper => arm!(
            datalib_etl_beeper_config::BeeperConfig,
            datalib_etl_beeper_config::BeeperRenderConfig,
            datalib_etl_beeper
        ),
        SourceType::Carddav => arm!(
            datalib_etl_carddav_config::CarddavConfig,
            datalib_etl_carddav_config::CarddavRenderConfig,
            datalib_etl_contacts
        ),
        SourceType::Linkedin => arm!(
            datalib_etl_linkedin_config::LinkedinConfig,
            datalib_etl_linkedin_config::LinkedinRenderConfig,
            datalib_etl_linkedin
        ),
        SourceType::GoogleTakeout => arm!(
            datalib_etl_google_takeout_config::GoogleTakeoutConfig,
            datalib_etl_google_takeout_config::GoogleTakeoutRenderConfig,
            datalib_etl_google_takeout
        ),
        SourceType::Media => arm!(
            datalib_etl_media_config::MediaConfig,
            datalib_etl_media_config::MediaRenderConfig,
            datalib_etl_media
        ),
        SourceType::Pdf => arm!(
            datalib_etl_pdf_config::PdfConfig,
            datalib_etl_pdf_config::PdfRenderConfig,
            datalib_etl_pdf
        ),
        SourceType::Perseus => arm!(
            datalib_etl_perseus_config::PerseusConfig,
            datalib_etl_perseus_config::PerseusRenderConfig,
            datalib_etl_perseus
        ),
        SourceType::Yolink => arm!(
            datalib_etl_yolink_config::YolinkConfig,
            datalib_etl_yolink_config::YolinkRenderConfig,
            datalib_etl_yolink
        ),
        SourceType::SignalBackup => arm!(
            datalib_etl_signal_config::SignalConfig,
            datalib_etl_signal_config::SignalRenderConfig,
            datalib_etl_signal
        ),
        SourceType::WhatsappBackup => arm!(
            datalib_etl_whatsapp_config::WhatsappConfig,
            datalib_etl_whatsapp_config::WhatsappRenderConfig,
            datalib_etl_whatsapp
        ),
        SourceType::SmsBackupRestore => arm!(
            datalib_etl_sms_backup_restore_config::SmsBackupRestoreConfig,
            datalib_etl_sms_backup_restore_config::SmsBackupRestoreRenderConfig,
            datalib_etl_sms_backup_restore
        ),
        SourceType::Lightroom => arm!(
            datalib_etl_lightroom_config::LightroomConfig,
            datalib_etl_lightroom_config::LightroomRenderConfig,
            datalib_etl_lightroom
        ),
        SourceType::Fsindex => arm!(
            datalib_etl_fsindex_config::FsindexConfig,
            datalib_etl_fsindex_config::FsindexRenderConfig,
            datalib_etl_fsindex
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `common.always_clear_before_ingest` has to survive the trip from the
    /// step's `--params` to the planned source, because the download driver
    /// is the only thing that reads it. A flag that parses and then goes
    /// nowhere reads exactly like one that works: the sync succeeds, and
    /// the deletions the user asked us to notice stay invisible.
    #[test]
    fn always_clear_before_ingest_reaches_the_planned_source() {
        let td = tempfile::tempdir().unwrap();
        let planned = plan(
            "sms_backup_restore",
            Phase::Download,
            "sms",
            serde_json::json!({
                "common": {
                    "input_path": "/tmp/sms",
                    "always_clear_before_ingest": true,
                }
            }),
            td.path(),
        )
        .unwrap();
        assert!(planned.always_clear_before_ingest);

        // Absent means off: every source that has never heard of the knob
        // must keep appending rather than start wiping itself.
        let default = plan(
            "sms_backup_restore",
            Phase::Download,
            "sms",
            serde_json::json!({ "common": { "input_path": "/tmp/sms" } }),
            td.path(),
        )
        .unwrap();
        assert!(!default.always_clear_before_ingest);
    }

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
        assert_eq!(dl.source_type, SourceType::SlackApi);
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

    /// An API source with no `sync:` block has nothing to fetch this
    /// run, so its download wave is empty — render still reads whatever
    /// an earlier run put in the store.
    #[test]
    fn download_without_sync_plans_empty_for_api_sources() {
        let td = tempfile::tempdir().unwrap();
        let dl = plan(
            "claude_api",
            Phase::Download,
            "claude",
            serde_json::json!({}),
            td.path(),
        )
        .unwrap();
        assert!(dl.processors.is_empty(), "no sync: means nothing to fetch");
    }

    /// `claude_export` is file-backed, not render-only: it ingests the
    /// export at `input_path` into the same raw store `claude_api`
    /// writes, so it plans a download wave like every other file-backed
    /// source (issue #207).
    #[test]
    fn claude_export_plans_an_ingest_from_its_input_path() {
        let td = tempfile::tempdir().unwrap();
        let export = td.path().join("unpacked");
        std::fs::create_dir_all(&export).unwrap();
        let dl = plan(
            "claude_export",
            Phase::Download,
            "claude-export",
            serde_json::json!({"common": {"input_path": export.to_str().unwrap()}}),
            td.path(),
        )
        .unwrap();
        assert_eq!(dl.processors.len(), 1);
        assert_eq!(dl.raw_path, td.path().join("claude-export/raw"));

        // …and its render wave reads that store, same as claude_api's.
        let rn = plan(
            "claude_export",
            Phase::Render,
            "claude-export",
            serde_json::json!({}),
            td.path(),
        )
        .unwrap();
        assert_eq!(rn.processors.len(), 1);
    }

    /// An export pointed at nothing would ingest the raw dir into
    /// itself. Refuse at plan time, which is where `datalib-dag --check`
    /// will see it.
    #[test]
    fn claude_export_without_an_input_path_is_a_config_error() {
        let td = tempfile::tempdir().unwrap();
        let err = plan(
            "claude_export",
            Phase::Download,
            "claude-export",
            serde_json::json!({}),
            td.path(),
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("input_path"), "{err}");
    }

    /// The API-shaped knobs are meaningless on an export and are
    /// rejected rather than ignored — a `sync:` block on a
    /// `claude_export` step used to silently start a live API download.
    #[test]
    fn claude_export_rejects_api_only_params() {
        let td = tempfile::tempdir().unwrap();
        let err = plan(
            "claude_export",
            Phase::Download,
            "claude-export",
            serde_json::json!({"sync": {}}),
            td.path(),
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("sync"), "{err}");
    }

    /// Download-only sources are not in the `ingested_tng` fixture
    /// pipeline (they render nothing, so there is no markdown for it to
    /// index), which means nothing else in CI exercises their dispatch
    /// arm or their `plan_*` pair. Without this, a `media` step's
    /// params could stop deserializing and every test would still pass.
    #[test]
    fn download_only_sources_plan_a_download_and_no_render() {
        let td = tempfile::tempdir().unwrap();
        for (ty, params) in [
            ("media", serde_json::json!({"playlists": false})),
            ("fsindex", serde_json::json!({})),
        ] {
            let dl = plan(
                ty,
                Phase::Download,
                "local",
                {
                    let mut v = params.clone();
                    v.as_object_mut().unwrap().insert(
                        "common".into(),
                        serde_json::json!({"input_path": td.path().to_str().unwrap()}),
                    );
                    v
                },
                td.path(),
            )
            .unwrap();
            assert_eq!(dl.source_type.as_str(), ty);
            assert_eq!(dl.raw_path, td.path().join("local/raw"));
            assert_eq!(dl.processors.len(), 1, "{ty} should plan one download");

            let rn = plan(ty, Phase::Render, "local", serde_json::json!({}), td.path()).unwrap();
            assert!(
                rn.processors.is_empty(),
                "{ty} renders nothing; download-only is structural, not a flag"
            );
        }
    }

    /// The provider config really is `deny_unknown_fields`, so a typo in
    /// a step's params fails at load rather than being ignored.
    #[test]
    fn a_misspelled_media_param_is_rejected() {
        let td = tempfile::tempdir().unwrap();
        let err = plan(
            "media",
            Phase::Download,
            "local",
            serde_json::json!({"playlist": false}),
            td.path(),
        )
        .unwrap_err();
        // `{:#}` for the whole chain: `to_string()` gives only the
        // outermost context ("parse --params as a media download
        // config"), and the field name lives in serde's error under it.
        let err = format!("{err:#}");
        assert!(err.contains("playlist"), "{err}");
        assert!(err.contains("unknown field"), "{err}");
    }

    /// Every `SourceType` must reach an arm of `plan`. The reverse —
    /// an arm for a type that does not exist — the compiler catches,
    /// since the match over the enum is exhaustive.
    #[test]
    fn every_declared_type_dispatches() {
        let td = tempfile::tempdir().unwrap();
        for &ty in <SourceType as strum::VariantArray>::VARIANTS {
            let err = plan(
                ty.as_str(),
                Phase::Download,
                "s",
                serde_json::json!({}),
                td.path(),
            )
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
            assert!(
                !err.contains("unknown source type"),
                "{ty} has no dispatch arm"
            );
        }
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
