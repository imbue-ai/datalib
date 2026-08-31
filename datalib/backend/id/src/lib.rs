//! The one place datalib mints an entity id.
//!
//! Every `grid_rows.uuid`, `markdown_uuid`, and `data-section-uuid`
//! anchor in the system is a UUIDv5 derived here, under a single root
//! namespace, from an explicit four-part recipe. The point is not the
//! hashing — sixteen providers were already doing that — it is that the
//! question *"could these two ids collide?"* now has one answer, read
//! off one file, instead of sixteen hand-rolled recipes each with its
//! own frozen namespace constant and its own idea of what an id is
//! scoped to.
//!
//! # What went wrong without it
//!
//! Before this crate, providers split four ways with no shared rule:
//!
//! * **Foreign string verbatim** — anthropic, chatgpt and notion used
//!   the upstream's own id as our primary key. 12% of the fixture's
//!   `grid_rows.uuid` values were consequently not UUIDs at all
//!   (`tu-{tool_use_id}`, `th-{msg_uuid}-{idx}`, ChatGPT's
//!   `msg-…` ids). Nothing parses the column as a UUID today, so this
//!   was benign — but it put a foreign namespace inside ours, where a
//!   single upstream id-reuse becomes our collision.
//! * **Provider namespace + upstream account scope** — slack, email,
//!   github, gitlab, beeper and friends. This is the shape that was
//!   right, and [`Scope::Upstream`] is it.
//! * **Provider namespace + our config's `source_name`** — signal,
//!   whatsapp, yolink, and (via a caller passing `source_name` into a
//!   parameter literally named `account_id`) contacts. Renaming a
//!   source in `config.toml` silently re-keys every row it ever
//!   produced and orphans every `feedback.target_uuids` pointing at
//!   them. [`Scope`] has no variant for this on purpose.
//! * **Content-addressed** — pdf (blake3), perseus (canonical work
//!   id). Deliberately source-independent, so two sources finding one
//!   file collapse to one row. [`Scope::Content`] keeps that explicit
//!   rather than accidental.
//!
//! # Why not `source_type` either
//!
//! Swapping `source_name` for the *type* (`"signal"`, `"yolink"`) is
//! stable against renames but stops discriminating exactly where the
//! discrimination was load-bearing: signal's `chat_id` is an
//! autoincrement local to one backup file, and yolink's `device` is a
//! user-typed label like `"fridge"`. Two configured accounts of either
//! type would collide on every row. What those providers need is a
//! stable *upstream* identity — signal's own account identifier,
//! yolink's `family_device_id` — which is what [`Scope::Upstream`]
//! asks for.
//!
//! # Why not mint opaque random ids
//!
//! Tempting, and it does make collisions impossible. It also costs
//! idempotent re-ingest: a v4 has to be looked up through a
//! backpointer table on every render, and a fresh data root
//! re-ingesting the same upstream data produces *different* ids. The
//! fixture suite asserts byte-stable convergence across three runs and
//! the insta goldens pin rendered output, both of which rest on ids
//! being a pure function of upstream data. Determinism is the property
//! to keep; uniqueness is the property to fix.

use uuid::Uuid;

/// Root namespace for every datalib-minted id. Frozen forever —
/// changing these bytes re-keys every row in every data root that has
/// ever existed, and orphans every `feedback.target_uuids` entry
/// pointing into the old keyspace.
///
/// Generated once as a v4 and hard-coded; it is a namespace, not a
/// secret, and it must never be regenerated.
pub const DATALIB_ID_NS: Uuid = Uuid::from_bytes([
    0x64, 0x61, 0x74, 0x61, 0x6c, 0x69, 0x62, 0x2d, 0x69, 0x64, 0x2d, 0x6e, 0x73, 0x2d, 0x76, 0x31,
]);

/// The space an entity id is unique within.
///
/// This is the decision that used to be implicit in each provider's
/// recipe string, and the one that determines whether two configured
/// sources can collide. Making it a type means a new provider has to
/// answer the question rather than copy whichever neighbour it read
/// first.
///
/// There is deliberately **no `SourceName` variant**. Scoping an id to
/// the config's `source_name` means renaming a source re-keys every
/// row it produced, silently orphaning filed feedback — and the name
/// is editable from the Manage tab, so that is a one-click data loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// Unique within one upstream account / workspace / organization,
    /// identified by a **provider-issued** id: an Anthropic
    /// `org_uuid`, a Slack `team_id`, a JMAP `account_id`, a Signal
    /// account identifier, a YoLink `family_device_id`.
    ///
    /// The default choice, and the only one that safely lets a user
    /// configure the same provider twice. The value must come from
    /// upstream data, never from our config: config strings are
    /// user-editable and re-keying on an edit is the failure mode this
    /// whole type exists to prevent.
    Upstream(&'a str),

    /// The natural key is already unique across the entire provider,
    /// so no further scoping is needed: a GitHub `{repo}:pr:{number}`,
    /// a Notion `page_id`, a WhatsApp `chat_jid`, an Anthropic
    /// `conversation_uuid`.
    ///
    /// Use only when the upstream genuinely guarantees this. "Probably
    /// unique" is [`Scope::Upstream`] with the account id, which costs
    /// nothing extra and cannot be wrong.
    ProviderGlobal,

    /// Identity is the content itself, so two sources that find the
    /// same bytes deliberately produce one row — a PDF discovered
    /// under two scanned trees, the same canonical text from two
    /// corpora.
    ///
    /// This *will* make two overlapping sources contend for one id.
    /// That is the intended behaviour, and `IdClaims` in
    /// `datalib_etl::grid_index` turns the contention into an error
    /// naming both sources rather than letting one silently erase the
    /// other.
    Content,
}

