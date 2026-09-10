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
    /// Resolved rate-limit give-up bounds for the download wave.
    pub download_params: DownloadParams,
    /// `common.always_clear_before_ingest`, resolved. Download wave only —
    /// render rewrites its own tree already.
    pub always_clear_before_ingest: bool,
    pub processors: Wave,
}

/// A planned source's processors, which are of a different type per
/// phase: download and render no longer share a trait, because they no
/// longer share a run context.
pub enum Wave {
    Download(Vec<Box<dyn DataProcessor>>),
    Render(Vec<Box<dyn RenderProcessor>>),
}

impl Wave {
    pub fn len(&self) -> usize {
        match self {
            Wave::Download(p) => p.len(),
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

/// The ingest step's raw store is its tree and nothing else. An explicit
/// `common.raw_path` is accepted only when it names that same tree; a
/// store kept elsewhere is a symlink at the tree, not a config knob,
/// because the runner versions and consumers read the tree by its id.
fn ingest_writes_its_own_tree(
    tree: &std::path::Path,
    resolved: &std::path::Path,
) -> Result<PathBuf> {
    anyhow::ensure!(
        resolved == tree,
        "`common.raw_path` is {resolved:?}, but this step writes only the tree its id \
         names, {tree:?}. Drop `raw_path`; to keep the store on another disk, put a \
         symlink at {tree:?}.",
        resolved = resolved.display(),
        tree = tree.display(),
    );
    Ok(tree.to_path_buf())
}

/// `raw_dir` is the raw store this plan is about: for [`Phase::Ingest`]
/// the tree the step writes, for [`Phase::Render`] the tree it reads. An
/// ingest whose params point `common.raw_path` anywhere else is refused —
/// a step writes only the tree its id names.
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
        anyhow::anyhow!(
            "unknown source type {step_type:?}; known types: {}",
            SourceType::known_list()
        )
    })?;
    // Read before `source` is handed to serde: which of the provider's
    // declared methods these params hold. Judged after `validate`, so a
    // misspelled table is reported as the unknown field it is.
    let held = crate::methods::held(&source, crate::methods::ingest_methods(source_type));

