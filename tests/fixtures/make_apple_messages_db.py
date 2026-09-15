#!/usr/bin/env python3
"""Generate a small, Messages-shaped `chat.db` fixture.

Run as a Bazel genrule
(`//datalib/backend/etl/providers/apple_messages:tng_chat_db`); the
output is a **plain SQLite file**, which is why this is Python and not
Rust (see `make_apple_photos_library.py`).

The table definitions are copied verbatim from a macOS 26 `chat.db`
(`_ClientVersion` 19602), trimmed to the tables the provider reads plus
the daemon bookkeeping `skip_churn` drops. What the tests need from it:

  * a message whose `text` is NULL and whose body is in
    `attributedBody` — Apple's `typedstream` archive of an
    NSAttributedString, which is every message written since Ventura —
    and one old-style row with `text` set and no `attributedBody`,
  * a body longer than 127 bytes, so the archive's two-byte length
    prefix (`0x81 lo hi`) is exercised,
  * an attachment: its message's body is one U+FFFC, and the file is
    named through `message_attachment_join`,
  * tapbacks: rows whose `associated_message_type` is 2000–2005 (add)
    or 3000–3005 (remove), pointing at a message by `p:0/<guid>`,
  * a group chat with a rename event (`item_type = 2`),
  * `date` as nanoseconds since 2001-01-01,
  * two join tables declared UNIQUE but not PRIMARY KEY.

Content is TNG-themed, per this repo's fixture convention.
"""

import os
import sqlite3
import sys

