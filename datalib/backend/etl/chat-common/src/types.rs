//! Normalized chat types. Each provider populates these from its own
//! row model before handing off to [`crate::render::render_all`].

use serde::Serialize;

/// What flavor of item this is. Collapses each provider's richer event
/// taxonomy into three buckets the renderer knows how to lay out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ItemKind {
    Text,
    Attachment,
    System,
}

/// A single attachment on an item. Multiple attachments per item are
/// allowed (albums, multi-file messages). The provider materializes
/// the bytes onto disk before calling `render_all`; this struct just
/// carries the relative path the markdown link points at.
#[derive(Debug, Clone, Serialize)]
pub struct NormalizedAttachment {
    /// Path relative to the bucket's `<page_dir>` (e.g.
    /// `"blobs/abc123.jpg"`) that the markdown link / `<img src=…>`
    /// will target. Provider is responsible for putting the bytes
    /// at `<page_dir>/<rel_path>` before render.
    pub rel_path: Option<String>,
    /// User-visible label (file name, image alt text). Falls back to
    /// the basename of `rel_path` when missing.
    pub file_name: Option<String>,
    /// MIME type if known. Used to decide `<img>` vs link-with-icon
    /// in the markdown body.
    pub mime_type: Option<String>,
    /// Byte length if known. Surfaced in the markdown as a human-
    /// readable size.
    pub byte_len: Option<i64>,
    /// Provider's source URL (e.g. WhatsApp's `direct_path`, Beeper's
    /// `source_url`). Surfaced when `rel_path` is missing so a reader
    /// can still trace where the bytes were supposed to come from.
    pub source_url: Option<String>,
    /// Upstream ref_id of the attachment bytes — the same key the
    /// provider hands to its per-chat [`BlobBundle`] in
    /// `parse`. When chat-common's renderer can resolve the ref_id in
    /// the bucket's bundle it writes the bytes under
    /// `<page_dir>/blobs/<short-blake3>.<ext>` and overwrites
    /// `rel_path` so the markdown link points at the materialized
    /// blob. Unknown ref_ids fall through to the "(not yet fetched)"
    /// placeholder.
    pub ref_id: Option<String>,
}

impl NormalizedAttachment {
    pub fn is_image(&self) -> bool {
        self.mime_type
            .as_deref()
            .is_some_and(|m| m.starts_with("image/"))
    }
}

/// One reaction (emoji + reactor) on an item.
#[derive(Debug, Clone, Serialize)]
pub struct NormalizedReaction {
    /// Stable per-reaction UUID minted by the provider. Used as the
    /// anchor on the reaction's rendered span and as the PK of its
    /// own grid_row.
    pub reaction_uuid: String,
    /// Human-readable label for the reactor ("Me" / "Will Riker" / …).
    pub reactor_display: String,
    /// The emoji or short string (`🫡`, `🔥`, …).
    pub emoji: String,
    /// Unix milliseconds when the reaction was sent, or `None` when
    /// upstream gave none. Used for fingerprint stability and for the
    /// reaction row's `when_ts` — see [`NormalizedChatItem::date_ms`]
    /// for why this is an `Option` and what `None` costs downstream.
    pub date_ms: Option<i64>,
    /// What this reaction is upstream, for its grid_row's backpointer
    /// columns. `None` for providers not yet ported onto `datalib_id`.
    pub source_ref: Option<UpstreamRef>,
}

