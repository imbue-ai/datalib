# apple_messages: the Messages app's own database

The `apple_messages` source mirrors `~/Library/Messages/chat.db` — the
SQLite file Apple's Messages app keeps on a Mac — table for table into
doltlite, through the same engine as `lightroom`, `apple_photos` and
`whatsapp` (`datalib/backend/etl/sqlite_mirror/`, explained in
[`lightroom/INGEST.md`](../lightroom/INGEST.md)). Then it renders every
chat through chat-common, one page per month. This file covers only what
Messages adds.

## The database

`chat.db` is plain, unencrypted SQLite in WAL mode, held open by the app
whenever it is running, so the default is a `VACUUM INTO` snapshot per
run. The tables the render reads:

| table | what |
|---|---|
| `chat` | one row per conversation; `guid` is `iMessage;-;+1…` for a 1:1 and `iMessage;+;chat…` for a group, `display_name` is the group's name (empty for a 1:1) |
| `handle` | one row per phone number or address; `id` is the number or address itself — contact names are not in this database |
| `message` | one row per message, tapback or group event |
| `attachment` | one row per file, `filename` under `~/Library/Messages/Attachments/` |
| `chat_message_join`, `chat_handle_join`, `message_attachment_join` | the rowid joins between them |

Every reference is a rowid, and the rowids are stable: the tables are
`AUTOINCREMENT` and nothing renumbers them. Identity still comes from the
guids beside them (`chat.guid`, `message.guid`), both minted by Messages
and the same in every copy of one Apple ID's database, so the ids the
render mints (`docs/dev/entity_ids.md`) are `ProviderGlobal`. Two of the
join tables are declared `UNIQUE` but not `PRIMARY KEY`; the config keys
them on that pair (`join_table_keys`) so `dolt_diff` can name their rows.

An iPhone backup's `3d0d7e5fb2ce288813306e4d4636395e047a3d28` is this
same database and can be pointed at directly.

## The body is not in `message.text`

Since macOS Ventura the `text` column is usually NULL and the body is in
`attributedBody`: Apple's `typedstream` archive of an NSAttributedString.
Measured on a macOS 26 database, 3 of 4 rows had no `text`. The archive
is a fixed prefix, the class name, then the string as a length-prefixed
UTF-8 run:

```
… NSString 01 94 84 01 2b <len> <utf-8 bytes> 86 …
```

`<len>` is one byte below 0x80, else `0x81` and two little-endian bytes,
else `0x82` and four. `typedstream.rs` reads exactly that and nothing
else; the attribute runs after it (message parts, mentions, the
attachment's transfer guid) are not needed. A message that is only an
attachment has the body `U+FFFC`, which render drops.

## Timestamps

`message.date` is nanoseconds since 2001-01-01 (seconds in a database
older than about 2011; render tells the two apart by magnitude). `0` is
"no date" and becomes a null `created_at`, not the epoch.

## Tapbacks and group events

A tapback is a `message` row whose `associated_message_type` is
2000–2005 (love, like, dislike, laugh, emphasize, question; 2006 is a
custom emoji in `associated_message_emoji`) with
`associated_message_guid` naming the target as `p:<part>/<guid>`. A
3000-series row withdraws one; render applies them in date order, so a
withdrawn tapback is absent from the page rather than shown and struck.
`item_type != 0` marks a group event (participant added or removed, a
rename, someone leaving), rendered as a system note.

## `skip_churn`

On by default. It drops the app's counters and settings
(`_SqliteDatabaseProperties`, `kvtable`), the Spotlight and CloudKit work
queues (`message_processing_task`, `persistent_tasks`,
`index_state_metrics`, `scheduled_messages_pending_cloudkit_delete`),
the cloud-sync tombstones (`sync_*`, `unsynced_*`), and every
`index_state` column. This list is the daemon tables by name, from one
database; it has not been measured across two snapshots the way
WhatsApp's `skip_churn` was, so a table that still moves between two
untouched runs is a finding, not a surprise. `recoverable_message_part`
(the "recently deleted" text) is kept: it is a message.

## Attachments are named, not copied

The rendered page lists each attachment by name, size and path and
draws chat-common's "(not yet fetched)" placeholder; no bytes reach the
CAS. Two reasons. Picking `chat.db` in the wizard grants the app access
to that file and nothing else — `~/Library/Messages/Attachments/` is
behind the same TCC wall and stays closed — and the tree is often tens
of gigabytes of video. A registry like WhatsApp's `wa_media_files`, fed
from a folder the user picks separately, is the shape if it is wanted.

## macOS permissions

`~/Library/Messages` is a location macOS protects: a process without
access gets `Operation not permitted` on a plain `ls`, and `sudo` does
not help. In the app, choosing the file in the picker is what grants
access (`docs/dev/wizard_file_pickers.md`; Cmd-Shift-G in the dialog
reaches the folder). Full Disk Access (System Settings → Privacy &
Security) is the durable fallback, and what a terminal needs to run
`datalib-dag` against the config directly — Finder already has it, so
dragging a copy out works when the terminal cannot.
