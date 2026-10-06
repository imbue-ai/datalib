# Beeper provider — ingest

> **Poorly supported.** Nobody is using this source, so it does not get
> the attention the others do. In particular it does not notice when
> something disappears upstream. Don't set
> `common.always_clear_before_ingest` on it: `index.db` is a cache the
> desktop app *evicts* from, so absence there does not mean deletion, and
> wiping before each ingest would throw away real history. Fixing that
> properly means reconciling against the megabridge files too. Expect
> rough edges.

Beeper Texts (the desktop app) keeps a unified per-account message
cache at:

```
~/Library/Application Support/BeeperTexts/index.db
~/Library/Application Support/BeeperTexts/media/
```

This provider reads those directly. **No network. No auth. No
Beeper API calls.** The desktop app has already pulled the data
from Beeper Cloud (for cloud bridges like Slack, Google Chat),
linked local megabridges (Signal, WhatsApp), and the network's own
servers, and it stores everything locally in a bridge-agnostic
schema. We re-shape that into our `rooms` / `users` / `events` tables
and the blob CAS.

## Setup

1. Install Beeper Texts and sign in.
2. Configure whichever chat networks you want to ingest (Signal,
   Google Chat, etc.) inside the desktop app. Let it run long
   enough to do its first sync; the app's caches need to be
   populated.
3. Add the group and its step pair to your `config.toml`:
   ```toml
   [[groups]]
   id = "beeper"
   type = "beeper"

   [[steps]]
   group = "beeper"
   function = "ingest"
   [steps.params.texts]
   sources = ["signal", "googlechat"]
   media = true
   # path = "~/Library/Application Support/BeeperTexts"   # the default

   [[steps]]
   group = "beeper"
   function = "render_markdown"
   inputs = ["beeper/ingest"]
   # [steps.params]
   # period = "month"   # or "day", "year", "all"
   ```

## What lands on disk

The ingest step writes `<data_root>/<group>/ingest/entities.doltlite_db`
(schema in `src/ingest/schema_raw.rs`) and its blob CAS beside it:

- `rooms` — one row per Beeper thread matching the configured
  networks, keyed by the Matrix room id. `network` is the canonical
  network name (`signal`, `googlechat`, …), normalized so the underlying
  bridge (`slackgo`, `discordgo`, …) doesn't leak.
- `users` — one row per participant, keyed by the Matrix user id:
  `display_name` and `full_name` from index.db's `participants`.
- `events` — one row per message **and** one per reaction, keyed by the
  Matrix event id. `event_type` carries Beeper's own taxonomy (`TEXT`,
  `IMAGE`, `FILE`, `REACTION`, `HIDDEN`, …), so render does not have to
  reconstruct it from raw Matrix shapes. `external_event_id` is the
  bridge's own id, filled in by the megabridge pass below.
- `beeper_media_attachments` — one edge per attachment slot, from its
  event to the bytes in the blob CAS. A file the desktop app has not
  cached yet (or any file, with `media = false`) gets an edge with a
  NULL `blake3`, and render draws a "(not yet fetched)" placeholder.

Every run is a `sync_runs` row; `sync_scope_state` is unused, since
there is no remote endpoint to checkpoint against.

## The megabridge pass

After the `index.db` pass, the ingest walks every
`local-<bridge>/megabridge.db` for a configured network and joins its
`message` table on `mxid` to fill `events.external_event_id` with the
bridge's own id (`src/ingest/megabridge.rs`). It adds no rows: a
megabridge message with no matching event is only counted, as
`events_orphaned`. Cloud bridges have no local file and are skipped.

## When part of a sync fails

Every run reads the whole of `index.db` and every configured bridge
database, so everything below is tried again by the next run, and its
row goes as soon as it reads. The step fails only when `index.db` is
missing or its `threads` table will not read, or the store will not take
a write.

| what failed | its row |
|---|---|
| a cached media file that will not read (usually: not cached yet) | `beeper_media_attachments:<event>#<slot>`, an error, on an edge with no bytes |
| an attachment whose URL is neither `mxc://` nor `localmxc://` | the same key, a warning: this build does not read it |
| attachments that carry no `id` | `phase:attachments`, with a count |
| one thread's participants, messages or reactions | `listing:messages <thread id>`; the other threads land |
| a `megabridge.db` that will not read | `phase:megabridge <network>`; its events keep the ids an earlier run gave them |
| a `sources` entry no account in `index.db` is on | `config:sources:<network>` |

