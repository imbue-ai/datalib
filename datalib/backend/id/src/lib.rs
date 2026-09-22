//! The one place datalib mints an entity id.
//!
//! Every `grid_rows.uuid`, `markdown_uuid` and `data-section-uuid` anchor is
//! minted here from one four-part recipe under one root namespace, so
//! "could these two ids collide?" has one answer read off one file.
//! Choosing a scope, and what each choice costs, is `docs/dev/entity_ids.md`.
//!
//! The layout is RFC 9562's version 8: the leading 48 bits are the
//! record's own `created_at` in unix milliseconds, the rest a v5 hash of
//! the recipe. Every store here is a doltlite prolly tree sorted by
//! primary key, and a write rewrites every leaf its keys fall in, so
//! keys that scatter (a plain hash) cost one leaf per row while keys
//! that sort by time cost one leaf per batch — `etl/README.md` § "What a
//! write costs" has the measurement. A record with no stamp of its own
//! takes zero and sorts to the left edge.

use uuid::Uuid;

/// Root namespace for every datalib-minted id. Frozen forever —
/// changing these bytes re-keys every row in every data root that has
/// ever existed, and orphans every `feedback.target_uuids` entry
/// pointing into the old keyspace.
pub const DATALIB_ID_NS: Uuid = Uuid::from_bytes([
    0x64, 0x61, 0x74, 0x61, 0x6c, 0x69, 0x62, 0x2d, 0x69, 0x64, 0x2d, 0x6e, 0x73, 0x2d, 0x76, 0x31,
]);

/// The provider half of the recipe: the space one provider's natural
/// keys are unique within, so two providers that happen to mint the
/// same key never collide.
///
/// **These values are frozen.** Each one is hashed into every id that
/// provider has ever minted, so changing one re-keys every
/// `grid_rows.uuid`, `markdown_uuid` and `data-section-uuid` it
/// produced, and orphans every `feedback.target_uuids` pointing at
/// them. The only supported way to change one is to bump that
/// provider's `RENDER_VERSION` in the same commit, which makes the
/// next run discard its rendered tree and re-render from the raw
/// store — see `datalib_step::render`.
///
/// One entry per provider that renders anything, spelled as its
/// `grid_rows.provider` tag; [`IdNamespace::Datalib`] is the one entry
/// that is not a provider at all.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
    strum::Display,
)]
#[strum(serialize_all = "snake_case")]
pub enum IdNamespace {
    /// IQAir's AirVisual monitors: the page datalib composes per source
    /// and the device rows under it.
    Airvisual,
    /// Apple's Messages app: chats and messages by the guids Messages
    /// mints for them.
    AppleMessages,
    Beeper,
    /// Both `claude_api` and `claude_export` — one raw store, one
    /// keyspace, so an export-seeded mirror kept fresh by the API does
    /// not mint two ids for one conversation.
    Claude,
    /// Claude Code sessions: every key is one Claude Code minted (a
    /// session id, a record uuid, a tool-use id).
    ClaudeCode,
    Chatgpt,
    Contacts,
    /// Every email method — JMAP, the Gmail API, an mbox — one keyspace
    /// per account id.
    Email,
    Facebook,
    Garmin,
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
    /// Not a provider: datalib's own measurements of a source's mirror,
    /// which are minted by this recipe like anything else. Kept in its
    /// own namespace so a storage row can never collide with a row from
    /// the source it measures. See `datalib_step::introspect`.
    Datalib,
}

