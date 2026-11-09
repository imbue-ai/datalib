#!/usr/bin/env python3
"""Generate a small, Mail-shaped `~/Library/Mail` folder, as one `.tar`.

Run as a Bazel genrule
(`//datalib/backend/etl/providers/email:tng_apple_mail`); the Envelope
Index in it is a **plain SQLite file**, which is why this is Python and not
Rust (see `make_apple_photos_library.py`). It is packed as a tar because
Mail's paths (`Envelope Index`, `[Gmail].mbox/All Mail.mbox`) are not paths
a Bazel output may have.

The table definitions are copied verbatim from a macOS 26 Envelope Index
(`V10`), trimmed to the tables the Apple Mail mode reads and a few beside
them. What the tests need from it (`docs/dev/email_download_modes.md` §6):

  * two accounts with mail: a Gmail one, whose files are all under
    `[Gmail]/All Mail` and whose other labels are `labels` rows, and a
    folder-based (Fastmail) one, where each folder holds its own files and
    a message in two folders is a file in each;
  * a third account with a mailbox and no mail (On My Mac's Outbox);
  * one message in both accounts;
  * a `.partial.emlx` whose report Mail kept beside it and whose
    invitation it did not, and one whose inline image (by `cid:`) and
    attached photo it kept;
  * a message the index has and no file holds, and one marked deleted;
  * a nested label and a nested folder, both percent-encoded in the URL;
  * a file under `Data/<n>/Messages/`, where Mail moves rowids past 1000;
  * a message only in `Envelope Index-wal`: the index is in WAL mode and
    the newest message was never checkpointed, which is how Mail leaves it
    while running. A reader that drops the WAL loses that message.

Content is TNG-themed, per this repo's fixture convention.
"""

import base64
import io
import os
import shutil
import sqlite3
import sys
import tarfile
import tempfile

SCHEMA = [
    """CREATE TABLE mailboxes (ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
url TEXT COLLATE BINARY NOT NULL,
total_count INTEGER NOT NULL DEFAULT 0,
unread_count INTEGER NOT NULL DEFAULT 0,
deleted_count INTEGER NOT NULL DEFAULT 0,
unseen_count INTEGER NOT NULL DEFAULT 0,
unread_count_adjusted_for_duplicates INTEGER NOT NULL DEFAULT 0,
change_identifier TEXT COLLATE BINARY,
source INTEGER,
alleged_change_identifier TEXT COLLATE BINARY,
UNIQUE(url) ON CONFLICT ABORT)""",
    """CREATE TABLE messages (ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
message_id INTEGER NOT NULL DEFAULT 0,
global_message_id INTEGER NOT NULL,
remote_id INTEGER,
document_id TEXT COLLATE BINARY,
sender INTEGER,
subject_prefix TEXT COLLATE BINARY,
subject INTEGER NOT NULL,
summary INTEGER,
date_sent INTEGER,
date_received INTEGER,
mailbox INTEGER NOT NULL,
remote_mailbox INTEGER,
flags INTEGER NOT NULL DEFAULT 0,
read INTEGER NOT NULL DEFAULT 0,
flagged INTEGER NOT NULL DEFAULT 0,
deleted INTEGER NOT NULL DEFAULT 0,
size INTEGER NOT NULL DEFAULT 0,
conversation_id INTEGER NOT NULL DEFAULT 0,
date_last_viewed INTEGER,
list_id_hash INTEGER,
unsubscribe_type INTEGER,
searchable_message INTEGER,
brand_indicator INTEGER,
display_date INTEGER,
flag_color INTEGER,
is_urgent INTEGER NOT NULL DEFAULT 0,
color TEXT COLLATE BINARY,
type INTEGER,
fuzzy_ancestor INTEGER,
automated_conversation INTEGER DEFAULT 0,
root_status INTEGER DEFAULT -1)""",
    """CREATE TABLE message_global_data (ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
message_id INTEGER,
follow_up_start_date INTEGER,
follow_up_end_date INTEGER,
follow_up_jsonstringformodelevaluationforsuggestions TEXT COLLATE BINARY,
download_state INTEGER NOT NULL DEFAULT 0,
read_later_date INTEGER,
send_later_date INTEGER,
validation_state INTEGER NOT NULL DEFAULT 0,
model_category INTEGER,
model_subcategory INTEGER,
category_model_version INTEGER,
category_is_temporary INTEGER,
model_analytics TEXT COLLATE BINARY,
model_high_impact INTEGER NOT NULL DEFAULT 0,
generated_summary INTEGER,
urgent INTEGER,
message_id_header TEXT COLLATE BINARY,
UNIQUE(message_id) ON CONFLICT ABORT)""",
    """CREATE TABLE server_messages (ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
message INTEGER REFERENCES messages(ROWID) ON DELETE SET NULL,
mailbox INTEGER NOT NULL REFERENCES mailboxes(ROWID) ON DELETE CASCADE,
sequence_identifier INTEGER,
read INTEGER NOT NULL,
deleted INTEGER NOT NULL,
replied INTEGER NOT NULL,
flagged INTEGER NOT NULL,
draft INTEGER NOT NULL,
forwarded INTEGER NOT NULL,
redirected INTEGER NOT NULL,
junk_level_set_by_user INTEGER NOT NULL,
junk_level INTEGER NOT NULL,
flag_color INTEGER NOT NULL,
remote_id INTEGER NOT NULL,
UNIQUE(mailbox, remote_id) ON CONFLICT ABORT)""",
    """CREATE TABLE subjects (ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
subject TEXT COLLATE RTRIM NOT NULL,
UNIQUE(subject) ON CONFLICT ABORT)""",
    """CREATE TABLE addresses (ROWID INTEGER PRIMARY KEY AUTOINCREMENT,
address TEXT COLLATE NOCASE NOT NULL,
comment TEXT COLLATE BINARY NOT NULL,
UNIQUE(address, comment) ON CONFLICT ABORT)""",
    """CREATE TABLE labels (message_id INTEGER REFERENCES messages(ROWID) ON DELETE CASCADE, mailbox_id INTEGER REFERENCES mailboxes(ROWID) ON DELETE CASCADE, PRIMARY KEY(message_id, mailbox_id)) WITHOUT ROWID""",
    """CREATE TABLE properties (ROWID INTEGER PRIMARY KEY, key, value, UNIQUE (key))""",
]

