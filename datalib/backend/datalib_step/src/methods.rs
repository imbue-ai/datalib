//! Which ingest method a step's params hold, and what it reaches.
//!
//! The declarations are the provider config crates' (`IngestMethods` in
//! `datalib_source_common`); this module maps a source type to its list
//! and applies the one rule the UI applies too
//! (`datalib/ui/src/config/ingestMethods.ts`): a method is held when its
//! path is written and its value is neither `null` nor `false`.

use anyhow::Result;
use datalib_source_common::{IngestMethod, IngestMethods, Reach};

use crate::source_type::SourceType;

pub fn ingest_methods(source_type: SourceType) -> &'static [IngestMethod] {
    match source_type {
        SourceType::Beeper => datalib_etl_beeper_config::BeeperConfig::METHODS,
        SourceType::Carddav => datalib_etl_carddav_config::CarddavConfig::METHODS,
        SourceType::ChatgptApi => datalib_etl_chatgpt_config::ChatgptConfig::METHODS,
        SourceType::ClaudeApi => datalib_etl_claude_config::ClaudeConfig::METHODS,
        SourceType::ClaudeExport => datalib_etl_claude_config::ClaudeExportConfig::METHODS,
        SourceType::Email => datalib_etl_email_config::EmailConfig::METHODS,
        SourceType::Fsindex => datalib_etl_fsindex_config::FsindexConfig::METHODS,
        SourceType::GithubApi => datalib_etl_github_config::GithubConfig::METHODS,
        SourceType::GitlabApi => datalib_etl_gitlab_config::GitlabConfig::METHODS,
        SourceType::GoogleTakeout => {
            datalib_etl_google_takeout_config::GoogleTakeoutConfig::METHODS
        }
        SourceType::Lightroom => datalib_etl_lightroom_config::LightroomConfig::METHODS,
        SourceType::Linkedin => datalib_etl_linkedin_config::LinkedinConfig::METHODS,
        SourceType::Media => datalib_etl_media_config::MediaConfig::METHODS,
        SourceType::NotionApi => datalib_etl_notion_config::NotionConfig::METHODS,
        SourceType::Pdf => datalib_etl_pdf_config::PdfConfig::METHODS,
        SourceType::Perseus => datalib_etl_perseus_config::PerseusConfig::METHODS,
        SourceType::SignalBackup => datalib_etl_signal_config::SignalConfig::METHODS,
        SourceType::SlackApi => datalib_etl_slack_config::SlackConfig::METHODS,
        SourceType::SmsBackupRestore => {
            datalib_etl_sms_backup_restore_config::SmsBackupRestoreConfig::METHODS
        }
        SourceType::WhatsappBackup => datalib_etl_whatsapp_config::WhatsappConfig::METHODS,
        SourceType::Yolink => datalib_etl_yolink_config::YolinkConfig::METHODS,
    }
}

pub fn held<'a>(params: &serde_json::Value, methods: &'a [IngestMethod]) -> Vec<&'a IngestMethod> {
    methods
        .iter()
        .filter(|m| is_held(params.pointer(&json_pointer(m.path))))
        .collect()
}

fn is_held(value: Option<&serde_json::Value>) -> bool {
    !matches!(
        value,
        None | Some(serde_json::Value::Null) | Some(serde_json::Value::Bool(false))
    )
}

fn json_pointer(dotted: &str) -> String {
    format!("/{}", dotted.replace('.', "/"))
}

/// Origin if anything held reaches one, else Local, else None.
pub fn reach_of(held: &[&IngestMethod]) -> Option<Reach> {
    if held.iter().any(|m| m.reach == Reach::Origin) {
        Some(Reach::Origin)
    } else if held.is_empty() {
        None
    } else {
        Some(Reach::Local)
    }
}

