// The `grid_rows.provider` tag, as one enum.
//
// The spelling is on disk in three places — the column itself, the
// `provider:` key in every rendered document's frontmatter, and the
// `rendered_md/<provider>/…` path segment — so `as_str` is a storage
// contract, not a label. Changing one would need a re-render and a
// re-index.

/// Which provider a `grid_rows` row came from, and so which
/// per-provider table holds its raw payload.
///
/// Names follow AGENTS.md's "Claude, not Anthropic" rule: the product a
/// person recognizes, never the vendor or the protocol behind it. Two
/// variants are named the wrong way and are stuck that way — see their
/// notes.
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
    /// Both `claude_api` and `claude_export`: one raw store, one tag.
    Claude,
    /// CardDAV address books.
    Contacts,
    Github,
    Gitlab,
    GoogleTakeout,
    /// ChatGPT. Spelled `openai` on disk, which is the vendor rather
    /// than the product — the one place the naming rule is broken.
    /// Fixing it means re-rendering and re-indexing every ChatGPT row,
    /// so the wrong name stays until something else forces that.
    #[strum(serialize = "openai")]
    Chatgpt,
    /// Email. Spelled `jmap` on disk, after the protocol the first
    /// download mode used — but the source has three modes now (JMAP,
    /// Gmail API, mbox) and they all write this tag. Same story as
    /// [`Provider::Chatgpt`]: wrong, and pinned by stored data.
    #[strum(serialize = "jmap")]
    Email,
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

    /// These two are wrong on purpose and pinned by stored data. The
    /// test is here so a well-meaning rename has to argue with it.
    #[test]
    fn the_two_misnamed_tags_keep_their_stored_spelling() {
        assert_eq!(Provider::Chatgpt.as_str(), "openai");
        assert_eq!(Provider::Email.as_str(), "jmap");
    }
}