GMAIL = "1F0B8A52-0000-4000-8000-00000000E1D0"
FASTMAIL = "5D3C9E70-0000-4000-8000-0000000000FA"
LOCAL = "2C7E0D11-0000-4000-8000-0000000000AA"
STORE = "6A1D2C3B-0000-4000-8000-0000000000BB"

# ROWID → URL, as Mail spells them: the path percent-encoded.
MAILBOXES = [
    (1, f"local://{LOCAL}/Outbox"),
    (2, f"imap://{GMAIL}/INBOX"),
    (3, f"imap://{GMAIL}/%5BGmail%5D/All%20Mail"),
    (4, f"imap://{GMAIL}/%5BGmail%5D/Sent%20Mail"),
    (5, f"imap://{GMAIL}/Away%20Team"),
    (6, f"imap://{GMAIL}/Away%20Team/Reports"),
    (10, f"imap://{FASTMAIL}/INBOX"),
    (11, f"imap://{FASTMAIL}/Sent"),
    (12, f"imap://{FASTMAIL}/travel"),
    (13, f"imap://{FASTMAIL}/travel/portugal"),
]

# Each mailbox's folder under `V10/<account>/`.
FOLDERS = {
    3: f"{GMAIL}/[Gmail].mbox/All Mail.mbox",
    10: f"{FASTMAIL}/INBOX.mbox",
    11: f"{FASTMAIL}/Sent.mbox",
    12: f"{FASTMAIL}/travel.mbox",
    13: f"{FASTMAIL}/travel.mbox/portugal.mbox",
}

T0 = 1767603600  # 2026-01-05 09:00 UTC, as Unix seconds.

FLAG_READ = 1 << 0
FLAG_ANSWERED = 1 << 2
FLAG_FLAGGED = 1 << 4


def message(mid, sender, to, subject, date, body, extra=""):
    return (
        f"From: {sender}\n"
        f"To: {to}\n"
        f"Subject: {subject}\n"
        f"Date: {date}\n"
        f"Message-ID: <{mid}>\n"
        f"{extra}"
        "\n"
        f"{body}\n"
    )


PICARD = "Jean-Luc Picard <picard@enterprise.example>"
RIKER = "William Riker <riker@enterprise.example>"
DATA = "Data <data@enterprise.example>"
TROI = "Deanna Troi <troi@enterprise.example>"
LAFORGE = "Geordi La Forge <laforge@enterprise.example>"
HAYES = "Admiral Hayes <hayes@starfleet.example>"