    // Each arm names two crates, because a provider is two crates: the
    // download half and the `_render` half that links `datalib_schema`.
    macro_rules! arm {
        // The usual shape: a provider's two waves are its
        // `plan_download` / `plan_render` pair.
        ($cfgty:ty, $rcfgty:ty, $dlp:ident, $rnp:ident) => {
            arm!($cfgty, $rcfgty, $dlp, $rnp, plan_download, plan_render)
        };
        // …and the shape for a provider serving more than one source
        // type, which needs a different entry point per type. Only
        // claude does: `claude_api` walks the live API,
        // `claude_export` ingests an export off disk, and they share
        // one `plan_render`.
        ($cfgty:ty, $rcfgty:ty, $dlp:ident, $rnp:ident, $dl:ident, $rn:ident) => {{
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
                    let raw_path = ingest_writes_its_own_tree(&raw_dir, cfg.common.raw_path())?;
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
                        processors: Wave::Download($dlp::processor::$dl(ctx, cfg)?),
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
                        processors: Wave::Render($rnp::processor::$rn(ctx, cfg)?),
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
                        raw_path: ingest_writes_its_own_tree(&raw_dir, cfg.common.raw_path())?,
                        reach: Some(crate::methods::reach_or_refuse(source_type, &held)?),
                        download_params: cfg.common.download_params.clone(),
                        always_clear_before_ingest: cfg.common.always_clear_before_ingest,
                        processors: Wave::Download($dlp::processor::plan_download(ctx, cfg)?),
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
        SourceType::ClaudeApi => arm!(
            datalib_etl_claude_config::ClaudeConfig,
            datalib_etl_claude_config::ClaudeRenderConfig,
            datalib_etl_claude,
            datalib_etl_claude_render
        ),
        SourceType::ClaudeExport => arm!(
            datalib_etl_claude_config::ClaudeExportConfig,
            datalib_etl_claude_config::ClaudeExportRenderConfig,
            datalib_etl_claude,
            datalib_etl_claude_render,
            plan_export_download,
            plan_render
        ),
        SourceType::ChatgptApi => arm!(
            datalib_etl_chatgpt_config::ChatgptConfig,
            datalib_etl_chatgpt_config::ChatgptRenderConfig,
            datalib_etl_chatgpt,
            datalib_etl_chatgpt_render
        ),
        SourceType::SlackApi => arm!(
            datalib_etl_slack_config::SlackConfig,
            datalib_etl_slack_config::SlackRenderConfig,
            datalib_etl_slack,
            datalib_etl_slack_render
        ),
        SourceType::GithubApi => arm!(
            datalib_etl_github_config::GithubConfig,
            datalib_etl_github_config::GithubRenderConfig,
            datalib_etl_github,
            datalib_etl_github_render
        ),
        SourceType::GitlabApi => arm!(
            datalib_etl_gitlab_config::GitlabConfig,
            datalib_etl_gitlab_config::GitlabRenderConfig,
            datalib_etl_gitlab,
            datalib_etl_gitlab_render
        ),
        SourceType::NotionApi => arm!(
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
        SourceType::Carddav => arm!(
            datalib_etl_carddav_config::CarddavConfig,
            datalib_etl_carddav_config::CarddavRenderConfig,
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
        SourceType::SignalBackup => arm!(
            datalib_etl_signal_config::SignalConfig,
            datalib_etl_signal_config::SignalRenderConfig,
            datalib_etl_signal,
            datalib_etl_signal_render
        ),
        SourceType::WhatsappBackup => arm!(
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
                "common": {
                    "input_path": "/tmp/sms",
                    "always_clear_before_ingest": true,
                }
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
            serde_json::json!({ "common": { "input_path": "/tmp/sms" } }),
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
            Phase::Ingest,
            "slack",
            raw_dir(td.path(), "slack", Phase::Ingest),
            source.clone(),
        )
        .unwrap();
        assert_eq!(dl.source_type, SourceType::SlackApi);
        assert_eq!(dl.raw_path, td.path().join("slack/ingest"));
        assert_eq!(dl.processors.len(), 1);

        // Render params are phase-specific: slack render needs none.
        let rn = plan(
            "slack_api",
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
            "slack_api",
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
            Phase::Ingest,
            "slack",
            raw_dir(td.path(), "slack", Phase::Ingest),
            serde_json::json!({"sync": {"dm_users": ["@riker"]}}),
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
            Phase::Ingest,
            "slack",
            raw_dir(td.path(), "slack", Phase::Ingest),
            serde_json::json!({"sync": {"dms": true, "dm_users": ["@riker"]}}),
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
            Phase::Ingest,
            "beeper",
            raw_dir(td.path(), "beeper", Phase::Ingest),
            serde_json::json!({"sync": {"sources": ["signal"], "period": "day"}}),
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
    /// step with no `sync:` used to plan an empty download wave and
    /// succeed, which read exactly like a working sync that found nothing.
    #[test]
    fn an_ingest_step_with_no_method_is_refused() {
        let td = tempfile::tempdir().unwrap();
        let err = plan(
            "claude_api",
            Phase::Ingest,
            "claude",
            raw_dir(td.path(), "claude", Phase::Ingest),
            serde_json::json!({}),
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("`sync`"), "{err}");
        assert!(
            err.contains("where the claude_api data comes from"),
            "{err}"
        );

        // A misspelled table is reported as what it is, not as a missing
        // method: serde's unknown-field error comes first (where the
        // provider's config is `deny_unknown_fields`, as pdf's is).
        let err = plan(
            "pdf",
            Phase::Ingest,
            "pdfs",
            raw_dir(td.path(), "pdfs", Phase::Ingest),
            serde_json::json!({"comon": {"input_path": "/scans"}}),
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("unknown field"), "{err}");
        assert!(!err.contains("where the pdf data comes from"), "{err}");
    }

    /// The planned source carries what its method reaches, read off the
    /// params by the provider's declaration.
    #[test]
    fn a_planned_ingest_knows_its_reach() {
        let td = tempfile::tempdir().unwrap();
        let dl = plan(
            "slack_api",
            Phase::Ingest,
            "slack",
            raw_dir(td.path(), "slack", Phase::Ingest),
            serde_json::json!({"sync": {}}),
        )
        .unwrap();
        assert_eq!(dl.reach, Some(Reach::Origin));
        let dl = plan(
            "pdf",
            Phase::Ingest,
            "pdfs",
            raw_dir(td.path(), "pdfs", Phase::Ingest),
            serde_json::json!({"common": {"input_path": td.path().to_str().unwrap()}}),
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
            Phase::Ingest,
            "claude-export",
            raw_dir(td.path(), "claude-export", Phase::Ingest),
            serde_json::json!({"common": {"input_path": export.to_str().unwrap()}}),
        )
        .unwrap();
        assert_eq!(dl.processors.len(), 1);
        assert_eq!(dl.raw_path, td.path().join("claude-export/ingest"));

        // …and its render wave reads that store, same as claude_api's.
        let rn = plan(
            "claude_export",
            Phase::Render,
            "claude-export",
            raw_dir(td.path(), "claude-export", Phase::Render),
            serde_json::json!({}),
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
            Phase::Ingest,
            "claude-export",
            raw_dir(td.path(), "claude-export", Phase::Ingest),
            serde_json::json!({}),
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
            Phase::Ingest,
            "claude-export",
            raw_dir(td.path(), "claude-export", Phase::Ingest),
            serde_json::json!({"sync": {}}),
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
                Phase::Ingest,
                "local",
                raw_dir(td.path(), "local", Phase::Ingest),
                {
                    let mut v = params.clone();
                    v.as_object_mut().unwrap().insert(
                        "common".into(),
                        serde_json::json!({"input_path": td.path().to_str().unwrap()}),
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
        assert!(err.contains("slack_api"), "{err}");
    }

    /// A `common.raw_path` pointing anywhere but the step's own tree is
    /// refused: the runner versions, and every consumer reads, the tree
    /// the id names, so a store written elsewhere would be one nothing
    /// downstream ever sees.
    #[test]
    fn an_ingest_step_refuses_a_raw_path_outside_its_tree() {
        let td = tempfile::tempdir().unwrap();
        let err = plan(
            "github_api",
            Phase::Ingest,
            "gh",
            raw_dir(td.path(), "gh", Phase::Ingest),
            serde_json::json!({"common": {"raw_path": "/mnt/big/gh-raw"}, "sync": {}}),
        )
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("raw_path"), "{err}");
        assert!(err.contains("gh/ingest"), "{err}");

        // Naming the tree itself is redundant, and harmless.
        let same = plan(
            "github_api",
            Phase::Ingest,
            "gh",
            raw_dir(td.path(), "gh", Phase::Ingest),
            serde_json::json!({
                "common": {"raw_path": td.path().join("gh/ingest").to_str().unwrap()},
                "sync": {}
            }),
        )
        .unwrap();
        assert_eq!(same.raw_path, td.path().join("gh/ingest"));
    }
}
