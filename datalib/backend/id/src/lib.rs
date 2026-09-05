//! The one place datalib mints an entity id.
//!
//! Every `grid_rows.uuid`, `markdown_uuid` and `data-section-uuid` anchor is
//! a UUIDv5 derived here from one four-part recipe under one root namespace,
//! so "could these two ids collide?" has one answer read off one file.
//! Choosing a scope, and what each choice costs, is `docs/dev/entity_ids.md`.

use uuid::Uuid;

/// Root namespace for every datalib-minted id. Frozen forever —
/// changing these bytes re-keys every row in every data root that has
/// ever existed, and orphans every `feedback.target_uuids` entry
/// pointing into the old keyspace.
pub const DATALIB_ID_NS: Uuid = Uuid::from_bytes([
    0x64, 0x61, 0x74, 0x61, 0x6c, 0x69, 0x62, 0x2d, 0x69, 0x64, 0x2d, 0x6e, 0x73, 0x2d, 0x76, 0x31,
]);

/// The space an entity id is unique within.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// Unique within one upstream account / workspace / organization,
    /// identified by a **provider-issued** id: an Anthropic
    /// `org_uuid`, a Slack `team_id`, a JMAP `account_id`, a Signal
    /// account identifier, a YoLink `family_device_id`.
    Upstream(&'a str),

    /// The natural key is already unique across the entire provider,
    /// so no further scoping is needed: a GitHub `{repo}:pr:{number}`,
    /// a Notion `page_id`, a WhatsApp `chat_jid`, an Anthropic
    /// `conversation_uuid`.
    ProviderGlobal,

    /// The configured source itself, identified by its **step id** —
    /// the stable half of a source's identity, not its display name.
    SourceInstance(&'a str),

    /// Identity is the content itself, so two sources that find the
    /// same bytes deliberately produce one row — a PDF discovered
    /// under two scanned trees, the same canonical text from two
    /// corpora.
    Content,
}

impl Scope<'_> {
    /// The scope's contribution to the recipe. Each variant gets a
    /// distinct tag so a `Content` id can never collide with an
    /// `Upstream` id that happens to carry the same string.
    fn tag(&self) -> (&'static str, &str) {
        match self {
            Scope::Upstream(id) => ("up", id),
            Scope::SourceInstance(id) => ("src", id),
            Scope::ProviderGlobal => ("pg", ""),
            Scope::Content => ("content", ""),
        }
    }
}

/// Mint the id for one entity.
///
/// Components are joined with `\x1f` (ASCII unit separator), which cannot
/// appear in any upstream id we ingest. Joining with `:` or `-` — as most of
/// the recipes this replaced did — makes `("a:b", "c")` and `("a", "b:c")`
/// hash identically.
///
/// **Feed the same `natural_key` string to `grid_rows.upstream_id`.** Using
/// one spelling to derive the id and storing another produces a backpointer
/// that looks plausible and regenerates nothing.
pub fn entity_id(provider: &str, scope: Scope<'_>, entity_kind: &str, natural_key: &str) -> Uuid {
    let (scope_tag, scope_val) = scope.tag();
    let recipe = format!(
        "{provider}\u{1f}{scope_tag}\u{1f}{scope_val}\u{1f}{entity_kind}\u{1f}{natural_key}"
    );
    Uuid::new_v5(&DATALIB_ID_NS, recipe.as_bytes())
}

/// Join the parts of a **composite natural key** — an Anthropic
/// `(message_uuid, tool_use_id)`, a Slack `(channel_id, ts)`.
///
/// Uses `#`, not the `\x1f` that separates recipe *components*, because this
/// exact string is also what `grid_rows.upstream_id` stores and what the
/// grid's "Copy source ID(s)" action puts on a clipboard. Parts must not
/// contain `#`; debug builds assert it.
pub fn composite_key(parts: &[&str]) -> String {
    debug_assert!(
        parts.iter().all(|p| !p.contains('#')),
        "composite_key parts must not contain '#': {parts:?}"
    );
    parts.join("#")
}

pub fn entity_id_str(
    provider: &str,
    scope: Scope<'_>,
    entity_kind: &str,
    natural_key: &str,
) -> String {
    entity_id(provider, scope, entity_kind, natural_key)
        .as_hyphenated()
        .to_string()
}