REPORT_PDF = bytes(range(256)) * 2
INVITE_ICS = b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nSUMMARY:Debrief\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
# A 1x1 PNG.
CONDUIT_PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="
)
CORE_JPEG = b"\xff\xd8\xff\xe0" + b"\x5a" * 300 + b"\xff\xd9"

REPORT = (
    f"From: {DATA}\n"
    f"To: {PICARD}\n"
    "Subject: Away team report\n"
    "Date: Tue, 06 Jan 2026 14:00:00 +0000\n"
    "Message-ID: <report@enterprise.example>\n"
    "MIME-Version: 1.0\n"
    "Content-Type: multipart/mixed; boundary=b\n"
    "\n"
    "--b\n"
    "Content-Type: text/plain\n"
    "\n"
    "The report is attached; the debrief invitation follows.\n"
    "--b\n"
    "Content-Type: application/pdf; name=report.pdf\n"
    "Content-Disposition: attachment; filename=report.pdf\n"
    "Content-Transfer-Encoding: base64\n"
    f"X-Apple-Content-Length: {len(REPORT_PDF)}\n"
    "\n"
    "\n"
    "--b\n"
    "Content-Type: text/calendar; name=invite.ics\n"
    "Content-Transfer-Encoding: base64\n"
    f"X-Apple-Content-Length: {len(INVITE_ICS)}\n"
    "\n"
    "\n"
    "--b--\n"
)

SCHEMATIC = (
    f"From: {LAFORGE}\n"
    f"To: {PICARD}\n"
    "Subject: Warp core schematic\n"
    "Date: Wed, 07 Jan 2026 10:00:00 +0000\n"
    "Message-ID: <schematic@enterprise.example>\n"
    "MIME-Version: 1.0\n"
    "Content-Type: multipart/mixed; boundary=outer\n"
    "\n"
    "--outer\n"
    "Content-Type: multipart/related; boundary=rel\n"
    "\n"
    "--rel\n"
    "Content-Type: text/html; charset=utf-8\n"
    "\n"
    '<p>The plasma conduits:</p><img src="cid:conduit@enterprise.example" alt="conduits">\n'
    "--rel\n"
    "Content-Type: image/png; name=conduit.png\n"
    "Content-ID: <conduit@enterprise.example>\n"
    "Content-Disposition: inline; filename=conduit.png\n"
    "Content-Transfer-Encoding: base64\n"
    f"X-Apple-Content-Length: {len(CONDUIT_PNG)}\n"
    "\n"
    "\n"
    "--rel--\n"
    "\n"
    "--outer\n"
    "Content-Type: image/jpeg; name=core.jpg\n"
    "Content-Disposition: attachment; filename=core.jpg\n"
    "Content-Transfer-Encoding: base64\n"
    f"X-Apple-Content-Length: {len(CORE_JPEG)}\n"
    "\n"
    "\n"
    "--outer--\n"
)

FLEET = message(
    "fleet-advisory@starfleet.example",
    HAYES,
    "Senior Officers <officers@enterprise.example>",
    "Fleet advisory: Romulan Neutral Zone",
    "Thu, 08 Jan 2026 08:00:00 +0000",
    "All ships are to maintain yellow alert along the Neutral Zone.",
)