impl Scope<'_> {
    /// The scope's contribution to the recipe. Each variant gets a
    /// distinct tag so a `Content` id can never collide with an
    /// `Upstream` id that happens to carry the same string.
    fn tag(&self) -> (&'static str, &str) {
        match self {
            Scope::Upstream(id) => ("up", id),
            Scope::ProviderGlobal => ("pg", ""),
            Scope::Content => ("content", ""),
        }
    }
}

/// Mint the id for one entity.
///
/// * `provider` — the `grid_rows.provider` tag (`"anthropic"`,
///   `"slack"`, …). Namespaces every id by provider, so two providers
///   can never collide however similar their natural keys look.
/// * `scope` — see [`Scope`].
/// * `entity_kind` — what *sort* of thing this is within the provider
///   (`"chat"`, `"message"`, `"thinking_block"`, `"tool_use"`). Two
///   entities with the same natural key but different kinds must get
///   different ids: this is the field that keeps a Slack thread root
///   distinct from the message at the same `ts`, which the old
///   documented Slack recipe got wrong.
/// * `natural_key` — the upstream's own identifier for the entity,
///   within `scope`.
///
/// Components are joined with `\x1f` (ASCII unit separator), which
/// cannot appear in any upstream id we ingest. Joining with `:` or `-`
/// — as most of the replaced recipes did — makes
/// `("a:b", "c")` and `("a", "b:c")` hash identically.
pub fn entity_id(provider: &str, scope: Scope<'_>, entity_kind: &str, natural_key: &str) -> Uuid {
    let (scope_tag, scope_val) = scope.tag();
    let recipe = format!(
        "{provider}\u{1f}{scope_tag}\u{1f}{scope_val}\u{1f}{entity_kind}\u{1f}{natural_key}"
    );
    Uuid::new_v5(&DATALIB_ID_NS, recipe.as_bytes())
}

/// Join the parts of a **composite natural key**.
///
/// A natural key is one recipe component; when the upstream identity is
/// a tuple — an Anthropic `(message_uuid, tool_use_id)`, a Slack
/// `(channel_id, ts)` — this is how the parts are joined.
///
/// Uses `#`, not the `\x1f` that separates recipe *components*,
/// because this exact string is also what `grid_rows.source_native_id`
/// stores and what the grid's "Copy source ID(s)" action puts on a
/// user's clipboard. A control character there would be user-hostile.
///
/// **Feed the same string to [`entity_id`] and to
/// `source_native_id`.** Building the key once and using it twice is
/// what makes the round-trip
/// (`entity_id(provider, scope, kind, source_native_id) == uuid`) hold;
/// deriving the id from one spelling and storing another produces a
/// backpointer that looks plausible and regenerates nothing.
/// `//tests/fixtures:ingested_tng_test` reimplements the recipe and
/// checks exactly this.
///
/// Parts must not contain `#`. Debug builds assert it; in release a
/// violating part would make the join ambiguous rather than unsafe.
pub fn composite_key(parts: &[&str]) -> String {
    debug_assert!(
        parts.iter().all(|p| !p.contains('#')),
        "composite_key parts must not contain '#': {parts:?}"
    );
    parts.join("#")
}

/// [`entity_id`] as the hyphenated string the schema columns store.
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
/// Split out from [`entity_id`] because an edge is not scoped to a
/// provider — it may join two documents from different ones — and its
/// natural key is the tuple itself. Producers must derive edge ids
/// this way so a re-render replaces its edges instead of duplicating
/// them.
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
                "anthropic",
                Scope::ProviderGlobal,
                "thinking_block",
                "M\u{1f}0"
            ),
            entity_id("anthropic", Scope::ProviderGlobal, "thinking_block", "M-0"),
        );
    }

    /// A `Content`-scoped id must not collide with an `Upstream` one
    /// carrying the same string, or a content hash reused as an
    /// account label would alias.
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
    }

    /// The invariant the fixture test enforces from the outside:
    /// whatever string goes into the recipe is the string that goes
    /// into `source_native_id`, so recomputing from the stored columns
    /// reproduces the id.
    #[test]
    fn a_composite_key_round_trips() {
        let key = composite_key(&["msg-1", "toolu_9"]);
        assert_eq!(key, "msg-1#toolu_9");
        assert_eq!(
            entity_id_str("anthropic", Scope::ProviderGlobal, "tool_use", &key),
            entity_id_str(
                "anthropic",
                Scope::ProviderGlobal,
                "tool_use",
                "msg-1#toolu_9"
            ),
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
