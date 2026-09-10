//! DDL for the curated `wa_*` mirror tables.
//!
//! Columns come verbatim from msgstore.db, with two changes: autoincrement
//! `_id` / `*_row_id` columns become stable identifiers (the internal `_id` is
//! dropped outright — it renumbers on phone restore, so it would be noise in
//! every dolt diff), and a parent's `*_row_id` foreign key is resolved to that
//! parent's stable PK columns.
//!
//! Two things a new reader trips over. `wa_message.text_data` holds the body
//! only for simple text messages; for links, replies and media captions it is
//! null and the body lives in `wa_message_text` / `wa_message_media` / the
//! add-on tables. And `wa_media_files` is keyed by blake3, so several
//! `wa_message_media` rows can point at one file — forwards and re-sends dedup
//! through the registry.

use uuid::Uuid;

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

/// Catalog of plaintext media files from the source backup. Bytes live in the
/// sibling CAS, and `blake3` is both this table's key and the CAS key.
///
/// `relative_path` is relative to the **backup root**, so it includes the
/// `Media/` prefix that msgstore puts on `wa_message_media.file_path`. The
/// shipped join is `wa_media_files.relative_path = wa_message_media.file_path`
/// and it only matches because both sides are anchored there.
///
/// Deliberately one digest: that same join is all render needs, so a second
/// hash was stored and never read back — and computing it forced a full
/// re-read of every media file on every run.
pub const WA_MEDIA_FILES_DDL: &str = "CREATE TABLE IF NOT EXISTS wa_media_files (
    blake3 TEXT PRIMARY KEY,
    relative_path TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    mime_type TEXT,
    CHECK (length(blake3) = 64)
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

/// v5 namespace for every UUID this provider mints. The bytes spell
/// `whatsapp:msgstr:` to keep it human-recognizable in dumps.
pub const WHATSAPP_UUID_NS: Uuid = Uuid::from_bytes([
    0x77, 0xa7, 0x59, 0xc0, 0xba, 0xc1, 0x4e, 0x6f, 0x9f, 0x8a, 0x73, 0x16, 0xc7, 0xba, 0xc7, 0xc0,
]);

pub fn whatsapp_chat_uuid(source: &str, chat_jid: &str) -> String {
    Uuid::new_v5(
        &WHATSAPP_UUID_NS,
        format!("whatsapp:chat:{source}:{chat_jid}").as_bytes(),
    )
    .to_string()
}

pub fn whatsapp_message_uuid(source: &str, chat_jid: &str, key_id: &str, from_me: i64) -> String {
    Uuid::new_v5(
        &WHATSAPP_UUID_NS,
        format!("whatsapp:msg:{source}:{chat_jid}:{key_id}:{from_me}").as_bytes(),
    )
    .to_string()
}

pub fn whatsapp_reaction_uuid(source: &str, chat_jid: &str, key_id: &str, from_me: i64) -> String {
    Uuid::new_v5(
        &WHATSAPP_UUID_NS,
        format!("whatsapp:react:{source}:{chat_jid}:{key_id}:{from_me}").as_bytes(),
    )
    .to_string()
}

pub fn whatsapp_markdown_uuid(chat_uuid: &str, period_key: &str) -> String {
    Uuid::new_v5(
        &WHATSAPP_UUID_NS,
        format!("whatsapp:doc:{chat_uuid}:{period_key}").as_bytes(),
    )
    .to_string()
}