pub fn reach_or_refuse(source_type: SourceType, held: &[&IngestMethod]) -> Result<Reach> {
    reach_of(held).ok_or_else(|| {
        let known = ingest_methods(source_type)
            .iter()
            .map(|m| {
                let does = match m.reach {
                    Reach::Origin => "downloads from the origin",
                    Reach::Local => "reads files on disk",
                };
                format!("`{}` ({does})", m.path)
            })
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::anyhow!(
            "this `ingest` step's params do not say where the {source_type} data comes from: \
             none of {known} is set. An ingest step needs one of them; a store that is \
             already filled and should not be ingested into again keeps only its render \
             step, which reads the tree as it is."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use strum::VariantArray;

    #[test]
    fn a_table_is_held_by_presence_and_a_flag_only_when_on() {
        let slack = ingest_methods(SourceType::SlackApi);
        assert_eq!(
            reach_of(&held(&json!({"sync": {}}), slack)),
            Some(Reach::Origin)
        );
        assert_eq!(reach_of(&held(&json!({}), slack)), None);
        // A table the provider never declared is not a method, however
        // it is spelled.
        assert_eq!(
            reach_of(&held(&json!({"common": {"input_path": "/x"}}), slack)),
            None
        );

        let linkedin = ingest_methods(SourceType::Linkedin);
        let export = json!({"common": {"input_path": "/export"}});
        assert_eq!(reach_of(&held(&export, linkedin)), Some(Reach::Local));
        let mut with_photos = export.clone();
        with_photos["fetch_photos"] = json!(true);
        assert_eq!(reach_of(&held(&with_photos, linkedin)), Some(Reach::Origin));
        let mut photos_off = export;
        photos_off["fetch_photos"] = json!(false);
        assert_eq!(reach_of(&held(&photos_off, linkedin)), Some(Reach::Local));
    }

    /// Email is the one with three ways in; `common.input_path` on its
    /// own is the mbox case and reads Local even with the account-label
    /// table beside it.
    #[test]
    fn email_reads_origin_for_a_server_and_local_for_an_mbox() {
        let email = ingest_methods(SourceType::Email);
        assert_eq!(
            reach_of(&held(&json!({"gmail_api": {"user_id": "me"}}), email)),
            Some(Reach::Origin)
        );
        assert_eq!(
            reach_of(&held(
                &json!({"common": {"input_path": "/mail.mbox"}, "mbox": {}}),
                email
            )),
            Some(Reach::Local)
        );
    }

    #[test]
    fn the_refusal_names_every_method_the_provider_takes() {
        let err = reach_or_refuse(SourceType::Email, &[])
            .unwrap_err()
            .to_string();
        for path in ["`sync`", "`gmail_api`", "`mbox`", "`common.input_path`"] {
            assert!(err.contains(path), "{err}");
        }
        assert!(err.contains("reads files on disk"), "{err}");
        assert!(err.contains("downloads from the origin"), "{err}");
    }

    /// Every type declares at least one method, or its ingest step could
    /// never be written.
    #[test]
    fn every_type_declares_a_method() {
        for &t in SourceType::VARIANTS {
            assert!(
                !ingest_methods(t).is_empty(),
                "{t} declares no ingest method"
            );
        }
    }

    /// The UI reads the declarations through a checked-in JSON generated
    /// from them. Regenerate with
    /// `bazel run //datalib/backend/datalib_step:ingest_methods.update`.
    #[test]
    fn ui_mirror_matches_the_declarations() {
        const REL: &str = "datalib/ui/src/config/ingestMethods.json";
        let mut by_type: BTreeMap<&str, &[IngestMethod]> = BTreeMap::new();
        for &t in SourceType::VARIANTS {
            by_type.insert(t.as_str(), ingest_methods(t));
        }
        let fresh = format!("{}\n", serde_json::to_string_pretty(&by_type).unwrap());

        if std::env::var("INSTA_UPDATE").as_deref() == Ok("always") {
            let root = std::env::var("INSTA_WORKSPACE_ROOT").unwrap_or_else(|_| ".".into());
            let path = std::path::Path::new(&root).join(REL);
            std::fs::write(&path, fresh)
                .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
            return;
        }
        let r = runfiles::Runfiles::create().expect("runfiles tree");
        let path = r
            .rlocation(format!("_main/{REL}"))
            .unwrap_or_else(|| panic!("rlocation for {REL}"));
        let on_disk = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        assert_eq!(
            on_disk, fresh,
            "{REL} is out of date with the provider config crates' declarations; \
             run `bazel run //datalib/backend/datalib_step:ingest_methods.update`"
        );
    }
}
