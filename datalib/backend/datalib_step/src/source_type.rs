//! The source types this binary can run, as one enum.
//!
//! The wire spelling is what a config's `[[steps]]` entry names and
//! what `datalib-step download|render <type>` takes on the command
//! line, so it is fixed by the configs people have already written —
//! `as_str` is that contract, not a display convenience. The value of
//! naming them here is that [`crate::dispatch::plan`] matches on the
//! enum: adding a variant without wiring it up is a compile error, not
//! a runtime "unknown source type", and the list a bad type is
//! reported against is derived from the same enum the dispatcher walks.

use strum::VariantArray;

/// One source type. Named for the product a person recognizes, never
/// for the vendor behind it — see AGENTS.md's "Claude, not Anthropic".
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
    /// Contacts over CardDAV. Served by the `contacts` provider crate.
    Carddav,
    ChatgptApi,
    /// The live claude.ai API.
    ClaudeApi,
    /// An unpacked claude.ai bulk export. Writes the same raw store as
    /// [`SourceType::ClaudeApi`] — see the provider's DOWNLOAD.md.
    ClaudeExport,
    Email,
    Fsindex,
    GithubApi,
    GitlabApi,
    GoogleTakeout,
    Lightroom,
    Linkedin,
    Media,
    NotionApi,
    Pdf,
    Perseus,
    SignalBackup,
    SlackApi,
    SmsBackupRestore,
    WhatsappBackup,
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
            SourceType::Carddav
                | SourceType::ChatgptApi
                | SourceType::ClaudeApi
                | SourceType::Email
                | SourceType::GithubApi
                | SourceType::GitlabApi
                | SourceType::NotionApi
                | SourceType::SlackApi
        )
    }

    /// The known types, sorted, for an error message.
    pub fn known_list() -> String {
        let mut names: Vec<&str> = SourceType::VARIANTS.iter().map(|t| t.as_str()).collect();
        names.sort_unstable();
        names.join(", ")
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
}