impl IdNamespace {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// The space an entity id is unique within.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// Unique within one upstream account / workspace / organization,
    /// identified by a **provider-issued** id: a Slack `team_id`, a
    /// JMAP `account_id`, a GitHub repository.
    /// Never a secret: the value is stored in `grid_rows.upstream_scope`
    /// in the clear.
    Upstream(&'a str),

    /// The natural key is already unique across the entire provider,
    /// so no further scoping is needed: a Notion `page_id`, an
    /// Anthropic `conversation_uuid`, a LinkedIn profile URL, a
    /// Facebook `fbid`.
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

/// The largest stamp the leading 48 bits can hold (the year 10889).
/// A stamp outside `0..=MAX_STAMP_MS` is clamped to the nearer edge, on
/// both sides of the round-trip check.
pub const MAX_STAMP_MS: i64 = (1 << 48) - 1;

/// An entity's identity: the id we mint, and what it was minted from.
///
/// Paired so `grid_rows.uuid` and its backpointer columns cannot drift —
/// build the key once, use it twice. `at` is the stamp in the id's
/// leading bits, and the one rule about it is that **it is the row's
/// `created_at` or nothing**: the fixture's round-trip check reads the
/// stamp back out of the uuid and compares it to `created_at_utc`, so
/// a stamp that is not the row's own fails there. A record with no
/// stamp, and a document whose stamp is derived from its items (a
/// chat's first message can move), pass `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub uuid: String,
    pub natural_key: String,
    pub entity_kind: &'static str,
    pub at: Option<i64>,
}

impl Identity {
    pub fn mint(
        namespace: IdNamespace,
        scope: Scope<'_>,
        entity_kind: &'static str,
        natural_key: String,
        at: Option<i64>,
    ) -> Self {
        Self {
            uuid: entity_id_str(namespace, scope, entity_kind, &natural_key, at),
            natural_key,
            entity_kind,
            at,
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
/// `at` is the record's own `created_at` in unix milliseconds, at the
/// precision the row stores it, or `None` — see [`Identity`] for the rule.
/// It goes in the leading 48 bits and nowhere else: two ids that differ
/// only in `at` share every hash bit, which is what lets the round-trip
/// check regenerate the hash from the backpointer alone.
///
/// **Feed the same `natural_key` string to `grid_rows.upstream_id`.** Using
/// one spelling to derive the id and storing another produces a backpointer
/// that looks plausible and regenerates nothing.
pub fn entity_id(
    namespace: IdNamespace,
    scope: Scope<'_>,
    entity_kind: &str,
    natural_key: &str,
    at: Option<i64>,
) -> Uuid {
    let (scope_tag, scope_val) = scope.tag();
    let namespace = namespace.as_str();
    let recipe = format!(
        "{namespace}\u{1f}{scope_tag}\u{1f}{scope_val}\u{1f}{entity_kind}\u{1f}{natural_key}"
    );
    time_prefixed(Uuid::new_v5(&DATALIB_ID_NS, recipe.as_bytes()), at)
}

/// Lay a v5 hash out as a v8 id: the stamp in the leading 48 bits, the
/// version nibble set to 8, the variant bits and every other hash bit
/// kept. The hash's own leading 48 bits are discarded, so the id keeps
/// 74 bits of hash — plenty for a keyspace a person's data will reach.
fn time_prefixed(hash: Uuid, at: Option<i64>) -> Uuid {
    let ms = at.unwrap_or(0).clamp(0, MAX_STAMP_MS) as u64;
    let mut b = hash.into_bytes();
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6] = 0x80 | (b[6] & 0x0f);
    Uuid::from_bytes(b)
}

/// The stamp a datalib-minted id carries in its leading bits, as unix
/// milliseconds; `None` for a zero stamp or a string that is not a uuid.
/// What a derived id (an edge, a diff row) copies so it sorts beside the
/// row it is about.
pub fn stamp_of(id: &str) -> Option<i64> {
    let b = Uuid::parse_str(id).ok()?.into_bytes();
    let mut be = [0u8; 8];
    be[2..].copy_from_slice(&b[..6]);
    match u64::from_be_bytes(be) {
        0 => None,
        ms => Some(ms as i64),
    }
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
    namespace: IdNamespace,
    scope: Scope<'_>,
    entity_kind: &str,
    natural_key: &str,
    at: Option<i64>,
) -> String {
    entity_id(namespace, scope, entity_kind, natural_key, at)
        .as_hyphenated()
        .to_string()
}

/// Id for one `edges` row, from the directed tuple it connects.
///
/// Separate from [`entity_id`] because an edge is not scoped to a provider —
/// it may join two documents from different ones — and its natural key is the
/// tuple itself. Producers must derive edge ids this way, so a re-render
/// replaces its edges instead of duplicating them. The stamp is the
/// source end's (the anchor's, else the document's), so the edges a
/// render writes beside a message land in the leaf its row does.
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
    let at = src_anchor_uuid
        .and_then(stamp_of)
        .or_else(|| stamp_of(src_markdown_uuid));
    time_prefixed(Uuid::new_v5(&DATALIB_ID_NS, recipe.as_bytes()), at)
        .as_hyphenated()
        .to_string()
}

/// Id for one `problems` row — one thing a step could not do to one
/// record — from what produced it and nothing else.
///
/// Not in the recipe: the sample, the JSON path, the stamps, the
/// render version. Two runs of the same code over the same record must
/// mint the same id, and a run after a fix must mint *no* row rather
/// than a different one, so anything that can vary between two runs of
/// the same code stays out. `"problem"` as the leading component keeps
/// it out of every entity id's space.
#[allow(clippy::too_many_arguments)]
pub fn problem_id(
    source_id: &str,
    stage: &str,
    scope_kind: &str,
    scope_key: &str,
    item_uuid: Option<&str>,
    field: Option<&str>,
    reason: &str,
    rule: Option<&str>,
) -> String {
    let recipe = format!(
        "problem\u{1f}{source_id}\u{1f}{stage}\u{1f}{scope_kind}\u{1f}{scope_key}\u{1f}{}\u{1f}{}\u{1f}{reason}\u{1f}{}",
        item_uuid.unwrap_or(""),
        field.unwrap_or(""),
        rule.unwrap_or(""),
    );
    Uuid::new_v5(&DATALIB_ID_NS, recipe.as_bytes())
        .as_hyphenated()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slack(scope: Scope<'_>, kind: &str, key: &str) -> Uuid {
        entity_id(IdNamespace::Slack, scope, kind, key, None)
    }

    fn chatgpt(scope: Scope<'_>, kind: &str, key: &str) -> Uuid {
        entity_id(IdNamespace::Chatgpt, scope, kind, key, None)
    }

    /// Two problems on one record are two ids; the same problem twice
    /// is one.
    #[test]
    fn problem_ids_follow_the_recipe_and_nothing_else() {
        let a = problem_id(
            "slack",
            "grid_row",
            "markdown",
            "md-1",
            Some("u-1"),
            Some("created_at"),
            "coercion_failed",
            None,
        );
        let again = problem_id(
            "slack",
            "grid_row",
            "markdown",
            "md-1",
            Some("u-1"),
            Some("created_at"),
            "coercion_failed",
            None,
        );
        let other_field = problem_id(
            "slack",
            "grid_row",
            "markdown",
            "md-1",
            Some("u-1"),
            Some("modified_at"),
            "coercion_failed",
            None,
        );
        assert_eq!(a, again);
        assert_ne!(a, other_field);
        assert_ne!(
            a,
            entity_id_str(
                IdNamespace::Slack,
                Scope::ProviderGlobal,
                "problem",
                "md-1",
                None
            )
        );
    }

    #[test]
    fn is_deterministic() {
        let a = slack(Scope::Upstream("T123"), "message", "C1:170.5");
        let b = slack(Scope::Upstream("T123"), "message", "C1:170.5");
        assert_eq!(a, b, "ids must be a pure function of their inputs");
    }

    #[test]
    fn is_a_v8_uuid() {
        for at in [None, Some(1_700_000_000_000)] {
            let id = entity_id(
                IdNamespace::Slack,
                Scope::Upstream("T123"),
                "message",
                "C1:170.5",
                at,
            );
            assert_eq!(id.get_version_num(), 8);
            assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
            // The shape `ingested_tng_test` asserts on.
            let s = id.as_hyphenated().to_string();
            assert_eq!(s.len(), 36);
            assert!(s.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "{s}");
        }
    }

    /// The stamp is the leading 48 bits and nothing else: two ids that
    /// differ only in `at` share every hash bit, and the stamp reads
    /// back out exactly. What `ingested_tng_test` relies on to check a
    /// row's uuid against its `created_at_utc` and its backpointer
    /// separately.
    #[test]
    fn the_stamp_is_the_prefix_and_the_hash_is_the_rest() {
        let mint = |at| {
            entity_id(
                IdNamespace::Slack,
                Scope::Upstream("T1"),
                "message",
                "C1#1.1",
                at,
            )
        };
        let ms = 1_700_000_000_123;
        let stamped = mint(Some(ms));
        let bare = mint(None);
        assert_eq!(stamp_of(&stamped.to_string()), Some(ms));
        assert_eq!(stamp_of(&bare.to_string()), None);
        assert_eq!(stamped.as_bytes()[6..], bare.as_bytes()[6..]);
        assert_eq!(&bare.as_bytes()[..6], &[0, 0, 0, 0, 0, 0]);
        assert!(bare.to_string().starts_with("00000000-0000-8"), "{bare}");
        // Text order is time order, which is the whole point.
        assert!(mint(Some(ms - 1)).to_string() < stamped.to_string());
        assert!(stamped.to_string() < mint(Some(ms + 1)).to_string());
    }

    /// Stamps outside the 48-bit range clamp rather than wrap, so a
    /// 1969 header sorts to the left edge instead of the year 10000.
    #[test]
    fn out_of_range_stamps_clamp() {
        let mint = |at| entity_id(IdNamespace::Slack, Scope::ProviderGlobal, "k", "n", at);
        assert_eq!(mint(Some(-5)), mint(Some(0)));
        assert_eq!(mint(Some(i64::MAX)), mint(Some(MAX_STAMP_MS)));
        assert_eq!(
            stamp_of(&mint(Some(MAX_STAMP_MS)).to_string()),
            Some(MAX_STAMP_MS)
        );
    }

    #[test]
    fn stamp_of_a_non_uuid_is_none() {
        assert_eq!(stamp_of("not a uuid"), None);
        assert_eq!(stamp_of(""), None);
    }

    #[test]
    fn identity_carries_what_it_was_minted_from() {
        let id = Identity::mint(
            IdNamespace::Slack,
            Scope::Upstream("T1"),
            "message",
            "C1#1.1".to_string(),
            Some(42),
        );
        assert_eq!(
            id.uuid,
            entity_id_str(
                IdNamespace::Slack,
                Scope::Upstream("T1"),
                "message",
                "C1#1.1",
                Some(42)
            )
        );
        assert_eq!(id.at, Some(42));
        assert_eq!(stamp_of(&id.uuid), Some(42));
    }

    /// Two providers that mint the same natural key must not collide.
    /// Asserted across every pair, so adding a namespace that
    /// duplicates an existing spelling fails here.
    #[test]
    fn namespaces_separate() {
        use strum::VariantArray;
        for (i, &a) in IdNamespace::VARIANTS.iter().enumerate() {
            for &b in &IdNamespace::VARIANTS[i + 1..] {
                assert_ne!(
                    entity_id(a, Scope::ProviderGlobal, "chat", "X", None),
                    entity_id(b, Scope::ProviderGlobal, "chat", "X", None),
                    "{a} and {b} mint the same id",
                );
            }
        }
    }

    #[test]
    fn upstream_scope_separates_two_accounts() {
        // The property that makes configuring one provider twice safe.
        // Signal's `chat_id` is an autoincrement local to a backup
        // file, so two accounts really do both have chat `1`.
        assert_ne!(
            slack(Scope::Upstream("acct-a"), "chat", "1"),
            slack(Scope::Upstream("acct-b"), "chat", "1"),
        );
    }

    /// The bug the old *documented* Slack recipe had: a thread root's
    /// `thread_ts` equals its own `ts`, so keying both on
    /// `{team}:{channel}:{ts}` collides on every single thread root.
    #[test]
    fn entity_kind_separates_thread_root_from_its_message() {
        let key = "C1\u{1f}1700000000.000100";
        assert_ne!(
            slack(Scope::Upstream("T1"), "thread", key),
            slack(Scope::Upstream("T1"), "message", key),
        );
    }

    /// Joining components with a separator that can appear inside a
    /// component makes the split ambiguous. Most of the recipes this
    /// crate replaces joined on `:` or `-`, both of which occur freely
    /// in upstream ids (and in UUIDs).
    #[test]
    fn component_boundaries_are_unambiguous() {
        assert_ne!(
            chatgpt(Scope::ProviderGlobal, "a:b", "c"),
            chatgpt(Scope::ProviderGlobal, "a", "b:c"),
        );
        assert_ne!(
            chatgpt(Scope::ProviderGlobal, "a-b", "c"),
            chatgpt(Scope::ProviderGlobal, "a", "b-c"),
        );
        // The concrete case: `th-{msg_uuid}-{block_index}` is
        // ambiguous between message `M` block `0` and a message
        // literally named `M-0`.
        assert_ne!(
            entity_id(
                IdNamespace::Claude,
                Scope::ProviderGlobal,
                "thinking_block",
                "M\u{1f}0",
                None
            ),
            entity_id(
                IdNamespace::Claude,
                Scope::ProviderGlobal,
                "thinking_block",
                "M-0",
                None
            ),
        );
    }

    /// A source-instance id is scoped to the step id, so two
    /// configured sources of one type stay apart — the property the
    /// bare provider type cannot give.
    #[test]
    fn source_instance_separates_two_configured_sources() {
        assert_ne!(
            slack(Scope::SourceInstance("home-slack"), "page", "timeseries"),
            slack(Scope::SourceInstance("work-slack"), "page", "timeseries"),
        );
    }

    /// A `Content`-scoped id must not collide with an `Upstream` one
    /// carrying the same string, or a content hash reused as an
    /// account label would alias.
    #[test]
    fn scope_variants_do_not_alias() {
        assert_ne!(
            chatgpt(Scope::Content, "document", "abc"),
            chatgpt(Scope::ProviderGlobal, "document", "abc"),
        );
        assert_ne!(
            chatgpt(Scope::Upstream(""), "document", "abc"),
            chatgpt(Scope::ProviderGlobal, "document", "abc"),
        );
        // An upstream account id and a step id are different spaces
        // even when they spell the same thing.
        assert_ne!(
            chatgpt(Scope::Upstream("x"), "k", "n"),
            chatgpt(Scope::SourceInstance("x"), "k", "n"),
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
            entity_id_str(
                IdNamespace::Claude,
                Scope::ProviderGlobal,
                "tool_use",
                &key,
                None
            ),
            entity_id_str(
                IdNamespace::Claude,
                Scope::ProviderGlobal,
                "tool_use",
                "msg-1#toolu_9",
                None
            ),
        );
    }

    #[test]
    fn composite_keys_do_not_alias_across_part_boundaries() {
        assert_ne!(composite_key(&["a", "bc"]), composite_key(&["ab", "c"]),);
    }

    /// An edge sorts beside its source end: the anchor's stamp when the
    /// edge leaves a section, else the document's, else zero.
    #[test]
    fn edges_take_their_source_ends_stamp() {
        let doc = entity_id_str(
            IdNamespace::Slack,
            Scope::ProviderGlobal,
            "thread",
            "t",
            Some(1_000),
        );
        let msg = entity_id_str(
            IdNamespace::Slack,
            Scope::ProviderGlobal,
            "message",
            "m",
            Some(2_000),
        );
        assert_eq!(
            stamp_of(&edge_id(&doc, Some(&msg), "md-b", None, None)),
            Some(2_000)
        );
        assert_eq!(
            stamp_of(&edge_id(&doc, None, "md-b", None, None)),
            Some(1_000)
        );
        assert_eq!(stamp_of(&edge_id("md-a", None, "md-b", None, None)), None);
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
