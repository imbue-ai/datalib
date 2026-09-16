// The provider-agnostic union table behind the grid. Every searchable
// entity in the system — conversations, messages, blocks, Slack messages,
// … — emits one row here, and the grid backend renders it with a single
// query and no per-provider branches.
//
// This struct is the source of truth for column names and types;
// `#[derive(PortableTable)]` derives the DDL from it. Per-provider tables
// stay authoritative for raw payloads; this is the denormalized projection.
//
// How each provider fills each column is in `docs/dev/grid_rows.md`.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// One row in the grid_rows table. Provider render steps emit one or more
/// per source entity; the grid backend and UI read them as a single union.
#[derive(Debug, Clone, Serialize, Deserialize, PortableTable, sqlx::FromRow)]
#[portable_table(table = "grid_rows", primary_key = "uuid")]
pub struct GridRow {
    /// Stable and globally unique. Must be deterministic from the source
    /// entity, so re-ingest is idempotent.
    #[col(sql = "VARCHAR(96)")]
    pub uuid: String,
    /// Which provider this row came from, and so which per-provider table
    /// holds its raw payload.
    #[col(sql = "VARCHAR(32)")]
    pub provider: String,
    /// Display label for the Kind column; drives the row-type filter and the
    /// icon. Not the same thing as `upstream_entity_kind`.
    #[col(sql = "VARCHAR(32)")]
    pub kind: String,
    /// Human-friendly provider name for the Source column.
    #[col(sql = "VARCHAR(32)")]
    pub source_label: String,
    /// When the thing this row describes came into being, as the source
    /// wrote it: ISO-8601 with explicit offset. A message's own stamp; for
    /// a document row (`is_document`) the earliest moment in it — the
    /// first message of a thread, a PR's `created_at`, a page's
    /// `created_time`. Synthesized for blocks and messages with no stamp
    /// of their own by bumping microseconds off the parent, so
    /// within-conversation order stays stable. The global sort key, and
    /// what `before:`/`after:` filter on.
    ///
    /// Null means the source has no timestamp — some entities aren't
    /// event-shaped, and we never fabricate one. Null rows are excluded by
    /// `before:`/`after:`.
    ///
    /// `created_at_utc` and `created_offset` are derived from this at index
    /// time and live in the DB but not on this struct. The grid sorts and
    /// filters on `created_at_utc`, where one zone and a fixed width make
    /// lexical order match chronological order; `created_offset` recovers
    /// the local wall-clock for display. This column itself stays as the
    /// source wrote it — it is the record's stamp — which is why it is
    /// not `created_at_utc` + `tz_offset` like the stamps we mint
    /// (AGENTS.md, "Timestamp convention").
    #[col(sql = "VARCHAR(40)")]
    #[derived(name = "created_at_utc", sql = "VARCHAR(40)")]
    #[derived(name = "created_offset", sql = "VARCHAR(8)")]
    pub created_at: Option<String>,
    /// When the thing this row describes last changed, as the source wrote
    /// it. For a document row the latest moment in it — the last message
    /// of a thread, a PR's `updated_at`, a page's `last_edited_time`. For
    /// an inner row, the edit stamp where the source keeps one, else null:
    /// null means "not known to have changed since `created_at`", never a
    /// copy of it. Same form and the same derived twins as `created_at`.
    #[col(sql = "VARCHAR(40)")]
    #[derived(name = "modified_at_utc", sql = "VARCHAR(40)")]
    #[derived(name = "modified_offset", sql = "VARCHAR(8)")]
    pub modified_at: Option<String>,
    /// True on the one row per rendered markdown document that *is* that
    /// document — the thread, the conversation, the PR, the page, the PDF
    /// — and false on every row inside it. Every row carries a
    /// `markdown_uuid`, so this is not "has a document": it is "opening
    /// this row opens a whole document rather than a place in one". A
    /// Browse of a source starts on these rows (`is:document` in the
    /// search bar), and the render store refuses a document with any
    /// number of them other than one.
    #[col(sql = "INTEGER")]
    pub is_document: bool,
    /// Display name of the author: the model slug for LLM responses, the
    /// account for user input, the real name for Slack.
    #[col(sql = "VARCHAR(255)")]
    pub author: Option<String>,
    /// Whose mirror this row came from — the login's email where the
    /// source stores one, else its name, else the provider's own id.
    /// Null for a source with no login (a PDF folder, an address book).
    /// Drives the `account:` filter.
    #[col(sql = "VARCHAR(96)")]
    pub account: Option<String>,
    /// Claude project name, or the repo full name for github/gitlab. Null
    /// for providers with no notion of a project.
    #[col(sql = "VARCHAR(96)")]
    pub project: Option<String>,
    /// The organization a login lives inside: Claude's Anthropic org,
    /// which disambiguates conversations that share a login but live in
    /// different orgs (a personal Max plan vs a Team workspace), or
    /// Slack's workspace (`T…`). Opaque and stable; pair with `org_name`
    /// for display. Null for every other provider.
    #[col(sql = "VARCHAR(96)")]
    pub org_uuid: Option<String>,
    /// Display name for `org_uuid`, shown in the Org column.
    #[col(sql = "VARCHAR(255)")]
    pub org_name: Option<String>,
    /// Slack channel, or a chat's display name (group subject, 1:1
    /// counterpart). Null for providers with no channel concept.
    #[col(sql = "VARCHAR(255)")]
    pub channel: Option<String>,
    /// Title of the parent conversation, carried onto every child row so a
    /// grid row stands alone without a join. For thread-level rows this
    /// duplicates the row's own title.
    #[col(sql = "TEXT")]
    pub conversation_name: Option<String>,
    /// The parent thread, so the preview pane knows what to open. Equals
    /// `uuid` for thread-level rows.
    #[col(sql = "VARCHAR(96)")]
    pub conversation_uuid: String,
    /// Zero-based position within the conversation, in the order the QMD
    /// renders messages. Null for thread-level rows.
    #[col(sql = "INT")]
    pub message_index: Option<i64>,
    /// Preview-pane path for the whole thread: `/chat/{conversation_uuid}`.
    #[col(sql = "VARCHAR(255)")]
    pub entire_chat: String,
    /// The full searchable body. Stored whole rather than pre-truncated,
    /// because the snippet is computed against the user's needle at query
    /// time.
    #[col(sql = "LONGTEXT")]
    pub text: String,
    /// `slack://channel?team=…&id=…&message=…` (or the https equivalent),
    /// behind the 'Open in Slack' context-menu item. Slack rows only.
    #[col(sql = "VARCHAR(512)")]
    pub slack_link: Option<String>,
    /// The rendered Markdown file for this row's thread, relative to the
    /// data root, so the preview pane can load it with no glob and no
    /// frontmatter scan. Set on every row; child rows inherit their parent's.
    ///
    /// Shape, and the byte-equality invariant against `markdowns.md_path`,
    /// are in `docs/dev/grid_rows.md`. Getting the prefix wrong silently
    /// drops the row from free-text results.
    ///
    /// Prefer `markdowns.md_path` (via `markdown_uuid`) when you want the
    /// file itself: same path, one writer, and the column `/api/chat`
    /// resolves through.
    #[col(sql = "VARCHAR(512)")]
    pub qmd_path: Option<String>,
    /// Canonical link back to the provider's own web UI. Null for providers
    /// with no stable public link.
    #[col(sql = "VARCHAR(1024)")]
    pub source_url: Option<String>,
    /// The commit this row is anchored to — head SHA at ingest for a PR/MR,
    /// the reviewed commit for a diff comment.
    #[col(sql = "VARCHAR(64)")]
    pub git_sha: Option<String>,
    /// The upstream's own identifier for this entity within
    /// `upstream_scope`: the backpointer half of the id pair. `uuid` is a
    /// one-way hash, so this preserves what it was minted from and lets a
    /// row be taken back to the provider's API.
    ///
    /// With `upstream_entity_kind` and `upstream_scope` this is the whole
    /// `entity_id` recipe minus the provider, so
    /// `entity_id(provider, scope, upstream_entity_kind, upstream_id) == uuid`
    /// holds by construction for a ported provider — which makes the
    /// backpointer verifiable rather than decorative.
    ///
    /// Null for a provider not yet ported onto `datalib_id`.
    #[col(sql = "VARCHAR(128)")]
    pub upstream_id: Option<String>,
    /// What sort of upstream thing this row is, in the provider's own
    /// vocabulary (`conversation`, `message`, `thinking_block`, `pr`,
    /// `page`). The `entity_kind` component of the `uuid` recipe.
    ///
    /// Distinct from `kind`, which is a display label: two providers can
    /// share a display label while meaning different upstream things, and a
    /// label can be reworded freely. This cannot — the id depends on it, and
    /// without it a bare `12345` is ambiguous between a GitHub review and a
    /// review comment.
    ///
    /// Null for a provider not yet ported onto `datalib_id`.
    #[col(sql = "VARCHAR(32)")]
    pub upstream_entity_kind: Option<String>,
    /// The upstream account / workspace / organization `upstream_id` is
    /// unique within: the `Scope::Upstream` value fed to `entity_id`. NULL
    /// means `Scope::ProviderGlobal` or `Scope::Content`, where the natural
    /// key needs no further scoping.
    ///
    /// Prefer a provider-issued value (Anthropic `org_uuid`, Slack
    /// `team_id`, JMAP `account_id`) over our own step id: an
    /// upstream-scoped id is a function of the data, so a fresh data root
    /// re-ingesting the same content reproduces it.
    ///
    /// Overlaps `account` in spirit but not contract — `account` is a
    /// display value and may be prettified; this is the exact opaque string
    /// the id was derived from and must not be.
    #[col(sql = "VARCHAR(96)")]
    pub upstream_scope: Option<String>,
    /// Notion only. The page this row lives in, so the grid can filter every
    /// row in a document. Equals `uuid` for page rows.
    #[col(sql = "VARCHAR(96)")]
    pub notion_page_uuid: Option<String>,
    /// Notion only. The block this row is anchored to — the heading block,
    /// or the block a discussion hangs off. Null for page-level rows.
    #[col(sql = "VARCHAR(96)")]
    pub notion_block_uuid: Option<String>,
    /// FK into `markdowns`: every rendered `.md` gets a row there, and every
    /// grid row inside that file points at it. Many-to-one, because a
    /// provider may shard one conversation across files (beeper renders one
    /// per period). The addressing primitive behind `/api/chat/{uuid}`, and
    /// what drives incremental re-render.
    ///
    /// Nullable until every renderer populates it.
    #[col(sql = "VARCHAR(96)")]
    pub markdown_uuid: Option<String>,
    /// How many bytes the thing this row describes occupies. A file's
    /// size on disk, a store's size, a chat message's body in UTF-8, a
    /// conversation's messages summed. NULL when the row describes
    /// something with no meaningful size.
    ///
    /// Which of those it is depends on `kind`, and the table in
    /// `docs/dev/grid_rows.md` says which. Never mix them under one
    /// kind: a store row measures the file, never a sum of its fields.
    #[col(sql = "BIGINT")]
    pub byte_size: Option<i64>,
    /// How many things this row counts: rows in a table, files under a
    /// directory, messages in a conversation. 1 when the row is one of
    /// the things a collection counts (a message); NULL when it is a
    /// single thing that is not counted by anything (a reaction).
    ///
    /// Deliberately unitless — what is being counted is `kind`'s job to
    /// say, not this column's.
    #[col(sql = "BIGINT")]
    pub item_count: Option<i64>,
}