SCHEMA = [
    "CREATE TABLE _SqliteDatabaseProperties (key TEXT, value TEXT, UNIQUE(key))",
    """CREATE TABLE handle (ROWID INTEGER PRIMARY KEY AUTOINCREMENT UNIQUE, id TEXT NOT NULL,
        country TEXT, service TEXT NOT NULL, uncanonicalized_id TEXT, person_centric_id TEXT,
        UNIQUE (id, service))""",
    """CREATE TABLE chat (ROWID INTEGER PRIMARY KEY AUTOINCREMENT, guid TEXT UNIQUE NOT NULL,
        style INTEGER, state INTEGER, account_id TEXT, properties BLOB, chat_identifier TEXT,
        service_name TEXT, room_name TEXT, account_login TEXT, is_archived INTEGER DEFAULT 0,
        last_addressed_handle TEXT, display_name TEXT, group_id TEXT, is_filtered INTEGER DEFAULT 0,
        successful_query INTEGER, engram_id TEXT, server_change_token TEXT,
        ck_sync_state INTEGER DEFAULT 0, original_group_id TEXT,
        last_read_message_timestamp INTEGER DEFAULT 0, cloudkit_record_id TEXT,
        last_addressed_sim_id TEXT, is_blackholed INTEGER DEFAULT 0, syndication_date INTEGER DEFAULT 0,
        syndication_type INTEGER DEFAULT 0, is_recovered INTEGER DEFAULT 0,
        is_deleting_incoming_messages INTEGER DEFAULT 0, is_pending_review INTEGER DEFAULT 0)""",
    """CREATE TABLE message (ROWID INTEGER PRIMARY KEY AUTOINCREMENT, guid TEXT UNIQUE NOT NULL,
        text TEXT, replace INTEGER DEFAULT 0, service_center TEXT, handle_id INTEGER DEFAULT 0,
        subject TEXT, country TEXT, attributedBody BLOB, version INTEGER DEFAULT 0,
        type INTEGER DEFAULT 0, service TEXT, account TEXT, account_guid TEXT, error INTEGER DEFAULT 0,
        date INTEGER, date_read INTEGER, date_delivered INTEGER, is_delivered INTEGER DEFAULT 0,
        is_finished INTEGER DEFAULT 0, is_emote INTEGER DEFAULT 0, is_from_me INTEGER DEFAULT 0,
        is_empty INTEGER DEFAULT 0, is_delayed INTEGER DEFAULT 0, is_auto_reply INTEGER DEFAULT 0,
        is_prepared INTEGER DEFAULT 0, is_read INTEGER DEFAULT 0, is_system_message INTEGER DEFAULT 0,
        is_sent INTEGER DEFAULT 0, has_dd_results INTEGER DEFAULT 0, is_service_message INTEGER DEFAULT 0,
        is_forward INTEGER DEFAULT 0, was_downgraded INTEGER DEFAULT 0, is_archive INTEGER DEFAULT 0,
        cache_has_attachments INTEGER DEFAULT 0, cache_roomnames TEXT, was_data_detected INTEGER DEFAULT 0,
        was_deduplicated INTEGER DEFAULT 0, is_audio_message INTEGER DEFAULT 0, is_played INTEGER DEFAULT 0,
        date_played INTEGER, item_type INTEGER DEFAULT 0, other_handle INTEGER DEFAULT 0, group_title TEXT,
        group_action_type INTEGER DEFAULT 0, share_status INTEGER DEFAULT 0, share_direction INTEGER DEFAULT 0,
        is_expirable INTEGER DEFAULT 0, expire_state INTEGER DEFAULT 0, message_action_type INTEGER DEFAULT 0,
        message_source INTEGER DEFAULT 0, associated_message_guid TEXT,
        associated_message_type INTEGER DEFAULT 0, balloon_bundle_id TEXT, payload_data BLOB,
        expressive_send_style_id TEXT, associated_message_range_location INTEGER DEFAULT 0,
        associated_message_range_length INTEGER DEFAULT 0, time_expressive_send_played INTEGER,
        message_summary_info BLOB, ck_sync_state INTEGER DEFAULT 0, ck_record_id TEXT,
        ck_record_change_tag TEXT, destination_caller_id TEXT, is_corrupt INTEGER DEFAULT 0,
        reply_to_guid TEXT, sort_id INTEGER, is_spam INTEGER DEFAULT 0, has_unseen_mention INTEGER DEFAULT 0,
        thread_originator_guid TEXT, thread_originator_part TEXT, syndication_ranges TEXT,
        synced_syndication_ranges TEXT, was_delivered_quietly INTEGER DEFAULT 0,
        did_notify_recipient INTEGER DEFAULT 0, date_retracted INTEGER, date_edited INTEGER,
        was_detonated INTEGER DEFAULT 0, part_count INTEGER, is_stewie INTEGER DEFAULT 0,
        is_sos INTEGER DEFAULT 0, is_critical INTEGER DEFAULT 0, bia_reference_id TEXT,
        is_kt_verified INTEGER DEFAULT 0, fallback_hash TEXT, associated_message_emoji TEXT,
        is_pending_satellite_send INTEGER DEFAULT 0, needs_relay INTEGER DEFAULT 0,
        schedule_type INTEGER DEFAULT 0, schedule_state INTEGER DEFAULT 0,
        sent_or_received_off_grid INTEGER DEFAULT 0, date_recovered INTEGER DEFAULT 0,
        is_time_sensitive INTEGER DEFAULT 0, ck_chat_id TEXT, index_state INTEGER DEFAULT 0)""",
    """CREATE TABLE attachment (ROWID INTEGER PRIMARY KEY AUTOINCREMENT, guid TEXT UNIQUE NOT NULL,
        created_date INTEGER DEFAULT 0, start_date INTEGER DEFAULT 0, filename TEXT, uti TEXT,
        mime_type TEXT, transfer_state INTEGER DEFAULT 0, is_outgoing INTEGER DEFAULT 0, user_info BLOB,
        transfer_name TEXT, total_bytes INTEGER DEFAULT 0, is_sticker INTEGER DEFAULT 0,
        sticker_user_info BLOB, attribution_info BLOB, hide_attachment INTEGER DEFAULT 0,
        ck_sync_state INTEGER DEFAULT 0, ck_server_change_token_blob BLOB, ck_record_id TEXT,
        original_guid TEXT UNIQUE NOT NULL, is_commsafety_sensitive INTEGER DEFAULT 0,
        emoji_image_content_identifier TEXT, emoji_image_short_description TEXT,
        preview_generation_state INTEGER DEFAULT 0)""",
    """CREATE TABLE chat_message_join (chat_id INTEGER REFERENCES chat (ROWID) ON DELETE CASCADE,
        message_id INTEGER REFERENCES message (ROWID) ON DELETE CASCADE, message_date INTEGER DEFAULT 0,
        index_state INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (chat_id, message_id))""",
    """CREATE TABLE chat_handle_join (chat_id INTEGER REFERENCES chat (ROWID) ON DELETE CASCADE,
        handle_id INTEGER REFERENCES handle (ROWID) ON DELETE CASCADE, UNIQUE(chat_id, handle_id))""",
    """CREATE TABLE message_attachment_join (message_id INTEGER REFERENCES message (ROWID) ON DELETE CASCADE,
        attachment_id INTEGER REFERENCES attachment (ROWID) ON DELETE CASCADE,
        UNIQUE(message_id, attachment_id))""",
    # The bookkeeping the daemons never stop writing.
    """CREATE TABLE kvtable (ROWID INTEGER PRIMARY KEY AUTOINCREMENT UNIQUE, key TEXT UNIQUE NOT NULL,
        value BLOB NOT NULL)""",
    """CREATE TABLE message_processing_task (ROWID INTEGER PRIMARY KEY AUTOINCREMENT UNIQUE,
        guid TEXT UNIQUE NOT NULL, task_flags INTEGER NOT NULL, reasons INTEGER NOT NULL)""",
    """CREATE TABLE sync_deleted_messages (ROWID INTEGER PRIMARY KEY AUTOINCREMENT UNIQUE,
        guid TEXT NOT NULL, recordID TEXT)""",
    """CREATE TABLE index_state_metrics (id INTEGER UNIQUE DEFAULT 1, pending_count INTEGER DEFAULT 0,
        donated_count INTEGER DEFAULT 0, redonation_count INTEGER DEFAULT 0, UNIQUE (id))""",
]

