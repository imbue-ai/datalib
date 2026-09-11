//! The answer a "test connection" gives back, in the one shape every
//! provider that can be probed produces. `datalib-step probe <type>`
//! prints it to stdout and the HTTP server forwards it verbatim, so
//! the field names here are the wire format the wizard reads.

use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantArray};

/// What a successful probe found.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeReport {
    /// Which download method was probed — the same word the config
    /// uses to select it (`gmail`, `jmap`, `api`).
    pub mode: String,
    pub account: ProbeAccount,
    /// What this account holds that a filter field can name: an email
    /// account's mailboxes, a chat account's conversations. Already in
    /// the order a picker should show them.
    pub items: Vec<ProbeItem>,
    /// Things worth telling the person who clicked the button that
    /// aren't failures.
    pub notes: Vec<String>,
}

/// The account the credentials actually reached. Shown back so "Test
/// connection" answers *which* account as well as *whether* — a
/// latchkey store with two logins in it will happily connect to the
/// wrong one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeAccount {
    /// The provider's own id for it.
    pub id: String,
    /// Canonical email address, when the provider tells us one.
    pub address: Option<String>,
    pub display_name: Option<String>,
    /// Total messages, when the provider reports it cheaply.
    pub message_estimate: Option<u64>,
}

/// What one item in a probe's list is.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ProbeItemKind {
    /// A folder emails are filed in. Both the download filter and the
    /// render filter can match one.
    Mailbox,
    /// A Gmail flag — downloadable, but never matched by the
    /// render-side filter.
    Keyword,
    /// One chat thread: a claude.ai conversation, a Slack DM. `path` is
    /// the provider's id for it.
    Conversation,
    /// A Slack channel, public or private. `path` is its bare name.
    Channel,
}

impl ProbeItemKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// One entry in a picker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeItem {
    /// The exact string to write into the field this item feeds — a
    /// label path, a conversation uuid.
    pub path: String,
    pub kind: ProbeItemKind,
    /// A human name for it, when `path` is an opaque id. `None` when
    /// the path already reads as its own name.
    pub title: Option<String>,
    /// A short tag the provider attaches: a JMAP mailbox's role
    /// (`inbox`, `sent`, …), a channel's `private` / `not a member`, a
    /// DM's `group`. Used for ordering and for a hint column in the
    /// picker, never matched by a filter.
    pub role: Option<String>,
    /// Messages here, when the provider reports it for free.
    pub messages: Option<u64>,
    /// People in it, when the provider reports it for free.
    pub members: Option<u64>,
    /// When this last changed, for a picker that sorts by recency.
    pub updated_at: Option<String>,
}

impl ProbeItem {
    /// The plainest item there is: a path that is its own name.
    pub fn new(path: impl Into<String>, kind: ProbeItemKind) -> Self {
        Self {
            path: path.into(),
            kind,
            title: None,
            role: None,
            messages: None,
            members: None,
            updated_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// strum and serde are independent derives producing independent
    /// strings; nothing but this makes them agree.
    #[test]
    fn strum_and_serde_spell_the_kinds_the_same() {
        for kind in ProbeItemKind::VARIANTS {
            let json = serde_json::to_string(kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            assert_eq!(ProbeItemKind::parse(kind.as_str()), Some(*kind));
        }
    }

    #[test]
    fn an_unknown_kind_is_none_rather_than_a_guess() {
        assert_eq!(ProbeItemKind::parse("folder"), None);
    }
}
