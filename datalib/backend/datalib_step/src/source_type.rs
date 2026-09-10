//! The source types this binary can run, as one enum.
//!
//! The wire spelling is what a `[[groups]]` entry's `type` names, so it
//! is fixed by the configs people have already written — `as_str` is
//! that contract, not a display convenience. A type names the thing
//! being mirrored, never the way it is reached: `claude`, whether the
//! data came over the API or out of an export; `contacts`, whether over
//! CardDAV or from `.vcf` files. Where the data comes from is a table
//! in the ingest step's params, named under the type — `api` is that
//! product's own API — and declared by each provider's config crate
//! (`IngestMethods`).
//!
//! The value of naming them here is that [`crate::dispatch::plan`]
//! matches on the enum: adding a variant without wiring it up is a
//! compile error, not a runtime "unknown source type", and the list a
//! bad type is reported against is derived from the same enum the
//! dispatcher walks.

use strum::VariantArray;

/// One source type. Named for the product a person recognizes, never
/// for the vendor behind it — see AGENTS.md's "Claude, not Anthropic".
/// Where a `grid_rows.provider` tag exists for a type, the two spell it
/// the same way.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
    strum::Display,
)]
#[strum(serialize_all = "snake_case")]
pub enum SourceType {
    Beeper,
    Chatgpt,
    /// Claude.ai over the API, or an unpacked export; one raw store.
    Claude,
    /// Contacts over CardDAV, or from `.vcf` files. Served by the
    /// `contacts` provider crate (its config crate is still `contacts_config`).
    Contacts,
    Email,
    Fsindex,
    Github,
    Gitlab,
    GoogleTakeout,
    Lightroom,
    Linkedin,
    Media,
    Notion,
    Pdf,
    Perseus,
    Signal,
    Slack,
    SmsBackupRestore,
    Whatsapp,
    Yolink,
}

impl SourceType {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a type this build has no provider for.
    pub fn parse(s: &str) -> Option<SourceType> {
        s.parse().ok()
    }

    /// Whether this type's downloader authenticates through latchkey,
    /// and so accepts a `latchkey_settings.account` naming which stored
    /// identity to mirror. Decides whether the multi-account note is
    /// appended to an auth hint; the schema itself is enforced by which
    /// provider config crates compose `LatchkeySettings` at all.
    pub const fn uses_latchkey_account(self) -> bool {
        matches!(
            self,
            SourceType::Contacts
                | SourceType::Chatgpt
                | SourceType::Claude
                | SourceType::Email
                | SourceType::Github
                | SourceType::Gitlab
                | SourceType::Notion
                | SourceType::Slack
        )
    }

    /// The known types, sorted, for an error message.
    pub fn known_list() -> String {
        let mut names: Vec<&str> = SourceType::VARIANTS.iter().map(|t| t.as_str()).collect();
        names.sort_unstable();
        names.join(", ")
    }

    /// Whether a spelling is one the configs written before the type
    /// named the thing mirrored used (`slack_api`, `claude_export`,
    /// `signal_backup`, `carddav`). Only for the error message: the
    /// rewrite itself is `datalib-migrate-config`'s, and is understood
    /// there alone.
    pub fn looks_retired(s: &str) -> bool {
        s == "carddav"
            || s == "claude_export"
            || s.strip_suffix("_api")
                .or_else(|| s.strip_suffix("_backup"))
                .is_some_and(|stem| SourceType::parse(stem).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_type_round_trips_and_is_distinct() {
        let mut names: Vec<&str> = SourceType::VARIANTS.iter().map(|t| t.as_str()).collect();
        let n = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), n, "duplicate source-type spelling");
        for &t in SourceType::VARIANTS {
            assert_eq!(SourceType::parse(t.as_str()), Some(t));
        }
        assert_eq!(SourceType::parse("carrier_pigeon"), None);
    }

    /// The retired spellings are not types any more, and are recognised
    /// only so the refusal can name the migrator.
    #[test]
    fn retired_spellings_do_not_parse_but_are_recognised() {
        for old in [
            "slack_api",
            "chatgpt_api",
            "claude_api",
            "claude_export",
            "github_api",
            "gitlab_api",
            "notion_api",
            "signal_backup",
            "whatsapp_backup",
            "carddav",
        ] {
            assert_eq!(SourceType::parse(old), None, "{old}");
            assert!(SourceType::looks_retired(old), "{old}");
        }
        assert!(!SourceType::looks_retired("slack"));
        assert!(!SourceType::looks_retired("carrier_pigeon_api"));
        // `sms_backup_restore` is the app's own name, not a method suffix.
        assert!(!SourceType::looks_retired("sms_backup_restore"));
    }

    /// Where a `grid_rows.provider` tag exists for a type, the config
    /// word and the tag are the same word; a person reading either
    /// should not have to translate.
    #[test]
    fn type_spellings_agree_with_the_provider_tag() {
        use datalib_schema::providers::Provider;
        for &t in SourceType::VARIANTS {
            let tag = Provider::parse(t.as_str());
            let download_only = matches!(
                t,
                SourceType::Fsindex | SourceType::Lightroom | SourceType::Media
            );
            assert_eq!(
                tag.is_some(),
                !download_only,
                "{t}: a type that renders has a provider tag spelled the same way"
            );
        }
    }
}
