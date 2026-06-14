//! DDL for the curated `wa_*` mirror tables.
//!
//! Each table holds columns verbatim from msgstore.db's corresponding
//! table, with two changes:
//!
//! 1. The autoincrement `_id` and `*_row_id` columns are replaced
//!    with stable identifiers (see [`crate`] docs for the rekey rules).
//!    The internal `_id` is dropped entirely — it would just be noise
//!    in dolt diffs since it renumbers on phone restore.
//! 2. The (parent's `*_row_id` foreign keys are resolved to the parent's
//!    stable PK columns. For example, `message_text.message_row_id` →
//!    `(chat_jid, key_id, from_me)` matching the parent `wa_message`.
//!
//! Column types match SQLite's source schema (`INTEGER`, `TEXT`,
//! `BLOB`, `REAL`). Doltlite supports the same types so re-typing
//! isn't needed.
//!
//! Schema notes for new readers:
//!
//! - `wa_message.text_data` carries the raw message body for simple
//!   text messages. For rich content (links, replies, media captions,
//!   …) the body lives in `wa_message_text` / `wa_message_media` /
//!   the add-on tables and `text_data` is null.
//! - `wa_message_add_on.parent_chat_jid` etc. are pinned at extract
//!   time by joining the source's `parent_message_row_id` → `message`
//!   → `(chat_jid, key_id, from_me)`. add-ons in WhatsApp model
//!   reactions, polls, pinned-in-chat markers, etc.
//! - `wa_media_files` is keyed by sha256 of the file bytes. Multiple
//!   `wa_message_media` rows can point at the same file (forwards,
//!   re-sends); the registry is the dedup.

/// Names of the data tables in the order they should be wiped before
/// each rebuild. Children before parents so foreign-key-style references
/// aren't briefly dangling (we don't declare FK constraints but the
/// load order still matters for half-aborted reruns).
pub const DATA_TABLES: &[&str] = &[
    "wa_message_add_on_reaction",
    "wa_message_add_on",
    "wa_message_media",
    "wa_message_text",
    "wa_message",
    "wa_chat",
    "wa_jid",
    "wa_media_files",
];

pub const WA_JID_DDL: &str = "CREATE TABLE IF NOT EXISTS wa_jid (
    raw_string TEXT PRIMARY KEY,
    user TEXT NOT NULL,
    server TEXT NOT NULL,
    agent INTEGER,
    device INTEGER,
    type INTEGER
);";

pub const WA_CHAT_DDL: &str = "CREATE TABLE IF NOT EXISTS wa_chat (
    chat_jid TEXT PRIMARY KEY,
    hidden INTEGER,
    subject TEXT,
    created_timestamp INTEGER,
    archived INTEGER,
    sort_timestamp INTEGER,
    mod_tag INTEGER,
    gen REAL,
    spam_detection INTEGER,
    unseen_earliest_message_received_time INTEGER,
    unseen_message_count INTEGER,
    unseen_missed_calls_count INTEGER,
    unseen_row_count INTEGER,
    plaintext_disabled INTEGER,
    vcard_ui_dismissed INTEGER,
    show_group_description INTEGER,
    ephemeral_expiration INTEGER,
    ephemeral_setting_timestamp INTEGER,
    ephemeral_displayed_exemptions INTEGER,
    ephemeral_disappearing_messages_initiator INTEGER,
    unseen_important_message_count INTEGER,
    group_type INTEGER,
    unseen_message_reaction_count INTEGER,
    unseen_comment_message_count INTEGER,
    growth_lock_level INTEGER,
    growth_lock_expiration_ts INTEGER,
    has_new_community_admin_dialog_been_acknowledged INTEGER,
    history_sync_progress INTEGER,
    chat_lock INTEGER,
    chat_origin TEXT,
    participation_status INTEGER,
    account_jid TEXT,
    chat_encryption_state INTEGER,
    group_member_count INTEGER,
    limited_sharing INTEGER,
    limited_sharing_setting_timestamp INTEGER,
    is_contact INTEGER,
    ephemeral_after_read_duration INTEGER,
    business_chat_state INTEGER
);";

pub const WA_MESSAGE_DDL: &str = "CREATE TABLE IF NOT EXISTS wa_message (
    chat_jid TEXT NOT NULL,
    key_id TEXT NOT NULL,
    from_me INTEGER NOT NULL,
    sender_jid TEXT,
    status INTEGER,
    broadcast INTEGER,
    recipient_count INTEGER,
    participant_hash TEXT,
    origination_flags INTEGER,
    origin INTEGER,
    timestamp INTEGER,
    received_timestamp INTEGER,
    receipt_server_timestamp INTEGER,
    message_type INTEGER,
    text_data TEXT,
    starred INTEGER,
    lookup_tables INTEGER,
    message_add_on_flags INTEGER,
    view_mode INTEGER,
    sort_id INTEGER,
    translated_text TEXT,
    server_sts INTEGER,
    PRIMARY KEY (chat_jid, key_id, from_me)
);";

