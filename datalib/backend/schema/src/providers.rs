// The `grid_rows.provider` tag, as one enum.
//
// The spelling is on disk in three places — the column itself, the
// `provider:` key in every rendered document's frontmatter, and the
// `render_markdown/<provider>/…` path segment — so `as_str` is a storage
// contract, not a label. Changing one would need a re-render and a
// re-index.

/// Which provider a `grid_rows` row came from, and so which
/// per-provider table holds its raw payload.
///
/// Names follow AGENTS.md's "Claude, not Anthropic" rule: the product a
/// person recognizes, never the vendor or the protocol behind it.
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
pub enum Provider {
    /// IQAir's AirVisual monitors.
    Airvisual,
    /// Apple's Messages app. The vendor stays for the reason it does on
    /// `apple_photos`: "messages" alone names nothing.
    AppleMessages,
    /// Baby Connect: a child's month of logged entries.
    BabyConnect,
    Beeper,
    /// Calendar events: Google Calendar, CalDAV (Fastmail, iCloud, …)
    /// and `.ics` files, one tag for all of them.
    Calendar,
    Chatgpt,
    /// Both `claude_api` and `claude_export`: one raw store, one tag.
    Claude,
    /// Claude Code sessions — the terminal, desktop and IDE transcripts.
    ClaudeCode,
    /// Codex CLI sessions — the terminal and IDE rollouts.
    Codex,
    /// CardDAV address books.
    Contacts,
    /// Not an upstream provider: datalib describing a source's own
    /// mirror. The per-source storage report is tagged this way rather
    /// than with the source's provider, so a measurement never lands in
    /// the same render unit as the data it measures.
    /// See `datalib_step::introspect`.
    Datalib,
    /// All three download modes — JMAP, Gmail API, mbox — write this
    /// one tag. Named for the thing, not for whichever protocol a
    /// particular mirror happens to use.
    Email,
    /// A "Download your information" export, the JSON flavour.
    Facebook,
    /// Garmin Connect: a watch's worth of health and activity data.
    Garmin,
    Github,
    Gitlab,
    /// Docs, Sheets and Slides through Drive's and each editor's API.
    GoogleDrive,
    GoogleTakeout,
    /// Adobe Lightroom Classic: a catalog's photos and videos.
    Lightroom,
    Linkedin,
    /// A folder of markdown notes: an Obsidian vault, a notes folder.
    Markdown,
    Notion,
    Pdf,
    Perseus,
    Signal,
    Slack,
    SmsBackupRestore,
    /// Todoist: a project's sections, tasks and comments.
    Todoist,
    Whatsapp,
    Yolink,
    /// Fixtures and unit tests only. A real row never carries it; it
    /// exists so the builder's setter can stay typed instead of taking
    /// a free-form string for the benefit of tests.
    Test,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a tag this build does not know — a row written by an
    /// older or newer version.
    pub fn parse(s: &str) -> Option<Provider> {
        s.parse().ok()
    }

    /// The product's name as a person writes it: what search's text
    /// calls a page's source when its renderer names none.
    pub fn label(self) -> &'static str {
        match self {
            Provider::Airvisual => "AirVisual",
            Provider::AppleMessages => "Apple Messages",
            Provider::BabyConnect => "Baby Connect",
            Provider::Beeper => "Beeper",
            Provider::Calendar => "Calendar",
            Provider::Chatgpt => "ChatGPT",
            Provider::Claude => "Claude",
            Provider::ClaudeCode => "Claude Code",
            Provider::Codex => "Codex",
            Provider::Contacts => "Contacts",
            Provider::Datalib => "datalib",
            Provider::Email => "Email",
            Provider::Facebook => "Facebook",
            Provider::Garmin => "Garmin",
            Provider::Github => "GitHub",
            Provider::Gitlab => "GitLab",
            Provider::GoogleDrive => "Google Drive",
            Provider::GoogleTakeout => "Google Takeout",
            Provider::Lightroom => "Lightroom",
            Provider::Linkedin => "LinkedIn",
            Provider::Markdown => "Markdown",
            Provider::Notion => "Notion",
            Provider::Pdf => "PDF",
            Provider::Perseus => "Perseus",
            Provider::Signal => "Signal",
            Provider::Slack => "Slack",
            Provider::SmsBackupRestore => "SMS Backup & Restore",
            Provider::Todoist => "Todoist",
            Provider::Whatsapp => "WhatsApp",
            Provider::Yolink => "YoLink",
            Provider::Test => "Test",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::VariantArray;

    /// Two providers sharing a spelling would merge their rows in the
    /// grid.
    #[test]
    fn every_tag_is_distinct_and_round_trips() {
        let mut spellings: Vec<&str> = Provider::VARIANTS.iter().map(|p| p.as_str()).collect();
        let n = spellings.len();
        spellings.sort_unstable();
        spellings.dedup();
        assert_eq!(spellings.len(), n, "duplicate provider spelling");
        for &p in Provider::VARIANTS {
            assert_eq!(Provider::parse(p.as_str()), Some(p));
        }
    }

    /// The two that used to be named for a vendor and a protocol. A
    /// rename back would need another re-render, so pin them.
    #[test]
    fn the_two_renamed_tags_are_named_for_the_thing() {
        assert_eq!(Provider::Chatgpt.as_str(), "chatgpt");
        assert_eq!(Provider::Email.as_str(), "email");
    }
}