/// One item in a chat doc — text message, attachment-bearing message,
/// or system event. The renderer chooses layout based on
/// `kind` and `attachments`.
#[derive(Debug, Clone, Serialize)]
pub struct NormalizedChatItem {
    /// Stable per-item UUID minted by the provider. Used as the section
    /// anchor (`id="m-{uuid}"`) and the message-level grid_row PK.
    pub message_uuid: String,
    /// Provider-stable identity string used in the fingerprint hash.
    /// Doesn't have to be human-readable.
    pub author_id: String,
    /// Pre-resolved author label ("Me", "Will Riker", "+15551234"). The
    /// provider owns the outgoing/incoming rule and any name lookup.
    pub author_display: String,
    /// Unix milliseconds for the item's effective timestamp, or `None`
    /// when this item has no timestamp at all.
    pub date_ms: Option<i64>,
    /// Optional message body. Text items always carry this; attachment
    /// items use it as the caption; system items use it as the summary.
    pub text: Option<String>,
    pub kind: ItemKind,
    pub attachments: Vec<NormalizedAttachment>,
    pub reactions: Vec<NormalizedReaction>,
    /// Free-form note rendered in italics under the body. Used today
    /// only for system events ("Worf joined", "ephemeral disappearing
    /// messages enabled", …); empty for everything else.
    pub system_note: Option<String>,
    /// Optional public URL for this individual message (Slack message
    /// permalink, …). Surfaced as a `↗` link in the message header and
    /// as the message-level grid_row's `source_url`. Takes precedence
    /// over an attachment-derived URL. `None` for providers with no
    /// per-message URL — the default for anything that doesn't set it.
    pub source_url: Option<String>,
    /// Optional override for this item's grid_row `kind`. Lets a provider
    /// distinguish per-message roles ("User Input", "LLM Response",
    /// "LLM Thinking", "Tool Call") instead of the single
    /// `RenderProfile::message_kind`. `None` → use `message_kind` — the
    /// default for anything that doesn't set it. Grid-only; doesn't change
    /// the markdown layout.
    pub kind_label: Option<String>,
    /// What this item is upstream, for the message-level grid_row's
    /// backpointer columns. `None` for providers not yet ported onto
    /// `datalib_id`.
    pub source_ref: Option<UpstreamRef>,
    /// Machinery rather than conversation — an assistant's tool calls
    /// and their results. The renderer folds each *run* of adjacent
    /// asides into one `<details>`, collapsed by default, so a
    /// transcript reads as what was said with the plumbing tucked
    /// away. `false` for anything a person or an assistant actually
    /// said, which is the default for every provider that doesn't set
    /// it. Layout only: an aside still gets its own anchor, its own
    /// grid_row, and its own place in the fingerprint.
    pub is_aside: bool,
}

/// The upstream's own identity for one chat item, carried through to
/// the message-level grid_row's `upstream_id` /
/// `upstream_entity_kind`.
#[derive(Debug, Clone, Serialize)]
pub struct UpstreamRef {
    /// The upstream's identifier for this item, within the chat's
    /// scope — an Anthropic `message_uuid`, a Slack `ts`, a JMAP
    /// `email_id`. The `natural_key` fed to `datalib_id::entity_id`.
    pub native_id: String,
    /// The `entity_kind` component of the same recipe, in the
    /// upstream's vocabulary (`"message"`, `"tool_use"`,
    /// `"thinking_block"`). Distinct from `kind_label`, which is a
    /// display string for the grid's Kind column: that one may be
    /// reworded freely, this one may not, because `uuid` derives from
    /// it.
    pub entity_kind: String,
}

impl UpstreamRef {
    /// Build from an ids-module `Identity`'s own `entity_kind` and
    /// `natural_key` — never from the expression that was *passed* to
    /// the id function.
    pub fn new(entity_kind: impl Into<String>, native_id: impl Into<String>) -> Self {
        Self {
            native_id: native_id.into(),
            entity_kind: entity_kind.into(),
        }
    }
}

/// Reactions to a message the mirror does not have.
///
/// **Not** "a message in another period" — a provider that buckets by
/// period is expected to file a reaction under its *target's* period,
/// not its own, so a reaction to a March message lands in the March
/// document however late it arrived. What is left over is the case
/// nothing can place: the target event is not in the store at all,
/// because it was never downloaded or it belongs to another
/// conversation. The renderer lists those at the end rather than
/// dropping a real event with nothing to say it happened.
#[derive(Debug, Clone, Serialize)]
pub struct OrphanReactions {
    /// The upstream's id for the message being reacted to. Shown as-is:
    /// there is nothing to link it to.
    pub target_native_id: String,
    pub reactions: Vec<NormalizedReaction>,
}

