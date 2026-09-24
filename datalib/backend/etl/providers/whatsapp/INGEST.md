# whatsapp — a mirrored msgstore, plus the media beside it

WhatsApp for Android backs its message store up as
`Databases/msgstore.db.crypt15`: an ordinary SQLite database, encrypted
with a 32-byte key the user can read off the app. This provider decrypts
it to a tempfile and mirrors that file into a doltlite store with the
engine `lightroom` and `apple_photos` use,
[`datalib_etl_sqlite_mirror`](/datalib/backend/etl/sqlite_mirror/) —
every table, dropped and refilled each run, doltlite keeping only what
changed. **Read [`lightroom/INGEST.md`](../lightroom/INGEST.md) first**;
this document covers only what is WhatsApp-shaped, and the one thing the
engine does not do here: the attachments.

Unlike the other two mirror sources this one **renders**. The render
crate (`datalib_etl_whatsapp_render`) reads the mirrored tables straight
off the pinned commit and turns them into the chat-common markdown, so
there is a `render_markdown` step and the messages reach `grid_rows`.

## What a backup looks like

Measured on a test phone's backup from 2026 (`whatsapp_msgstore_probe`,
below, prints this for any backup):

| | |
| --- | --- |
| tables | 277 plain + 1 virtual (`ai_thread_info_fts`, with 5 shadow tables the engine skips) |
| keys | 83 tables on `_id INTEGER PRIMARY KEY`, 87 `AUTOINCREMENT`, 5 composite |
| indexes | **none** — WhatsApp drops every secondary index before backing up; the 27 that remain are the auto-indexes of PRIMARY KEY / UNIQUE constraints |
| the graph | `chat.jid_row_id → jid._id`; `message.chat_row_id → chat._id`; `message_text` / `message_media` keyed on `message_row_id`; `message_add_on_reaction` keyed on `message_add_on_row_id` |
| the body | `message.text_data` only for plain text; captions, link previews and add-ons live in the child tables |
| attachments | `message_media.file_path` is `Media/WhatsApp Images/IMG-….jpg` — a path under the backup root, into the plaintext `Media/` tree beside `Databases/` |

## Keys: `_id` is a rowid, and it is stable

Every reference in msgstore is a rowid, and the natural identity of a
message is composite *and cross-table*: `(jid.raw_string, key_id,
from_me)` reached through `chat`. The engine's stable-key rewrite is
single-column and intra-table, so it cannot express that, and the
backup carries no UNIQUE index to hang it on anyway. So the mirror keys
every table on what msgstore declares — the rowid.

That is fine because **the rowids do not move between backups of one
phone.** Measured on two real backups a week apart (2026-06-01 →
06-07), joining `jid`, `chat` and `message` across them on their natural
keys: `_id` differed for 0 of 13, 0 of 4 and 0 of 10 rows. The tables
are `AUTOINCREMENT`, and a restore copies the database file rather than
re-importing it. The events that would renumber — a schema migration
that rebuilds a table, a phone-to-phone transfer — read as one full
"removed, added" commit, which is the honest record of what happened.

The one table that *does* renumber is `props`, which WhatsApp rewrites
wholesale, and it is excluded by default (below).

An earlier version of this provider re-keyed nine hand-picked tables on
their natural keys at ingest time: eleven hundred lines of column lists
and rowid→key maps, defending against a renumbering the measurement
says does not occur, and dropping the other 260-odd tables on the
floor. Identity now lives where it is used: render resolves the rowid
graph to natural keys and mints the uuids from those
(`schema_raw::whatsapp_message_uuid`), so a uuid never contains a rowid
even though the store is keyed on them.

## Read state: one mark per chat

msgstore has no per-message "read" flag. Each `chat` row carries a read
mark instead, `last_read_message_row_id`: an incoming message whose
`_id` is past it is unread. Checked on the real backup above
(2026-09-24): that rule counts exactly each chat's
`unseen_message_count`. A chat nothing was read in points the mark at
the seed row, `_id` 1, so everything in it counts; a chat with only the
account's own messages leaves it NULL. `last_read_message_sort_id` is
the same mark in `sort_id` terms, and `sort_id` equals `_id` on every
row we have seen. Render reads the row-id form (`whatsapp_render`'s
`parse.rs`).

The fixture (`whatsapp_make_fixture`) now builds `chat` with the real
table's columns in the real order, and fills the message pointers the
way the phone does: newest message for last/display, the spec's
`last_read_message_id` (else the newest) for the read mark and the read
receipt, the unseen counters from the mark.

## Names: `wa.db`, beside msgstore