# 2026-04-05T09:15:00Z, as nanoseconds since the Apple epoch, 2001-01-01.
# Not the fixture convention's 24th century: in nanoseconds that is past
# what an INTEGER column holds, and a real `chat.db` never carries one.
APPLE_EPOCH = 978_307_200
T0 = (1_775_380_500 - APPLE_EPOCH) * 1_000_000_000
MINUTE = 60 * 1_000_000_000

ACCOUNT = "E:picard@enterprise.example"


def ts_int(n: int) -> bytes:
    """A typedstream integer: one byte below 0x80, else a tag and LE bytes."""
    if n < 0x80:
        return bytes([n])
    if n < 0x10000:
        return b"\x81" + n.to_bytes(2, "little")
    return b"\x82" + n.to_bytes(4, "little")


def attributed_body(text: str) -> bytes:
    """The archive Messages writes: an NSAttributedString whose string is
    `text` and whose one attribute run marks it as message part 0."""
    body = text.encode("utf-8")
    return (
        b"\x04\x0bstreamtyped\x81\xe8\x03\x84\x01@\x84\x84\x84\x12NSAttributedString\x00"
        b"\x84\x84\x08NSObject\x00\x85\x92\x84\x84\x84\x08NSString\x01\x94\x84\x01+"
        + ts_int(len(body))
        + body
        + b"\x86\x84\x02iI\x01"
        + ts_int(len(text))
        + b"\x92\x84\x84\x84\x0cNSDictionary\x00\x94\x84\x01i\x01\x92\x84\x96\x96\x1d"
        b"__kIMMessagePartAttributeName\x86\x92\x84\x84\x84\x08NSNumber\x00\x84\x84\x07"
        b"NSValue\x00\x94\x84\x01*\x84\x99\x99\x00\x86\x86\x86"
    )


# (ROWID, id)
HANDLES = [(1, "+14155550142"), (2, "+14155550187")]

# (ROWID, guid, chat_identifier, display_name, style)
CHATS = [
    (1, "iMessage;-;+14155550142", "+14155550142", None, 45),
    (2, "iMessage;+;chat240603120915", "chat240603120915", "Bridge crew", 43),
]
CHAT_HANDLES = [(1, 1), (2, 1), (2, 2)]

GUID = {
    1: "A1B2C3D4-0001-4000-8000-000000000001",
    2: "A1B2C3D4-0002-4000-8000-000000000002",
    3: "A1B2C3D4-0003-4000-8000-000000000003",
    4: "A1B2C3D4-0004-4000-8000-000000000004",
    5: "A1B2C3D4-0005-4000-8000-000000000005",
    6: "A1B2C3D4-0006-4000-8000-000000000006",
    7: "A1B2C3D4-0007-4000-8000-000000000007",
    8: "A1B2C3D4-0008-4000-8000-000000000008",
    9: "A1B2C3D4-0009-4000-8000-000000000009",
    10: "A1B2C3D4-0010-4000-8000-000000000010",
}

LONG_BODY = (
    "Captain's log, supplemental. The away team reports that the settlement on "
    "the third planet is deserted, but the power grid is still running; Mr. Data "
    "estimates it was abandoned no more than a week ago."
)

