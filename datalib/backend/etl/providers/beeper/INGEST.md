# Beeper provider — ingest

> **Poorly supported.** Few people use this source, so it does not get
> the attention the others do. Expect rough edges.

Beeper Texts, the desktop app, keeps a cache of every chat it shows in
its data directory (on macOS, `~/Library/Application Support/BeeperTexts`).
This provider mirrors that cache. **No network, no auth, no Beeper API
calls.** The app has already pulled the messages from Beeper's servers
(cloud bridges such as Slack and Google Chat) and from its on-device
bridges (Signal, WhatsApp), so the cache is the one place every network
meets.

The download keeps the files' rows as they are and reads no meaning into
them; `../beeper_render/TRANSLATE.md` says how render turns them into
chats.

## Setup

1. Install Beeper Texts, sign in, connect the networks you want, and let
   it run long enough to fill its cache.
2. Add the source to `config.toml`:
   ```toml
   [[groups]]
   id = "beeper"
   type = "beeper"

   [[steps]]
   group = "beeper"
   function = "ingest"
   [steps.params.texts]
   media = true
   # path = "~/Library/Application Support/BeeperTexts"   # the default

   [[steps]]
   group = "beeper"
   function = "render_markdown"
   inputs = ["beeper/ingest"]
   [steps.params]
   # period = "month"                     # or "day", "year", "all"
   # networks = ["signal", "googlechat"]  # unset: every network
   ```

## What one run does

Three passes into `<data_root>/<group>/ingest/entities.doltlite_db`, each
**append-only**: a row the app drops from its cache stays here, because
keeping what the cache lets go is the reason to mirror it. A run that
finds nothing new commits nothing.

1. **`index.db`**, through the shared SQLite mirror
   (`datalib/backend/etl/sqlite_mirror/`), from a `VACUUM INTO` snapshot
   so the app can keep writing. Five tables, the ones render reads:
   `threads` (one row per chat), `participants`,
   `participant_identifiers` (the numbers and addresses the app knows
   for a person), `mx_room_messages` and `mx_reactions`. `mx_room_messages` declares a local `id` as its key,
   which a re-index renumbers, so the mirror leaves that column out and
   keys the table on `(roomID, eventID)`: a message is its Matrix event
   in its room.
2. **Every `local-*/megabridge.db`**, the on-device bridges' own
   databases. Only `message` is mirrored, keyed on `(bridge_id, mxid)`:
   it pairs each Matrix event with the network's own id for the message
   (`id`, `part_id`; Signal's is `<sender ACI>|<sent ms>`), which is
   what finds the same message in a phone's backup. Every bridge writes
   the same schema, so their rows share the one table, told apart by
   `bridge_id`. Cloud bridges have no local file.
3. **`media/`**: every cached file the store does not hold yet goes into
   the blob CAS, people's avatars among them, with a `beeper_media_files` row naming it by its path
   under `media/`. The app names a file by its media id and never
   rewrites it, so a file already held is not read again. With
   `media = false` this pass is skipped and attachments render without
   bytes.

Every network the app holds is mirrored; the render step's `networks`
picks which ones become documents.

## When part of a run fails

The step fails when `index.db` is missing (before the store is opened,
so a source that never downloaded has no store to render) or will not
mirror. Everything else is a `problems` row, and the next run tries again.

| what failed | its row |
|---|---|
| a `megabridge.db` that will not read | `phase:megabridge local-<bridge>`; its earlier rows stay |
| `media/` entries that will not list | `phase:media` |
| a cached file that will not read | `beeper_media_files:<path>` |

## Three Beeper runtimes, two covered

| Runtime | Examples | Where the data lives | This provider? |
|---|---|---|---|
| Cloud bridge | Slack, Google Chat, Telegram | Beeper's servers, end-to-end encrypted, *and* the app's `index.db` | yes, via `index.db` |
| On-device bridge | Signal, WhatsApp | `local-*/megabridge.db`, *and* `index.db`; never on Beeper's servers | yes, via both |
| Platform SDK | iMessage | `~/Library/Messages/chat.db`, read live by the app; **not** in `index.db` | no |

iMessage is the `apple_messages` source's, which mirrors `chat.db`.

The app also serves a local HTTP API (Beeper Desktop API, on
`localhost:23373`). It reads the same cache, has no history `index.db`
lacks, and names a message by `index.db`'s local `id` rather than its
Matrix event, so the files are the better source.