# (rowid, global id, mailbox, other mailboxes, read/flag bits, conversation,
#  days after T0, message, file name, parts Mail kept beside it, deleted)
MESSAGES = [
    (
        1,
        1,
        3,
        [2],
        FLAG_READ,
        100,
        0,
        message(
            "briefing@enterprise.example",
            PICARD,
            "Senior Staff <staff@enterprise.example>",
            "Briefing at 0900",
            "Mon, 05 Jan 2026 09:00:00 +0000",
            "Observation lounge, 0900.",
        ),
        "emlx",
        {},
        False,
    ),
    (
        1002,
        2,
        3,
        [2],
        0,
        100,
        0.01,
        message(
            "reply@enterprise.example",
            RIKER,
            PICARD,
            "Re: Briefing at 0900",
            "Mon, 05 Jan 2026 09:05:00 +0000",
            "I'll be there.",
            "In-Reply-To: <briefing@enterprise.example>\n",
        ),
        "emlx",
        {},
        False,
    ),
    (
        3,
        3,
        3,
        [6],
        FLAG_READ | FLAG_ANSWERED,
        101,
        1,
        REPORT,
        "partial.emlx",
        {"2": ("report.pdf", REPORT_PDF)},
        False,
    ),
    (
        4,
        4,
        3,
        [2],
        FLAG_READ | FLAG_FLAGGED,
        102,
        2,
        SCHEMATIC,
        "partial.emlx",
        {"1.2": ("conduit.png", CONDUIT_PNG), "2": ("core.jpg", CORE_JPEG)},
        False,
    ),
    (
        5,
        5,
        3,
        [2, 4],
        FLAG_READ,
        103,
        3,
        message(
            "orders@enterprise.example",
            PICARD,
            HAYES,
            "Orders acknowledged",
            "Fri, 09 Jan 2026 09:00:00 +0000",
            "Enterprise is under way.",
        ),
        "emlx",
        {},
        False,
    ),
    (
        6,
        6,
        3,
        [2],
        0,
        104,
        3,
        message(
            "not-downloaded@enterprise.example",
            TROI,
            PICARD,
            "Counselling schedule",
            "Fri, 09 Jan 2026 10:00:00 +0000",
            "Mail has not fetched this yet.",
        ),
        None,
        {},
        False,
    ),
    (
        7,
        7,
        3,
        [2],
        FLAG_READ,
        105,
        4,
        message(
            "q-again@enterprise.example",
            "Q <q@continuum.example>",
            PICARD,
            "Mon capitaine",
            "Sat, 10 Jan 2026 11:00:00 +0000",
            "Deleted, and good riddance.",
        ),
        "emlx",
        {},
        True,
    ),
    (8, 8, 3, [2], 0, 106, 3, FLEET, "emlx", {}, False),
    # The Fastmail account: folders, each with its own files.
    (20, 8, 10, [], FLAG_READ, 200, 3, FLEET, "emlx", {}, False),
    (
        21,
        9,
        12,
        [],
        FLAG_READ,
        201,
        4,
        message(
            "risa@enterprise.example",
            TROI,
            RIKER,
            "Shore leave on Risa",
            "Sun, 11 Jan 2026 18:00:00 +0000",
            "The jamaharon is booked.",
        ),
        "emlx",
        {},
        False,
    ),
    (
        22,
        9,
        13,
        [],
        FLAG_READ,
        201,
        4,
        message(
            "risa@enterprise.example",
            TROI,
            RIKER,
            "Shore leave on Risa",
            "Sun, 11 Jan 2026 18:00:00 +0000",
            "The jamaharon is booked.",
        ),
        "emlx",
        {},
        False,
    ),
    (
        23,
        10,
        11,
        [],
        FLAG_READ,
        201,
        4.1,
        message(
            "risa-reply@enterprise.example",
            RIKER,
            TROI,
            "Re: Shore leave on Risa",
            "Sun, 11 Jan 2026 19:00:00 +0000",
            "Bring the horga'hn.",
            "In-Reply-To: <risa@enterprise.example>\n",
        ),
        "emlx",
        {},
        False,
    ),
]

# Written after the checkpoint, so it lives only in `Envelope Index-wal`.
MESSAGES_IN_WAL = [
    (
        24,
        11,
        10,
        [],
        0,
        202,
        5,
        message(
            "late-arrival@enterprise.example",
            "Wesley Crusher <crusher.w@starfleet.example>",
            RIKER,
            "Arriving on the next transport",
            "Mon, 12 Jan 2026 07:00:00 +0000",
            "Permission to come aboard.",
        ),
        "emlx",
        {},
        False,
    ),
]

PLIST = (
    '<?xml version="1.0" encoding="UTF-8"?>\n'
    '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" '
    '"http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n'
    '<plist version="1.0">\n<dict>\n'
    "\t<key>conversation-id</key>\n\t<integer>{conv}</integer>\n"
    "\t<key>date-received</key>\n\t<integer>{date}</integer>\n"
    "\t<key>flags</key>\n\t<integer>{flags}</integer>\n"
    "</dict>\n</plist>\n"
)


def messages_dir(mailbox, rowid):
    """Mail spreads a store's files over `Data/<digits>/Messages/` once rowids
    pass 1000: the thousands, digit by digit, last digit first."""
    data = os.path.join("V10", FOLDERS[mailbox], STORE, "Data")
    thousands = rowid // 1000
    if thousands:
        data = os.path.join(data, *reversed(str(thousands)))
    return data