# (rowid, chat, handle, from_me, minutes after T0, `text` column, body,
# attachment rowid, tapback (type, target rowid), new group name)
MESSAGES = [
    (1, 1, 1, 0, 0, None, "Captain, the away team is ready.", None, None, None),
    # An old-style row: `text` set, no `attributedBody`.
    (2, 1, 0, 1, 1, "Make it so.", None, None, None, None),
    (3, 1, 1, 0, 5, None, "\ufffc", 1, None, None),
    # Picard loves the picture.
    (4, 1, 0, 1, 6, None, "Loved an image", None, (2000, 3), None),
    (5, 2, 2, 0, 30, None, "Query: shall I recalibrate the sensors?", None, None, None),
    (6, 2, 0, 1, 31, None, "Yes, Data.", None, None, None),
    # Riker likes Data's question, then takes it back.
    (7, 2, 1, 0, 32, None, "Liked a message", None, (2001, 5), None),
    (8, 2, 1, 0, 33, None, "Removed a like", None, (3001, 5), None),
    (9, 2, 0, 1, 40, None, None, None, None, "Bridge crew"),
    (10, 1, 0, 1, 60 * 24 * 40, None, LONG_BODY, None, None, None),
]

ATTACHMENT = (
    1,
    "F0E1D2C3-0001-4000-8000-000000000001",
    "~/Library/Messages/Attachments/0a/10/F0E1D2C3-0001-4000-8000-000000000001/IMG_1701.jpeg",
    "public.jpeg",
    "image/jpeg",
    "IMG_1701.jpeg",
    130_860,
)


def main(out_path: str) -> None:
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    con = sqlite3.connect(out_path)
    try:
        for stmt in SCHEMA:
            con.execute(stmt)
        con.executemany(
            "INSERT INTO handle (ROWID, id, country, service) VALUES (?, ?, 'US', 'iMessage')",
            HANDLES,
        )
        con.executemany(
            "INSERT INTO chat (ROWID, guid, style, state, account_id, chat_identifier, "
            " service_name, account_login, display_name) "
            "VALUES (?, ?, ?, 3, '1E22DDA8-6BF1-470F-8320-780789C67D13', ?, 'iMessage', ?, ?)",
            [(r, g, style, ident, ACCOUNT, name) for r, g, ident, name, style in CHATS],
        )
        con.executemany(
            "INSERT INTO chat_handle_join (chat_id, handle_id) VALUES (?, ?)",
            CHAT_HANDLES,
        )
        for (
            rowid,
            chat,
            handle,
            from_me,
            at,
            text,
            body,
            attachment,
            tapback,
            rename,
        ) in MESSAGES:
            date = T0 + at * MINUTE
            con.execute(
                "INSERT INTO message (ROWID, guid, text, handle_id, attributedBody, service, "
                " account, date, is_from_me, is_read, cache_has_attachments, item_type, "
                " group_title, associated_message_guid, associated_message_type) "
                "VALUES (?, ?, ?, ?, ?, 'iMessage', ?, ?, ?, 1, ?, ?, ?, ?, ?)",
                (
                    rowid,
                    GUID[rowid],
                    text,
                    handle,
                    attributed_body(body) if body is not None else None,
                    ACCOUNT,
                    date,
                    from_me,
                    1 if attachment else 0,
                    2 if rename else 0,
                    rename,
                    f"p:0/{GUID[tapback[1]]}" if tapback else None,
                    tapback[0] if tapback else 0,
                ),
            )
            con.execute(
                "INSERT INTO chat_message_join (chat_id, message_id, message_date) "
                "VALUES (?, ?, ?)",
                (chat, rowid, date),
            )
            if attachment:
                con.execute(
                    "INSERT INTO message_attachment_join (message_id, attachment_id) "
                    "VALUES (?, ?)",
                    (rowid, attachment),
                )
        rowid, guid, filename, uti, mime, name, size = ATTACHMENT
        con.execute(
            "INSERT INTO attachment (ROWID, guid, created_date, filename, uti, mime_type, "
            " transfer_name, total_bytes, original_guid) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (rowid, guid, T0 + 5 * MINUTE, filename, uti, mime, name, size, guid),
        )

        con.executemany(
            "INSERT INTO _SqliteDatabaseProperties (key, value) VALUES (?, ?)",
            [
                ("counter_in_all", "5"),
                ("counter_out_all", "5"),
                ("_ClientVersion", "19602"),
            ],
        )
        con.executemany(
            "INSERT INTO kvtable (key, value) VALUES (?, ?)",
            [("chatVersion", b"\x01"), ("iMessage", b"\x01")],
        )
        con.executemany(
            "INSERT INTO message_processing_task (guid, task_flags, reasons) VALUES (?, 4, 0)",
            [(GUID[5],), (GUID[6],)],
        )
        con.execute(
            "INSERT INTO sync_deleted_messages (guid, recordID) VALUES (?, ?)",
            ("A1B2C3D4-0099-4000-8000-000000000099", "rec99"),
        )
        con.execute("INSERT INTO index_state_metrics (id, pending_count) VALUES (1, 2)")
        con.commit()
    finally:
        con.close()


if __name__ == "__main__":
    main(sys.argv[1])