With `media = false` an edge with no bytes is a choice, not a problem,
and has no row.

## Three Beeper runtimes — only two covered

| Runtime | Examples | Where the data lives | This provider? |
|---|---|---|---|
| Cloud bridge | Slack (`slackgo`), Google Chat, Telegram, WhatsApp (cloud) | `matrix.beeper.com` on Beeper's servers, *and* cached locally in `index.db` | yes, via index.db |
| Local megabridge | Signal, WhatsApp (local mode) | `local-*/megabridge.db` on your machine, *and* cached in `index.db` | yes, via index.db |
| Platform-SDK | iMessage | `~/Library/Messages/chat.db` (macOS), read live by Beeper Texts with its Full Disk Access grant. **Not** cached in `index.db`. | no |

iMessage is covered by the separate `apple_messages` source, which
mirrors `chat.db` itself.

## Filtering

`texts.sources` is a list of canonical network names. Each matches
index.db `threads` rows whose `accountID` equals one of these patterns
or starts with one followed by `.` or `_`
(`account_patterns_for` in `src/ingest/index_db.rs`):

| `sources` entry | `accountID` patterns |
|---|---|
| `signal` | `local-signal` |
| `googlechat` | `googlechat`, `local-googlechat` |
| `slack` | `slackgo`, `local-slack`, `slack` |
| `whatsapp` | `whatsapp`, `local-whatsapp` |
| `telegram` | `telegram`, `local-telegram` |
| `discord` | `discordgo`, `local-discord`, `discord` |
| `linkedin` | `linkedin`, `local-linkedin` |
| `twitter` | `twitter`, `local-twitter` |
| `instagram` | `instagramgo`, `local-instagram`, `instagram` |
| `facebook` | `facebookgo`, `local-facebook`, `facebook` |
| `sms` | `gmessages`, `local-gmessages` |
| `imessage` | `imessage`, `local-imessage` *(no effect: index.db carries no iMessage data)* |

Only `signal` and `googlechat` have been exercised against a real
install.

## Why this reader shells out to `sqlite3`

Our workspace links `sqlx` against **doltlite**, and Cargo's
`links = "sqlite3"` rule allows only one SQLite-linking crate in a
graph, so a second, stock SQLite (`rusqlite` with `bundled`) cannot be
added beside it. Measured against `BeeperTexts/index.db`'s `threads`
through our doltlite-linked binary, on doltlite 0.11.2:

| Column     | stock SQLite (`sqlite3` CLI) | our doltlite-linked binary |
|------------|------------------------------|----------------------------|
| `accountID` | `"slackgo.TSTHRQ7MY-U06LVPXQD9B"` (text) | `"4374"` (integer — actually `length(thread)`) |
| `thread`   | full JSON (text)              | `NULL` |

`CAST(accountID AS TEXT)` did not help, and neither did checkpointing
the WAL on a private copy. That measurement predates doltlite 0.11.53's
fix for values read from plain SQLite files
([doltlite.md § Versions](/docs/dev/doltlite.md#versions-the-storage-format-and-what-each-pin-brought)),
and a synthetic table of the same shape (WAL, JSON over 4 KB) reads
correctly on 0.50.13; the `sqlite_mirror` sources read other apps'
SQLite files through the same sqlx. Re-measure against a real
`index.db` before dropping the `sqlite3` subprocess.

So both readers run the system `sqlite3` CLI (or `BEEPER_SQLITE3`) as
`sqlite3 -json -readonly <path>`, with the SQL on stdin. They open the
plain path, not a `file:…?immutable=1` URI: `immutable=1` ignores the
WAL, and Beeper Texts is a live writer whose newest rows are still in
it. `-readonly` reads through the WAL without taking a write lock.

## Media path resolution

Attachments inside `mx_room_messages.message` carry an `id` that is an
`mxc://` or `localmxc://` URI. We map those to on-disk paths under
`media/`:

| URI | On-disk path |
|---|---|
| `mxc://local.beeper.com/<id>` | `media/local.beeper.com/<id>` |
| `mxc://beeper.com/<id>` | `media/beeper.com/<id>` |
| `localmxc://local-signal/<id>` | `media/localhostlocal-signal/<id>` |

Beeper Texts decrypts content before caching, so the on-disk files
are plaintext. A file that has not been viewed in the desktop app may
not exist on disk; its edge is recorded with no bytes and a `problems`
row, and a later run with the file present fills it in and clears the
row.
