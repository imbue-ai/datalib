//! `(source type, phase)` → provider dispatch.

use std::path::PathBuf;

use anyhow::{Context, Result};
use datalib_etl::processor::{DataProcessor, PlanContext};
use datalib_etl_render::processor::RenderProcessor;
use datalib_source_common::{Defaults, DownloadParams, Reach};

use crate::source_type::SourceType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Ingest,
    Render,
}

/// A normalized, planned source: the per-wave processors plus the
/// resolved envelope facts the step driver needs.
pub struct PlannedSource {
    pub name: String,
    pub source_type: SourceType,
    /// The raw store: the ingest step's own tree, or for a render the
    /// tree its input names.
    pub raw_path: PathBuf,
    /// Whether the method the ingest step's params hold reaches a live
    /// origin or reads files on disk. `None` for a render.
    pub reach: Option<Reach>,
    /// Resolved rate-limit give-up bounds for the ingest wave.
    pub download_params: DownloadParams,
    /// `common.always_clear_before_ingest`, resolved. Ingest wave only —
    /// render rewrites its own tree already.
    pub always_clear_before_ingest: bool,
    pub processors: Wave,
}

/// A planned source's processors, which are of a different type per
/// phase: download and render no longer share a trait, because they no
/// longer share a run context.
pub enum Wave {
    Ingest(Vec<Box<dyn DataProcessor>>),
    Render(Vec<Box<dyn RenderProcessor>>),
}