/// One rendered-markdown bucket: a slice of a chat covering a single
/// period key (`2024-03`, `2024-03-15`, `2024`, or `all`). Drives the
/// .md file and its sidecar.
#[derive(Debug, Clone, Serialize)]
pub struct NormalizedDoc {
    pub period_key: String,
    /// Stable per-bucket UUID minted by the provider (typically v5 over
    /// `(chat_uuid, period_key)`).
    pub markdown_uuid: String,
    pub items: Vec<NormalizedChatItem>,
    /// Empty for every provider that buckets a whole chat into one
    /// document, which is most of them, and empty for a period-bucketed
    /// one whose targets all resolve.
    pub orphan_reactions: Vec<OrphanReactions>,
}

/// A complete chat as exposed to chat-common's renderer.
#[derive(Debug, Clone, Serialize)]
pub struct NormalizedChat {
    /// Provider-local chat id. Goes into the fingerprint hash and the
    /// on-disk path slug.
    pub id: String,
    /// Stable per-chat UUID minted by the provider. Same value across
    /// every bucket of this chat.
    pub chat_uuid: String,
    /// Human-readable label that goes into the chat-level grid_row's
    /// `conversation_name` (and the page title when `title` is unset).
    /// E.g. "Will Riker" or "Bridge Crew".
    pub display: String,
    /// Optional first-class page title (the `<h1>`). When set, it
    /// replaces the derived `"{source_label} · {display}"` heading —
    /// e.g. Slack's "#channel: <root snippet>". `conversation_name`
    /// still comes from `display`. `None` falls back to the derived
    /// heading — the default for anything that doesn't set it.
    pub title: Option<String>,
    /// Whose mirror this is, surfaced in the chat-level grid_row's
    /// `account` column: the login's email where the raw store has it
    /// (see [`crate::account_label`]), never who wrote the thing — a
    /// page someone else created still belongs to the account that
    /// downloaded it. `None` for a source with no login at all.
    pub account: Option<String>,
    /// Who the page is by, where it has a single author (a Claude
    /// project's creator). Surfaced in the chat-level grid_row's
    /// `author`, which is otherwise null — a chat's authors are on its
    /// items.
    pub author: Option<String>,
    /// Optional sub-group context (matrix workspace, slack
    /// channel-network). Surfaced in `project`.
    pub project: Option<String>,
    /// Upstream id used by the source app (matrix room id, WhatsApp
    /// JID, Anthropic conversation UUID, Slack
    /// `{channel_id}:{thread_ts}`). Goes into the chat-level grid_row's
    /// `upstream_id` column and the .md frontmatter.
    pub external_id: Option<String>,
    /// The `Scope::Upstream` value every row in this chat was minted
    /// under — the exact provider-issued string fed to
    /// `datalib_id::entity_id`, stamped into `grid_rows.upstream_scope`.
    /// `None` for chats minted under `ProviderGlobal` or `Content`.
    pub upstream_scope: Option<String>,
    /// Optional public URL for the conversation's source artifact (a
    /// LinkedIn post, a Slack thread permalink, …). Surfaced as the `↗`
    /// link in the page title and the chat-level grid_row's `source_url`.
    /// `None` for backup-based providers with no public per-chat URL —
    /// the default for anything that doesn't set it.
    pub source_url: Option<String>,
    /// Optional owning-org identity, surfaced in every grid_row's
    /// `org_uuid` / `org_name` columns: the organization a login lives
    /// inside — Claude's Anthropic org (a personal plan or a Team
    /// workspace), Slack's workspace. `None` for everything else — the
    /// default.
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    /// Extra path segment between `render_markdown/` and the chat's own
    /// directory, for a source that bridges several upstreams and wants
    /// them apart on disk (Beeper's `<network>/`). `None` — the default
    /// — puts the chat directly under `render_markdown/<chat_uuid>/`.
    pub path_prefix: Option<String>,
    /// Buckets sorted by period_key.
    pub buckets: Vec<NormalizedDoc>,
}