msgstore knows almost nothing about who people are: `lid_display_name`
names some linked ids, and on the real test phone it was empty, so every
chat rendered as a phone number or a raw `…@lid`. The names the phone
shows live in `wa.db`, which WhatsApp backs up as
`Backups/wa.db.crypt15`, encrypted with the same key. Its `wa_contacts`
holds `display_name` (the phone's address book), `given_name` /
`family_name`, and `wa_name` (the name the person set themselves), one
row per address-book entry, keyed by jid.

The mirror engine mirrors one source per store, so `wa.db` is not
mirrored: the ingest decrypts it and copies `wa_contacts` into
`wa_db_contacts`, one row per jid holding all that jid's rows as JSON
(`schema_raw.rs` says why). A backup without `Backups/wa.db.crypt15`
leaves the stored contacts as they were, as a missing `Media/` does.

Render names a jid by the first of: its address-book name, its
`lid_display_name`, the name the person set, its phone number, the raw
jid — each looked up under the jid and, for a linked id, under the phone
number `jid_map` gives it.

Profile photos are not in any backup: `wa_contacts` has only their
timestamps (`photo_ts`, `thumb_ts`), and the images stay in the app's
private storage. `Media/WhatsApp Profile Photos` holds only photos
someone saved by hand.

## `skip_churn`: what moves when nothing happened

Between the two backups above, with no message sent, three tables
changed:

| table | what it is |
| --- | --- |
| `props` | the app's own key/value settings; rewritten and renumbered on every backup |
| `backup_changes` | WhatsApp's "what changed since the last backup" log — the `ACHANGE` of msgstore, and superseded by the mirror's own `dolt_log` |
| `frequent` | per-contact usage counters |

`skip_churn = true` — the default — excludes them
(`CHURN_TABLE_PATTERNS` in `whatsapp_config`). Everything else that
moved in a week of use was a message, a chat, a contact mapping or a
device row: real data. With the three excluded, mirroring the same
backup twice produces no commit, which is what makes an unchanged phone
an unchanged store.

`exclude_tables`, `exclude_columns` and `include_tables` take globs
exactly as `apple_photos` does.

## Media: the part the engine does not do

The database names attachments; the bytes are in `Media/`, in the
clear. After the mirror run, the ingest walks `Media/` (through the host
fingerprint cache, so an unchanged tree is a stat per file), puts any
new bytes into the sidecar blob CAS keyed by blake3, and drops-and-
refills `wa_media_files` — the one table this provider authors
(`schema_raw.rs`). It sits in the same store as the mirrored tables and
survives the engine's per-run drop because it is named in
`MirrorOptions::sidecar_tables`.

`wa_media_files.relative_path` is anchored at the backup root, with the
`Media/` prefix, because that is exactly how `message_media.file_path`
spells it and the join is the only thing linking a message to its bytes.
A backup pulled without `Media/` mirrors fine and renders every
attachment as a placeholder; copying `Media/` in later and re-running
fills them in — render's diff scan follows a changed registry row back
to its message.

## What render does with it

`whatsapp_render/src/render/parse.rs` loads `jid`, `chat`, `message`,
`message_media` ⋈ `wa_media_files`, and `message_add_on` ⋈
`message_add_on_reaction` off the pinned commit and resolves the rowid
graph in memory. The incremental scan (`render.rs`) asks
`dolt_diff_<table>` for each table's changed rows, walks them up to a
`chat._id` — at HEAD for a row that still exists, at the previous
commit for one that was removed, since a rowid is the same row at
either — and then to a JID the same way. A chat that only the previous
commit can name is one the phone deleted, and its documents are
dropped.

## Running it

As a DAG step — the `whatsapp` stanza in
[`docs/user/config_examples/all_sources.toml`](/docs/user/config_examples/all_sources.toml):

```toml
[[groups]]
id = "whatsapp"
type = "whatsapp"

[[steps]]
group = "whatsapp"
function = "ingest"
[steps.params.backup]
path = "~/backups/WhatsApp"      # holds Databases/msgstore.db.crypt15 and Media/

[[steps]]
group = "whatsapp"
function = "render_markdown"
inputs = ["whatsapp/ingest"]
```

The key comes from `WHATSAPP_BACKUP_DECRYPTION_KEY` (or the env var
named by `key_env_var`); how to get it is in
[`getting_your_data.md`](/docs/user/getting_your_data.md#whatsapp).

## Measuring a backup

```sh
bazelisk build //datalib/backend/etl/providers/whatsapp:whatsapp_msgstore_probe
p=bazel-bin/datalib/backend/etl/providers/whatsapp/whatsapp_msgstore_probe

# Inventory: table count, key shapes, largest tables.
$p ~/backups/WhatsApp/Databases/msgstore.db.crypt15

# Rowid stability between two backups, joined on natural keys.
$p Databases/msgstore-2026-06-01.1.db.crypt15 Databases/msgstore-2026-06-07.1.db.crypt15

# Just the plaintext, for the engine's own CLI or sqlite3.
$p --decrypt Databases/msgstore.db.crypt15 /tmp/msgstore.db
```

Nothing it prints is message content: table names, row counts, key
columns, and match counts.
