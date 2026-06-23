# Beeper provider — Extract

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
servers — and it stores everything locally in a stable
bridge-agnostic schema. We re-shape that into our `rooms` / `users` /
`events` / `blobs` doltlite tables.

## Setup

1. Install Beeper Texts and sign in.
2. Configure whichever chat networks you want to ingest (Signal,
   Google Chat, etc.) inside the desktop app. Let it run long
   enough to do its first sync; the app's caches need to be
   populated.
3. Configure the source in YAML:
   ```yaml
   - name: beeper
     type: beeper
     sync:
       sources: ["signal", "googlechat"]
       media: true
   ```

That's it.

## What lands on disk

`beeper-download` (or `frankweiler-sync` with a `beeper` source)
writes a single doltlite file at
`<data_root>/raw/<name>/entities.doltlite_db`. Tables:

- `rooms` — one row per Beeper "thread" matching the configured
  networks. `bridge_network` is the canonical network name
  (`signal`, `googlechat`, …), normalized so the underlying
  bridge version (`slackgo`, `discordgo`, …) doesn't leak.
- `users` — one row per participant we've seen. `full_name` /
  `display_name` from `participants`, `payload` carries the full
  participant row.
- `events` — one row per message AND one row per reaction. The
  `event_type` column carries Beeper's own taxonomy (`TEXT`,
  `IMAGE`, `FILE`, `REACTION`, `MEMBERSHIP`, …) so translators
  don't have to reconstruct it from raw Matrix shapes.
- `blobs` — cached media bytes. Each blob row's `kind` is
  `beeper_media`, `owning_id` is the originating event's UUID, and
  `slot` is the attachment filename. Files that haven't been
  downloaded by the desktop app yet (cache miss) produce a
  metadata-only row with `last_error` set.

The shared `sync_runs` / `sync_scope_state` tables that every
doltlite raw store carries are present but unused for Beeper, since
we don't have a remote endpoint to checkpoint against.

## Three Beeper runtimes — only one currently covered

Beeper actually has three different bridge models per chat network.
This provider covers the first two (because they both land in
index.db); the third is a separate code path that's not yet
implemented:

| Runtime | Examples | Where the data lives | This provider? |
|---|---|---|---|
| Cloud bridge | Slack (`slackgo`), Google Chat, Telegram, WhatsApp (cloud) | `matrix.beeper.com` on Beeper's servers, *and* cached locally in `index.db` | ✅ via index.db |
| Local megabridge | Signal, WhatsApp (local mode) | `local-*/megabridge.db` on your machine, *and* cached in `index.db` | ✅ via index.db |
| Platform-SDK | iMessage | `~/Library/Messages/chat.db` (macOS) — read live by Beeper Texts using its Full Disk Access grant. **Not** cached in `index.db`. | ❌ not yet |

For iMessage we'd add a separate reader for `chat.db`. Beeper Texts
has Full Disk Access; this provider, run from a terminal, does not
by default — so the binary would surface a clear error if FDA
hasn't been granted to the parent process.

## Filtering

`BeeperSync.sources` is a list of canonical network names. Each
maps to the right `accountID` prefixes inside `index.db.threads`:

| `sources:` entry | matches `accountID` prefixes |
|---|---|
| `signal` | `local-signal_…` |
| `googlechat` | `googlechat`, `googlechat.…` |
| `slack` | `slackgo.…`, `slackgo_…`, `slack.…` |
| `whatsapp` | `whatsapp.…`, `local-whatsapp_…` |
| `telegram` | `telegram.…`, `local-telegram_…` |
| `discord` | `discordgo.…`, `local-discord_…` |
| `linkedin` | `linkedin.…`, `local-linkedin_…` |
| `twitter` | `twitter.…`, `local-twitter_…` |
| `instagram` | `instagramgo.…`, `local-instagram_…` |
| `facebook` | `facebookgo.…`, `local-facebook_…` |
| `sms` | `gmessages.…`, `local-gmessages_…` |
| `imessage` | `imessage_…` *(no effect today — index.db doesn't carry iMessage data)* |

Only `signal` and `googlechat` are explicitly tested at the
moment. The others should work but haven't been exercised.

## Why this reader shells out to `sqlite3` instead of using sqlx

Our workspace links `sqlx` against **doltlite** (a SQLite fork with
record-format extensions, used so the doltlite files we *write*
gain version-control superpowers). That's fine for reading our
own output, but it makes doltlite the wrong engine for reading
*other apps'* SQLite files: doltlite misinterprets some
stock-SQLite record-type bytes.

Empirically observed against `BeeperTexts/index.db.threads`:

| Column     | stock SQLite (`sqlite3` CLI) | our doltlite-linked binary |
|------------|------------------------------|----------------------------|
| `accountID` | `"slackgo.TSTHRQ7MY-U06LVPXQD9B"` (text) | `"4374"` (integer — actually `length(thread)`!) |
| `thread`   | full JSON (text)              | `NULL` |

`typeof(accountID)` returns `"text"` to stock and `"integer"` to
doltlite. Forcing `CAST(accountID AS TEXT) AS alias` doesn't help
(the underlying value is already mis-typed). Copying the file +
running `PRAGMA wal_checkpoint(TRUNCATE)` on our private copy
doesn't help either, so the original "doltlite isn't applying the
WAL" theory was wrong — the WAL is irrelevant; doltlite's record
decoder differs from stock SQLite's even on the main btree pages.

We can't add a second SQLite-linking crate (`rusqlite` with
`bundled`, or another `libsqlite3-sys` consumer) because Cargo's
`links = "sqlite3"` rule allows only one in a graph.

**Workaround**: shell out to the system `sqlite3` CLI in
`-json -readonly` mode. macOS ships with stock SQLite at
`/usr/bin/sqlite3`. We use a `file:...?immutable=1` URI so the live
Beeper Texts writer can't be blocked by our reads. Throughput
overhead is small for the row volumes we deal with (hundreds of
threads, tens of thousands of messages).

If we ever switch the workspace off of doltlite (or doltlite
catches up to stock's record-format quirks), this reader can be
flipped back to in-process sqlx with no schema change.

## Concurrent access with Beeper Texts

The `sqlite3 -readonly` invocation opens `index.db` with
`file:...?immutable=1` so a live writer (Beeper Texts) can't block
us and we can't block it. Stock SQLite handles concurrent
readers-with-a-writer cleanly in WAL mode, which is what Beeper
Texts uses.

## Media path resolution

Attachments inside `mx_room_messages.message` carry an `id` field
that's an `mxc://` or `localmxc://` URI. We map those to on-disk
paths under `media/`:

| URI | On-disk path |
|---|---|
| `mxc://local.beeper.com/<id>` | `media/local.beeper.com/<id>` |
| `mxc://beeper.com/<id>` | `media/beeper.com/<id>` |
| `localmxc://local-signal/<id>` | `media/localhostlocal-signal/<id>` |

Beeper Texts decrypts content before caching, so the on-disk files
are plaintext. Files that haven't been viewed in the desktop app
yet may not exist on disk; in that case we record metadata + URL
only and move on (`blob_errors` counter increments).