pub const WA_MESSAGE_TEXT_DDL: &str = "CREATE TABLE IF NOT EXISTS wa_message_text (
    chat_jid TEXT NOT NULL,
    key_id TEXT NOT NULL,
    from_me INTEGER NOT NULL,
    description TEXT,
    page_title TEXT,
    url TEXT,
    font_style INTEGER,
    text_color INTEGER,
    background_color INTEGER,
    preview_type INTEGER,
    invite_link_group_type INTEGER,
    counter_abuse_token TEXT,
    fb_experiment_id INTEGER,
    social_media_post_type INTEGER,
    link_media_duration_seconds INTEGER,
    link_end_index INTEGER,
    PRIMARY KEY (chat_jid, key_id, from_me)
);";

pub const WA_MESSAGE_MEDIA_DDL: &str = "CREATE TABLE IF NOT EXISTS wa_message_media (
    chat_jid TEXT NOT NULL,
    key_id TEXT NOT NULL,
    from_me INTEGER NOT NULL,
    autotransfer_retry_enabled INTEGER,
    transferred INTEGER,
    face_x INTEGER,
    face_y INTEGER,
    has_streaming_sidecar INTEGER,
    page_count INTEGER,
    thumbnail_height_width_ratio REAL,
    first_scan_sidecar BLOB,
    first_scan_length INTEGER,
    message_url TEXT,
    media_upload_handle TEXT,
    sticker_flags INTEGER,
    raw_transcription_text TEXT,
    first_viewed_timestamp INTEGER,
    is_animated_sticker INTEGER,
    premium_message INTEGER,
    media_caption TEXT,
    metadata_url TEXT,
    motion_photo_presentation_offset_ms INTEGER,
    qr_url TEXT,
    media_key_domain INTEGER,
    e2ee_media_key BLOB,
    emoji_tags TEXT,
    multicast_id TEXT,
    media_job_uuid TEXT,
    transcoded INTEGER,
    file_path TEXT,
    file_size INTEGER,
    suspicious_content INTEGER,
    trim_from INTEGER,
    trim_to INTEGER,
    media_key BLOB,
    media_key_timestamp INTEGER,
    width INTEGER,
    height INTEGER,
    gif_attribution INTEGER,
    direct_path TEXT,
    mime_type TEXT,
    file_length INTEGER,
    media_name TEXT,
    file_hash TEXT,
    media_duration INTEGER,
    enc_file_hash TEXT,
    partial_media_hash TEXT,
    partial_media_enc_hash TEXT,
    original_file_hash TEXT,
    mute_video INTEGER,
    doodle_id TEXT,
    media_source_type INTEGER,
    accessibility_label TEXT,
    media_transcode_quality INTEGER,
    is_offloaded INTEGER,
    PRIMARY KEY (chat_jid, key_id, from_me)
);";

pub const WA_MESSAGE_ADD_ON_DDL: &str = "CREATE TABLE IF NOT EXISTS wa_message_add_on (
    chat_jid TEXT NOT NULL,
    key_id TEXT NOT NULL,
    from_me INTEGER NOT NULL,
    sender_jid TEXT,
    parent_chat_jid TEXT,
    parent_key_id TEXT,
    parent_from_me INTEGER,
    timestamp INTEGER,
    status INTEGER,
    message_add_on_type INTEGER,
    received_timestamp INTEGER,
    expiry_duration_in_secs INTEGER,
    server_timestamp INTEGER,
    expiry_timestamp INTEGER,
    expiry_type INTEGER,
    PRIMARY KEY (chat_jid, key_id, from_me)
);";

pub const WA_MESSAGE_ADD_ON_REACTION_DDL: &str =
    "CREATE TABLE IF NOT EXISTS wa_message_add_on_reaction (
    chat_jid TEXT NOT NULL,
    key_id TEXT NOT NULL,
    from_me INTEGER NOT NULL,
    reaction TEXT,
    sender_timestamp INTEGER,
    PRIMARY KEY (chat_jid, key_id, from_me)
);";

/// Catalog of plaintext media files from the source backup. Bytes
/// live in the sibling CAS file (managed by `frankweiler_etl::blob_cas`);
/// `wa_media_files.blake3` is the CAS key. `sha256` stays as the
/// upstream identifier (matches `wa_message_media.file_hash`).
pub const WA_MEDIA_FILES_DDL: &str = "CREATE TABLE IF NOT EXISTS wa_media_files (
    sha256 TEXT PRIMARY KEY,
    relative_path TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    mtime_unix INTEGER,
    mime_type TEXT,
    blake3 TEXT NULL,
    CHECK (blake3 IS NULL OR length(blake3) = 64)
);";

/// All DDL statements in dependency-safe creation order.
pub const ALL_DDL: &[&str] = &[
    WA_JID_DDL,
    WA_CHAT_DDL,
    WA_MESSAGE_DDL,
    WA_MESSAGE_TEXT_DDL,
    WA_MESSAGE_MEDIA_DDL,
    WA_MESSAGE_ADD_ON_DDL,
    WA_MESSAGE_ADD_ON_REACTION_DDL,
    WA_MEDIA_FILES_DDL,
];
