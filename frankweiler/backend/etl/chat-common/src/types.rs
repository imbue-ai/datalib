//! Normalized chat types. Each provider populates these from its own
//! row model before handing off to [`crate::render::render_all`].
//!
//! These types are *display-shaped*: every field is what the renderer
//! needs to emit markdown or fill a `GridRow`. They are not meant as a
//! lossless representation of the source data — the raw store keeps
//! that. UUIDs are pre-minted by the provider (each has its own v5
//! namespace) so chat-common stays provider-agnostic.

use serde::Serialize;

/// What flavor of item this is. Collapses each provider's richer event
/// taxonomy into three buckets the renderer knows how to lay out.
///
/// Mapping reference:
///
/// | provider  | source value              | NormalizedItem.kind  |
/// |-----------|---------------------------|----------------------|
/// | Beeper    | TEXT, NOTICE              | Text                 |
/// | Beeper    | IMAGE, VIDEO, FILE, AUDIO | Attachment           |
/// | Beeper    | MEMBERSHIP, HIDDEN, *     | System               |
/// | Signal    | StandardMessage           | Text or Attachment   |
/// | Signal    | ChatUpdate, etc.          | System (when shown)  |
/// | WhatsApp  | message_type=0            | Text                 |
/// | WhatsApp  | message_type ∈ {1..media} | Attachment           |
/// | WhatsApp  | message_system rows       | System               |
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
    ///
    /// `None` is legal — the renderer surfaces a "(not yet fetched)"
    /// placeholder. The grid_row's full-text-search column still
    /// gets the caption or file_name.
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
    ///
    /// [`BlobBundle`]: frankweiler_etl::blob_cas::BlobBundle
    pub ref_id: Option<String>,
}

impl NormalizedAttachment {
    /// True when MIME type suggests an inline image. The renderer uses
    /// this to pick `![alt](url)` vs `[alt](url) (size)` markdown.
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
    /// Unix milliseconds when the reaction was sent. Used for
    /// fingerprint stability.
    pub date_ms: i64,
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
    /// Unix milliseconds for the item's effective timestamp.
    pub date_ms: i64,
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
    /// Human-readable label that goes into the page title and the
    /// chat-level grid_row's `conversation_name`. E.g. "Will Riker"
    /// or "Bridge Crew".
    pub display: String,
    /// Optional account scope (Beeper's account_id, slack's
    /// team_id). Surfaced in the chat-level grid_row's `account`
    /// column.
    pub account: Option<String>,
    /// Optional sub-group context (matrix workspace, slack
    /// channel-network). Surfaced in `project`.
    pub project: Option<String>,
    /// Upstream id used by the source app (matrix room id, WhatsApp
    /// JID, signal recipient identifier). Goes into the
    /// chat-level grid_row's `external_id` and the .md frontmatter.
    pub external_id: Option<String>,
    /// Optional public URL for the conversation's source artifact (a
    /// LinkedIn post, a Slack thread permalink, …). Surfaced as the `↗`
    /// link in the page title and the chat-level grid_row's `source_url`.
    /// `None` for backup-based providers with no public per-chat URL —
    /// the default for anything that doesn't set it.
    pub source_url: Option<String>,
    /// Buckets sorted by period_key.
    pub buckets: Vec<NormalizedDoc>,
}