def write_files(root, rows):
    for rowid, _, mailbox, _, flags, conv, days, raw, kind, parts, _ in rows:
        if kind is None:
            continue
        data = os.path.join(root, messages_dir(mailbox, rowid))
        os.makedirs(os.path.join(data, "Messages"), exist_ok=True)
        body = raw.encode()
        date = int(T0 + days * 86400)
        trailer = PLIST.format(conv=conv, date=date, flags=flags).encode()
        with open(os.path.join(data, "Messages", f"{rowid}.{kind}"), "wb") as f:
            f.write(f"{len(body):<10}\n".encode() + body + trailer)
        for part, (name, content) in parts.items():
            folder = os.path.join(data, "Attachments", str(rowid), part)
            os.makedirs(folder, exist_ok=True)
            with open(os.path.join(folder, name), "wb") as f:
                f.write(content)


def insert(con, rows):
    seen_global = set()
    for rowid, gid, mailbox, others, flags, conv, days, raw, _, _, deleted in rows:
        subject = raw.split("Subject: ", 1)[1].split("\n", 1)[0]
        mid = raw.split("Message-ID: ", 1)[1].split("\n", 1)[0]
        con.execute("INSERT OR IGNORE INTO subjects (subject) VALUES (?)", (subject,))
        (subject_id,) = con.execute(
            "SELECT ROWID FROM subjects WHERE subject = ?", (subject,)
        ).fetchone()
        if gid not in seen_global:
            seen_global.add(gid)
            con.execute(
                "INSERT OR IGNORE INTO message_global_data (ROWID, message_id, message_id_header) "
                "VALUES (?, ?, ?)",
                (gid, gid * 7919, mid),
            )
        date = int(T0 + days * 86400)
        con.execute(
            "INSERT INTO messages (ROWID, message_id, global_message_id, remote_id, subject, "
            " date_sent, date_received, display_date, mailbox, flags, read, flagged, deleted, "
            " size, conversation_id) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            (
                rowid,
                gid * 7919,
                gid,
                10000 + rowid,
                subject_id,
                date - 1,
                date,
                date,
                mailbox,
                flags,
                flags & FLAG_READ,
                (flags & FLAG_FLAGGED) >> 4,
                int(deleted),
                len(raw),
                conv,
            ),
        )
        for other in others:
            con.execute(
                "INSERT INTO labels (message_id, mailbox_id) VALUES (?, ?)",
                (rowid, other),
            )


def build(root):
    data = os.path.join(root, "V10", "MailData")
    os.makedirs(data)
    for mailbox in FOLDERS.values():
        os.makedirs(os.path.join(root, "V10", mailbox), exist_ok=True)
    write_files(root, MESSAGES + MESSAGES_IN_WAL)

    index = os.path.join(data, "Envelope Index")
    con = sqlite3.connect(index)
    for stmt in SCHEMA:
        con.execute(stmt)
    con.executemany("INSERT INTO mailboxes (ROWID, url) VALUES (?, ?)", MAILBOXES)
    con.executemany(
        "INSERT INTO properties (key, value) VALUES (?, ?)",
        [("version", "4"), ("minor_version", "84003"), ("UUID", STORE)],
    )
    insert(con, MESSAGES)
    con.commit()

    con.execute("PRAGMA journal_mode=WAL")
    con.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    con.execute("PRAGMA wal_autocheckpoint=0")
    insert(con, MESSAGES_IN_WAL)
    con.commit()
    return con, index


def pack(root, out_path):
    """A tar of `root` with no trace of the machine or the hour. The WAL's
    header still carries a salt SQLite draws at random, as Mail's does."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames.sort()
            for name in sorted(filenames):
                path = os.path.join(dirpath, name)
                if name.endswith("-shm"):
                    continue
                info = tar.gettarinfo(path, arcname=os.path.relpath(path, root))
                info.mtime = 0
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                info.mode = 0o644
                with open(path, "rb") as f:
                    tar.addfile(info, f)
    with open(out_path, "wb") as f:
        f.write(buf.getvalue())


def main():
    out_path = sys.argv[1]
    work = tempfile.mkdtemp()
    try:
        root = os.path.join(work, "Mail")
        # The connection stays open while packing: closing the last one
        # would checkpoint the WAL into the main file and delete it.
        con, _ = build(root)
        pack(root, out_path)
        con.close()
    finally:
        shutil.rmtree(work)


if __name__ == "__main__":
    main()