impl Wave {
    pub fn len(&self) -> usize {
        match self {
            Wave::Ingest(p) => p.len(),
            Wave::Render(p) => p.len(),
        }
    }
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

/// `raw_dir` is the raw store this plan is about: for [`Phase::Ingest`]
/// the tree the step writes, for [`Phase::Render`] the tree it reads. A
/// step writes only the tree its id names, and nothing in its params can
/// say otherwise.
pub fn plan(
    step_type: &str,
    phase: Phase,
    name: &str,
    raw_dir: PathBuf,
    source: serde_json::Value,
) -> Result<PlannedSource> {
    // Declared before `arm!`: a `macro_rules!` body only sees bindings
    // that exist at its definition site.
    let source_type = SourceType::parse(step_type).ok_or_else(|| {
        let known = SourceType::known_list();
        if SourceType::looks_retired(step_type) {
            anyhow::anyhow!(
                "{step_type:?} is a type spelled the way configs were written before a \
                 group's `type` named the thing mirrored rather than the way it is reached. \
                 Rewrite the file once: `datalib-migrate-config <data root> --force`. Known \
                 types: {known}"
            )
        } else {
            anyhow::anyhow!("unknown source type {step_type:?}; known types: {known}")
        }
    })?;
    crate::methods::refuse_retired_params(source_type, &source)?;
    // Read before `source` is handed to serde: which of the provider's
    // declared methods these params hold. Judged after `validate`, so a
    // misspelled table is reported as the unknown field it is.
    let held = crate::methods::held(&source, crate::methods::ingest_methods(source_type));

    // Each arm names two crates, because a provider is two crates: the
    // download half and the `_render` half that links `datalib_schema`.
    // A provider's two waves are its `plan_ingest` / `plan_render`
    // pair; which *method* runs is the provider's own reading of its
    // params.
    macro_rules! arm {
        ($cfgty:ty, $rcfgty:ty, $dlp:ident, $rnp:ident) => {{
            let ctx = PlanContext {
                name: name.to_string(),
                // Playback redirection goes through the
                // DATALIB_HTTP_PLAYBACK env (set by `download
                // --playback-root`), not per-plan.
                playback_root: None,
            };
            match phase {
                Phase::Ingest => {
                    let mut cfg: $cfgty = serde_json::from_value(source).with_context(|| {
                        format!("parse --params as a {source_type} download config")
                    })?;
                    // No global `defaults:` stanza in DAG mode (each step
                    // is self-contained): fold the built-in defaults only.
                    cfg.common.fold_defaults(&Defaults::default());
                    cfg.common.resolve_paths(raw_dir.clone());
                    cfg.validate()
                        .with_context(|| format!("source {name:?} (type={source_type})"))?;
                    let raw_path = cfg.common.raw_path().to_path_buf();
                    let reach = crate::methods::reach_or_refuse(source_type, &held)?;
                    let download_params = cfg.common.download_params.clone();
                    let always_clear_before_ingest = cfg.common.always_clear_before_ingest;
                    PlannedSource {
                        name: name.to_string(),
                        source_type,
                        raw_path,
                        reach: Some(reach),
                        download_params,
                        always_clear_before_ingest,
                        processors: Wave::Ingest($dlp::processor::plan_ingest(ctx, cfg)?),
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
                    cfg.common.resolve_paths(raw_dir.clone());
                    let raw_path = cfg.common.raw_path().to_path_buf();
                    PlannedSource {
                        name: name.to_string(),
                        source_type,
                        raw_path,
                        reach: None,
                        // Rate-limit bounds are download-only machinery.
                        download_params: Default::default(),
                        always_clear_before_ingest: false,
                        processors: Wave::Render($rnp::processor::plan_render(ctx, cfg)?),
                    }
                }
            }
        }};
    }

    // A source that renders nothing: it plans an ingest wave and an
    // empty render one. The render config is still parsed, so a typo in
    // a render step's params is still rejected rather than ignored.
    macro_rules! ingest_only {
        ($cfgty:ty, $rcfgty:ty, $dlp:ident) => {{
            let ctx = PlanContext {
                name: name.to_string(),
                playback_root: None,
            };
            match phase {
                Phase::Ingest => {
                    let mut cfg: $cfgty = serde_json::from_value(source).with_context(|| {
                        format!("parse --params as a {source_type} download config")
                    })?;
                    cfg.common.fold_defaults(&Defaults::default());
                    cfg.common.resolve_paths(raw_dir.clone());
                    cfg.validate()
                        .with_context(|| format!("source {name:?} (type={source_type})"))?;
                    PlannedSource {
                        name: name.to_string(),
                        source_type,
                        raw_path: cfg.common.raw_path().to_path_buf(),
                        reach: Some(crate::methods::reach_or_refuse(source_type, &held)?),
                        download_params: cfg.common.download_params.clone(),
                        always_clear_before_ingest: cfg.common.always_clear_before_ingest,
                        processors: Wave::Ingest($dlp::processor::plan_ingest(ctx, cfg)?),
                    }
                }
                Phase::Render => {
                    let mut cfg: $rcfgty = serde_json::from_value(source).with_context(|| {
                        format!("parse --params as a {source_type} render config")
                    })?;
                    cfg.common.resolve_paths(raw_dir.clone());
                    PlannedSource {
                        name: name.to_string(),
                        source_type,
                        raw_path: cfg.common.raw_path().to_path_buf(),
                        reach: None,
                        download_params: Default::default(),
                        always_clear_before_ingest: false,
                        processors: Wave::Render(Vec::new()),
                    }
                }
            }
        }};
    }

    // Exhaustive on purpose: a `SourceType` variant with no arm here is
    // a compile error, which is the whole reason the type exists.
    Ok(match source_type {
        SourceType::Claude => arm!(
            datalib_etl_claude_config::ClaudeConfig,
            datalib_etl_claude_config::ClaudeRenderConfig,
            datalib_etl_claude,
            datalib_etl_claude_render
        ),
        SourceType::Chatgpt => arm!(
            datalib_etl_chatgpt_config::ChatgptConfig,
            datalib_etl_chatgpt_config::ChatgptRenderConfig,
            datalib_etl_chatgpt,
            datalib_etl_chatgpt_render
        ),
        SourceType::Slack => arm!(
            datalib_etl_slack_config::SlackConfig,
            datalib_etl_slack_config::SlackRenderConfig,
            datalib_etl_slack,
            datalib_etl_slack_render
        ),
        SourceType::Github => arm!(
            datalib_etl_github_config::GithubConfig,
            datalib_etl_github_config::GithubRenderConfig,
            datalib_etl_github,
            datalib_etl_github_render
        ),
        SourceType::Gitlab => arm!(
            datalib_etl_gitlab_config::GitlabConfig,
            datalib_etl_gitlab_config::GitlabRenderConfig,
            datalib_etl_gitlab,
            datalib_etl_gitlab_render
        ),
        SourceType::Notion => arm!(
            datalib_etl_notion_config::NotionConfig,
            datalib_etl_notion_config::NotionRenderConfig,
            datalib_etl_notion,
            datalib_etl_notion_render
        ),
        SourceType::Email => arm!(
            datalib_etl_email_config::EmailConfig,
            datalib_etl_email_config::EmailRenderConfig,
            datalib_etl_email,
            datalib_etl_email_render
        ),
        SourceType::Beeper => arm!(
            datalib_etl_beeper_config::BeeperConfig,
            datalib_etl_beeper_config::BeeperRenderConfig,
            datalib_etl_beeper,
            datalib_etl_beeper_render
        ),
        SourceType::Contacts => arm!(
            datalib_etl_contacts_config::ContactsConfig,
            datalib_etl_contacts_config::ContactsRenderConfig,
            datalib_etl_contacts,
            datalib_etl_contacts_render
        ),
        SourceType::Linkedin => arm!(
            datalib_etl_linkedin_config::LinkedinConfig,
            datalib_etl_linkedin_config::LinkedinRenderConfig,
            datalib_etl_linkedin,
            datalib_etl_linkedin_render
        ),
        SourceType::GoogleTakeout => arm!(
            datalib_etl_google_takeout_config::GoogleTakeoutConfig,
            datalib_etl_google_takeout_config::GoogleTakeoutRenderConfig,
            datalib_etl_google_takeout,
            datalib_etl_google_takeout_render
        ),
        SourceType::Media => ingest_only!(
            datalib_etl_media_config::MediaConfig,
            datalib_etl_media_config::MediaRenderConfig,
            datalib_etl_media
        ),
        SourceType::Pdf => arm!(
            datalib_etl_pdf_config::PdfConfig,
            datalib_etl_pdf_config::PdfRenderConfig,
            datalib_etl_pdf,
            datalib_etl_pdf_render
        ),
        SourceType::Perseus => arm!(
            datalib_etl_perseus_config::PerseusConfig,
            datalib_etl_perseus_config::PerseusRenderConfig,
            datalib_etl_perseus,
            datalib_etl_perseus_render
        ),
        SourceType::Yolink => arm!(
            datalib_etl_yolink_config::YolinkConfig,
            datalib_etl_yolink_config::YolinkRenderConfig,
            datalib_etl_yolink,
            datalib_etl_yolink_render
        ),
        SourceType::Signal => arm!(
            datalib_etl_signal_config::SignalConfig,
            datalib_etl_signal_config::SignalRenderConfig,
            datalib_etl_signal,
            datalib_etl_signal_render
        ),
        SourceType::Whatsapp => arm!(
            datalib_etl_whatsapp_config::WhatsappConfig,
            datalib_etl_whatsapp_config::WhatsappRenderConfig,
            datalib_etl_whatsapp,
            datalib_etl_whatsapp_render
        ),
        SourceType::SmsBackupRestore => arm!(
            datalib_etl_sms_backup_restore_config::SmsBackupRestoreConfig,
            datalib_etl_sms_backup_restore_config::SmsBackupRestoreRenderConfig,
            datalib_etl_sms_backup_restore,
            datalib_etl_sms_backup_restore_render
        ),
        SourceType::Lightroom => ingest_only!(
            datalib_etl_lightroom_config::LightroomConfig,
            datalib_etl_lightroom_config::LightroomRenderConfig,
            datalib_etl_lightroom
        ),
        SourceType::ApplePhotos => ingest_only!(
            datalib_etl_apple_photos_config::ApplePhotosConfig,
            datalib_etl_apple_photos_config::ApplePhotosRenderConfig,
            datalib_etl_apple_photos
        ),
        SourceType::Fsindex => ingest_only!(
            datalib_etl_fsindex_config::FsindexConfig,
            datalib_etl_fsindex_config::FsindexRenderConfig,
            datalib_etl_fsindex
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The tree a step of this phase has for its raw store: its own
    /// `<name>/ingest` when ingesting, the same tree as its input when
    /// rendering.
    fn raw_dir(root: &Path, name: &str, _phase: Phase) -> PathBuf {
        datalib_etl::layout::ingest_root(root, name)
    }

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
            Phase::Ingest,
            "sms",
            raw_dir(td.path(), "sms", Phase::Ingest),
            serde_json::json!({
                "backup": {"path": "/tmp/sms"},
                "common": {"always_clear_before_ingest": true}
            }),
        )
        .unwrap();
        assert!(planned.always_clear_before_ingest);

        // Absent means off: every source that has never heard of the knob
        // must keep appending rather than start wiping itself.
        let default = plan(
            "sms_backup_restore",
            Phase::Ingest,
            "sms",
            raw_dir(td.path(), "sms", Phase::Ingest),
            serde_json::json!({ "backup": {"path": "/tmp/sms"} }),
        )
        .unwrap();
        assert!(!default.always_clear_before_ingest);
    }

    #[test]
    fn plans_slack_download_and_render_from_phase_params() {
        let td = tempfile::tempdir().unwrap();
        let source: serde_json::Value = serde_json::json!({
            "api": {"media": true, "channels": ["chat-qi"], "since": "2026-06-15"}
        });
        let dl = plan(
            "slack",
            Phase::Ingest,
            "slack",
            raw_dir(td.path(), "slack", Phase::Ingest),
            source.clone(),
        )
        .unwrap();
        assert_eq!(dl.source_type, SourceType::Slack);
        assert_eq!(dl.raw_path, td.path().join("slack/ingest"));
        assert_eq!(dl.processors.len(), 1);

        // Render params are phase-specific: slack render needs none.
        let rn = plan(
            "slack",
            Phase::Render,
            "slack",
            raw_dir(td.path(), "slack", Phase::Render),
            serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(rn.processors.len(), 1);

        // Download-shaped params on a render step fail loudly instead
        // of being silently ignored.
        let err = plan(
            "slack",
            Phase::Render,
            "slack",
            raw_dir(td.path(), "slack", Phase::Render),
            source,
        )
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
            serde_json::json!({"api": {"media": true, "channels": ["chat-qi"]}}),
        )
        .expect("a pre-dms config must still parse");
        let api = cfg.api.expect("api");
        assert!(!api.dms, "an upgrade must not start mirroring DMs");
        assert!(api.dm_conversations.is_none());
    }

    /// The one combination the provider refuses, refused where the
    /// step actually reads its params — `plan` is what calls
    /// `validate`, and a rule that isn't wired into it is not enforced.
    #[test]
    fn slack_dm_conversations_without_dms_fails_at_plan_time() {
        let td = tempfile::tempdir().unwrap();
        let err = plan(
            "slack",
            Phase::Ingest,
            "slack",
            raw_dir(td.path(), "slack", Phase::Ingest),
            serde_json::json!({"api": {"dm_conversations": ["D0123ABCD"]}}),
        )
        .unwrap_err();
        // `{:#}` walks the cause chain, which is what `main.rs` prints
        // (one line per `e.chain()` entry) — the bare `to_string()` is
        // only the outermost "source ... (type=slack)" context.
        let err = format!("{err:#}");
        assert!(err.contains("dm_conversations"), "{err}");
        assert!(err.contains("dms = true"), "{err}");

        // …and is accepted with the switch on.
        plan(
            "slack",
            Phase::Ingest,
            "slack",
            raw_dir(td.path(), "slack", Phase::Ingest),
            serde_json::json!({"api": {"dms": true, "dm_conversations": ["D0123ABCD"]}}),
        )
        .expect("dms = true with an allowlist is the supported shape");
    }

    #[test]
    fn render_knobs_are_rejected_on_download_and_read_on_render() {
        let td = tempfile::tempdir().unwrap();
        // `period` used to live in the ingest table; the download
        // planner points at its new home on the render step.
        let err = plan(
            "beeper",
            Phase::Ingest,
            "beeper",
            raw_dir(td.path(), "beeper", Phase::Ingest),
            serde_json::json!({"texts": {"sources": ["signal"], "period": "day"}}),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("render step's params"), "{err}");

        let rn = plan(
            "beeper",
            Phase::Render,
            "beeper",
            raw_dir(td.path(), "beeper", Phase::Render),
            serde_json::json!({"period": "day"}),
        )
        .unwrap();
        assert_eq!(rn.processors.len(), 1);
    }

    /// An `ingest` step whose params hold none of its provider's methods
    /// is refused at plan time, where `datalib-dag --check` sees it. A
    /// step with no method used to plan an empty ingest wave and
    /// succeed, which read exactly like a working sync that found nothing.
    #[test]
    fn an_ingest_step_with_no_method_is_refused() {
        let td = tempfile::tempdir().unwrap();
        let err = plan(
            "claude",
            Phase::Ingest,
            "claude",
            raw_dir(td.path(), "claude", Phase::Ingest),
            serde_json::json!({}),
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("`api`"), "{err}");
        assert!(err.contains("`export`"), "{err}");
        assert!(err.contains("where the claude data comes from"), "{err}");

        // A misspelled table is reported as what it is, not as a missing
        // method: serde's unknown-field error comes first (where the
        // provider's config is `deny_unknown_fields`, as pdf's is).
        let err = plan(
            "pdf",
            Phase::Ingest,
            "pdfs",
            raw_dir(td.path(), "pdfs", Phase::Ingest),
            serde_json::json!({"fswlk": {"path": "/scans"}}),
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("unknown field"), "{err}");
        assert!(!err.contains("where the pdf data comes from"), "{err}");
    }

    /// A config in the shape written before method tables — `sync`, or
    /// `common.input_path` — is refused whole, naming the migrator, on
    /// every type. The old type spellings are refused the same way.
    #[test]
    fn the_retired_shape_is_refused_and_names_the_migrator() {
        let td = tempfile::tempdir().unwrap();
        for (ty, params) in [
            ("slack", serde_json::json!({"sync": {"channels": ["x"]}})),
            (
                "pdf",
                serde_json::json!({"common": {"input_path": "/scans"}}),
            ),
            (
                "email",
                serde_json::json!({"common": {"input_path": "/m.mbox"}, "mbox": {}}),
            ),
        ] {
            let err = plan(
                ty,
                Phase::Ingest,
                "s",
                raw_dir(td.path(), "s", Phase::Ingest),
                params,
            )
            .unwrap_err();
            let err = format!("{err:#}");
            assert!(err.contains("datalib-migrate-config"), "{ty}: {err}");
        }
        for old in ["slack_api", "claude_export", "signal_backup", "carddav"] {
            let err = plan(
                old,
                Phase::Ingest,
                "s",
                raw_dir(td.path(), "s", Phase::Ingest),
                serde_json::json!({}),
            )
            .unwrap_err();
            let err = format!("{err:#}");
            assert!(err.contains("datalib-migrate-config"), "{old}: {err}");
        }
    }

    /// The planned source carries what its method reaches, read off the
    /// params by the provider's declaration.
    #[test]
    fn a_planned_ingest_knows_its_reach() {
        let td = tempfile::tempdir().unwrap();
        let dl = plan(
            "slack",
            Phase::Ingest,
            "slack",
            raw_dir(td.path(), "slack", Phase::Ingest),
            serde_json::json!({"api": {}}),
        )
        .unwrap();
        assert_eq!(dl.reach, Some(Reach::Origin));
        let dl = plan(
            "pdf",
            Phase::Ingest,
            "pdfs",
            raw_dir(td.path(), "pdfs", Phase::Ingest),
            serde_json::json!({"fswalk": {"path": td.path().to_str().unwrap()}}),
        )
        .unwrap();
        assert_eq!(dl.reach, Some(Reach::Local));
        let rn = plan(
            "pdf",
            Phase::Render,
            "pdfs",
            raw_dir(td.path(), "pdfs", Phase::Render),
            serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(rn.reach, None);
    }

    /// One `claude` type, two methods: `export` is file-backed, not
    /// render-only. It ingests the export at its `path` into the same
    /// raw store `api` writes, so it plans a ingest wave like every
    /// other file-backed source (issue #207).
    #[test]
    fn claude_export_plans_an_ingest_from_its_path() {
        let td = tempfile::tempdir().unwrap();
        let export = td.path().join("unpacked");
        std::fs::create_dir_all(&export).unwrap();
        let dl = plan(
            "claude",
            Phase::Ingest,
            "claude-export",
            raw_dir(td.path(), "claude-export", Phase::Ingest),
            serde_json::json!({"export": {"path": export.to_str().unwrap()}}),
        )
        .unwrap();
        assert_eq!(dl.processors.len(), 1);
        assert_eq!(dl.reach, Some(Reach::Local));
        assert_eq!(dl.raw_path, td.path().join("claude-export/ingest"));

        // …and its render wave reads that store, same as the API's.
        let rn = plan(
            "claude",
            Phase::Render,
            "claude-export",
            raw_dir(td.path(), "claude-export", Phase::Render),
            serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(rn.processors.len(), 1);

        // Both at once is refused at plan time, where `--check` sees it.
        let err = plan(
            "claude",
            Phase::Ingest,
            "claude",
            raw_dir(td.path(), "claude", Phase::Ingest),
            serde_json::json!({"api": {}, "export": {"path": export.to_str().unwrap()}}),
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("both"), "{err}");
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
                Phase::Ingest,
                "local",
                raw_dir(td.path(), "local", Phase::Ingest),
                {
                    let mut v = params.clone();
                    v.as_object_mut().unwrap().insert(
                        "fswalk".into(),
                        serde_json::json!({"path": td.path().to_str().unwrap()}),
                    );
                    v
                },
            )
            .unwrap();
            assert_eq!(dl.source_type.as_str(), ty);
            assert_eq!(dl.raw_path, td.path().join("local/ingest"));
            assert_eq!(dl.processors.len(), 1, "{ty} should plan one download");

            let rn = plan(
                ty,
                Phase::Render,
                "local",
                raw_dir(td.path(), "local", Phase::Render),
                serde_json::json!({}),
            )
            .unwrap();
            assert_eq!(
                rn.processors.len(),
                0,
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
            Phase::Ingest,
            "local",
            raw_dir(td.path(), "local", Phase::Ingest),
            serde_json::json!({"playlist": false}),
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
                Phase::Ingest,
                "s",
                raw_dir(td.path(), "s", Phase::Ingest),
                serde_json::json!({}),
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
            Phase::Ingest,
            "x",
            raw_dir(td.path(), "x", Phase::Ingest),
            serde_json::json!({}),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("slack"), "{err}");
    }

    /// `common.raw_path` used to name the store, and was then refused
    /// unless it named the step's own tree. It is not a key any more:
    /// the runner versions, and every consumer reads, the tree the id
    /// names, so a store written elsewhere would be one nothing
    /// downstream ever sees. A config still carrying it is refused by
    /// name, with the migrator named.
    #[test]
    fn an_ingest_step_refuses_a_raw_path_key() {
        let td = tempfile::tempdir().unwrap();
        let err = plan(
            "github",
            Phase::Ingest,
            "gh",
            raw_dir(td.path(), "gh", Phase::Ingest),
            serde_json::json!({"common": {"raw_path": "/mnt/big/gh-raw"}, "api": {}}),
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("raw_path"), "{err}");
        assert!(err.contains("datalib-migrate-config"), "{err}");
        // A render step's `common` is the slim envelope, and refuses it too.
        let err = plan(
            "github",
            Phase::Render,
            "gh",
            raw_dir(td.path(), "gh", Phase::Render),
            serde_json::json!({"common": {"raw_path": "/mnt/big/gh-raw"}}),
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("raw_path"), "{err}");
    }
}