/// Id for one `edges` row, from the directed tuple it connects.
///
/// Separate from [`entity_id`] because an edge is not scoped to a provider —
/// it may join two documents from different ones — and its natural key is the
/// tuple itself. Producers must derive edge ids this way, so a re-render
/// replaces its edges instead of duplicating them.
pub fn edge_id(
    src_markdown_uuid: &str,
    src_anchor_uuid: Option<&str>,
    dst_markdown_uuid: &str,
    dst_anchor_uuid: Option<&str>,
    label: Option<&str>,
) -> String {
    let recipe = format!(
        "edge\u{1f}{src_markdown_uuid}\u{1f}{}\u{1f}{dst_markdown_uuid}\u{1f}{}\u{1f}{}",
        src_anchor_uuid.unwrap_or(""),
        dst_anchor_uuid.unwrap_or(""),
        label.unwrap_or(""),
    );
    Uuid::new_v5(&DATALIB_ID_NS, recipe.as_bytes())
        .as_hyphenated()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_deterministic() {
        let a = entity_id("slack", Scope::Upstream("T123"), "message", "C1:170.5");
        let b = entity_id("slack", Scope::Upstream("T123"), "message", "C1:170.5");
        assert_eq!(a, b, "ids must be a pure function of their inputs");
    }

    #[test]
    fn is_a_v5_uuid() {
        let id = entity_id("slack", Scope::Upstream("T123"), "message", "C1:170.5");
        assert_eq!(id.get_version_num(), 5);
        // The shape `ingested_tng_test` asserts on.
        let s = id.as_hyphenated().to_string();
        assert_eq!(s.len(), 36);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "{s}");
    }

    #[test]
    fn provider_separates() {
        assert_ne!(
            entity_id("slack", Scope::ProviderGlobal, "chat", "X"),
            entity_id("notion", Scope::ProviderGlobal, "chat", "X"),
        );
    }

    #[test]
    fn upstream_scope_separates_two_accounts() {
        // The property that makes configuring one provider twice safe.
        // Signal's `chat_id` is an autoincrement local to a backup
        // file, so two accounts really do both have chat `1`.
        assert_ne!(
            entity_id("signal", Scope::Upstream("acct-a"), "chat", "1"),
            entity_id("signal", Scope::Upstream("acct-b"), "chat", "1"),
        );
    }

    /// The bug the old *documented* Slack recipe had: a thread root's
    /// `thread_ts` equals its own `ts`, so keying both on
    /// `{team}:{channel}:{ts}` collides on every single thread root.
    #[test]
    fn entity_kind_separates_thread_root_from_its_message() {
        let key = "C1\u{1f}1700000000.000100";
        assert_ne!(
            entity_id("slack", Scope::Upstream("T1"), "thread", key),
            entity_id("slack", Scope::Upstream("T1"), "message", key),
        );
    }

    /// Joining components with a separator that can appear inside a
    /// component makes the split ambiguous. Most of the recipes this
    /// crate replaces joined on `:` or `-`, both of which occur freely
    /// in upstream ids (and in UUIDs).
    #[test]
    fn component_boundaries_are_unambiguous() {
        assert_ne!(
            entity_id("p", Scope::ProviderGlobal, "a:b", "c"),
            entity_id("p", Scope::ProviderGlobal, "a", "b:c"),
        );
        assert_ne!(
            entity_id("p", Scope::ProviderGlobal, "a-b", "c"),
            entity_id("p", Scope::ProviderGlobal, "a", "b-c"),
        );
        // The concrete case: `th-{msg_uuid}-{block_index}` is
        // ambiguous between message `M` block `0` and a message
        // literally named `M-0`.
        assert_ne!(
            entity_id(
                "claude",
                Scope::ProviderGlobal,
                "thinking_block",
                "M\u{1f}0"
            ),
            entity_id("claude", Scope::ProviderGlobal, "thinking_block", "M-0"),
        );
    }

    /// A `Content`-scoped id must not collide with an `Upstream` one
    /// carrying the same string, or a content hash reused as an
    /// account label would alias.
    /// A source-instance id is scoped to the step id, so two
    /// configured sources of one type stay apart — the property the
    /// bare provider type cannot give.
    #[test]
    fn source_instance_separates_two_configured_sources() {
        assert_ne!(
            entity_id(
                "yolink",
                Scope::SourceInstance("home-yolink"),
                "page",
                "timeseries"
            ),
            entity_id(
                "yolink",
                Scope::SourceInstance("cabin-yolink"),
                "page",
                "timeseries"
            ),
        );
    }

    #[test]
    fn scope_variants_do_not_alias() {
        assert_ne!(
            entity_id("pdf", Scope::Content, "document", "abc"),
            entity_id("pdf", Scope::ProviderGlobal, "document", "abc"),
        );
        assert_ne!(
            entity_id("pdf", Scope::Upstream(""), "document", "abc"),
            entity_id("pdf", Scope::ProviderGlobal, "document", "abc"),
        );
        // An upstream account id and a step id are different spaces
        // even when they spell the same thing.
        assert_ne!(
            entity_id("p", Scope::Upstream("x"), "k", "n"),
            entity_id("p", Scope::SourceInstance("x"), "k", "n"),
        );
    }

    /// The invariant the fixture test enforces from the outside:
    /// whatever string goes into the recipe is the string that goes
    /// into `upstream_id`, so recomputing from the stored columns
    /// reproduces the id.
    #[test]
    fn a_composite_key_round_trips() {
        let key = composite_key(&["msg-1", "toolu_9"]);
        assert_eq!(key, "msg-1#toolu_9");
        assert_eq!(
            entity_id_str("claude", Scope::ProviderGlobal, "tool_use", &key),
            entity_id_str("claude", Scope::ProviderGlobal, "tool_use", "msg-1#toolu_9"),
        );
    }

    #[test]
    fn composite_keys_do_not_alias_across_part_boundaries() {
        assert_ne!(composite_key(&["a", "bc"]), composite_key(&["ab", "c"]),);
    }

    #[test]
    fn edges_are_deterministic_and_direction_sensitive() {
        let fwd = edge_id("md-a", Some("s1"), "md-b", Some("d1"), Some("x"));
        assert_eq!(
            fwd,
            edge_id("md-a", Some("s1"), "md-b", Some("d1"), Some("x"))
        );
        assert_ne!(
            fwd,
            edge_id("md-b", Some("d1"), "md-a", Some("s1"), Some("x"))
        );
        // An absent anchor is distinct from an empty one only insofar
        // as they render the same; document that they intentionally do
        // NOT differ, so nobody relies on the difference.
        assert_eq!(
            edge_id("md-a", None, "md-b", None, None),
            edge_id("md-a", Some(""), "md-b", Some(""), Some("")),
        );
    }
}
