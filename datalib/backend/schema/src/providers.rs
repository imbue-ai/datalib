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
    Beeper,
    Chatgpt,
    /// Both `claude_api` and `claude_export`: one raw store, one tag.
    Claude,
    /// CardDAV address books.
    Contacts,
    /// Not an upstream provider: datalib describing a source's own
    /// mirror. The per-source storage report is tagged this way rather
    /// than with the source's provider, so a measurement never lands in
    /// the same bucket as the data it measures.
    /// See `datalib_step::introspect`.
    Datalib,
    /// All three download modes — JMAP, Gmail API, mbox — write this
    /// one tag. Named for the thing, not for whichever protocol a
    /// particular mirror happens to use.
    Email,
    Github,
    Gitlab,
    GoogleTakeout,
    Linkedin,
    Notion,
    Pdf,
    Perseus,
    Signal,
    Slack,
    SmsBackupRestore,
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
