# Slack ingest

The ingest step of a `slack` group mirrors a Slack workspace into
`<data_root>/<group>/ingest/entities.doltlite_db`, with the blob CAS
beside it (`slack-ingest` does the same from the command line). The
tables (`src/ingest/schema_raw.rs`) are `workspaces`, `users`,
`channels`, `messages`, `threads` (one id per thread whose replies have
been asked for), `slack_attachments` (the edges to file bytes in the
CAS), and the account's own state: `channel_read_states`, `saved_items`
and `bookmarks`. Each row is keyed by its upstream Slack identifier — a
message and a thread by `{team}#{channel}#{ts}` — with the response
stored as JSONB in `payload` and a `<table>_bookkeeping` sidecar beside
each table.

## Auth

The downloader does not handle Slack tokens directly. It runs
`latchkey curl`, which puts the token latchkey holds for the `slack`
service on each request ([`docs/dev/latchkey.md`](../../../../../docs/dev/latchkey.md)).

Required Slack OAuth scopes (user token):

  * `channels:history`, `groups:history`, `im:history`, `mpim:history`
  * `channels:read`, `groups:read`, `im:read`, `mpim:read`
  * `users:read`, `auth:test`
  * `bookmarks:read`

`client.counts` and `saved.list` are not in Slack's published API; they
are what Slack's own web client calls, and they answer the browser-session
token `latchkey` holds. A token that cannot call them costs those two
tables and nothing else (see "The account's own state" below).

### File downloads

File bytes live on `https://files.slack.com/`, which the `slack`
service's `baseApiUrls` covers. No extra service registration is
needed: the same `slack` credential signs both `slack.com/api/` and
`files.slack.com/` requests, and both go through the same transport, so
a file download is retried, rate-limited and replayed like an API call.

## API surface used

| Method                      | Purpose                                  |
|-----------------------------|------------------------------------------|
| `auth.test`                 | Identify the workspace + the calling user |
| `conversations.list`        | Enumerate channels                       |
| `users.list`                | Enumerate workspace users                |
| `conversations.history`     | Each channel's uncovered stretches + refresh window |
| `conversations.replies`     | The replies of each thread the store owes |
| `client.counts`             | How far the account has read, per conversation |
| `saved.list`                | The account's "Saved for later" items    |
| `bookmarks.list`            | A conversation's header bookmarks        |

`shapes.rs` knows each method's response shape: where its items are and
what each is keyed by.

The first three are also the whole of `datalib-step probe slack`
(`src/probe.rs`). The wizard's "Check connection" runs it for
`auth.test` alone. Each picker's "Load" asks for one list:
`--list channels` pages `conversations.list` for channels only, as
`channel` items; `--list conversations` pages `users.list` and then
`conversations.list` for `im,mpim`, as `conversation` items (path =
Slack's id, title = what the sync will call it). The two never share
a listing, so a workspace's directory is read only by someone who
wants to pick DMs.

## Channels and DMs

`conversations.list` covers four surfaces, selected by its `types`
param. We send `public_channel,private_channel` by default and append
`im,mpim` only when `dms` is set — so **`dms = false` is enforced by
never asking**, not by filtering a response that already contains your
DMs.

The two scopes are independent namespaces and neither filters the
other: `channels` names channels by name, `dm_conversations` names DMs
by Slack's conversation id (`D…` for a 1:1, `G…`/`C…` for a group DM),
either bare or inside a pasted link — `Copy link` on a DM gives
`https://<ws>.slack.com/archives/<id>`, and `conversation_id` in
`ingest/mod.rs` reads the id out of any of Slack's link shapes. A DM
has no channel name to match, so folding both into one list would
silently drop every DM. `dm_conversations` set with `dms = false` is a
config error, because both silent readings of it are wrong.

What the surfaces actually look like on the wire (checked against the
live API; the differences are what the code is shaped around):

| Field | `public/private_channel` | `im` (1:1 DM) | `mpim` (group DM) |
|---|---|---|---|
| `name` | `general` | **absent** | `mpdm-alice--bob--carol-1` |
| `is_member` | yes | **absent** | yes |
| `is_archived` | yes | yes | yes |
| who's in it | — | `user` (just them) | `members` (**includes you**) |

Two consequences, both load-bearing:

  * **The member filter cannot be applied to a DM.** With neither
    `channels` nor `all_channels` set, the walk keeps only channels the
    account is a member of (`members_only`). A 1:1 DM has no
    `is_member` field, so that predicate would reject every one of them
    — on a real workspace, 99 of 203 DM rows. Hence the `is_dm` column:
    the `is_member` predicate runs only against `is_dm = 0`.
    `is_archived` applies to everything.
  * **Both DM shapes reduce to one participant list.** `dm_user_ids`
    stores `user` or `members` verbatim, comma-joined, and
    `dm_counterparts` subtracts the `auth.test` user at read time. One
    column answers both questions the DM path asks — is this a
    conversation with someone on the allowlist, and whose names title
    it — for either shape, so a group DM needs no special case. (It
    never subtracts to nothing: a DM with yourself keeps your name.)

A DM is titled after its counterparts — `@Jean-Luc Picard`,
`@William T. Riker, Worf` — since `#D0123ABCD` is unreadable. Slack's
`mpdm-…` handle is the fallback when participants can't be resolved;
it is not split back into people, because a Slack handle may itself
contain dashes.

## What bounds how far back we mirror

`since` — and only `since`. It is the oldest message any pass will ask
for (`YYYY-MM-DD` or RFC 3339, default `DEFAULT_SINCE` = 2024-01-01), so
"mirror just the last week" is `since` set to seven days ago.

`refresh_window_days` is *not* that knob, despite reading like one. It
re-reads the trailing days of what is already mirrored, so edits and
reactions on stored messages get picked up; it never narrows a run's
range. Setting it to 7 on a fresh store still walks everything from
`since`, and setting it on an existing store only adds API calls. The
window is counted back from the run's own clock (`FetchOptions::now`,
which the step pins), as the age of a listing sweep is.

Unset, it is `DEFAULT_REFRESH_WINDOW_DAYS` (30), for a configured
source and for the `slack-ingest` CLI alike. `0` turns the pass off.

## What a run still owes

Nothing in the store is a cursor, and nothing records that a channel,
a thread or a file is "done". Each run asks the store what it holds
and fetches the difference
([`sync_state.md`](/docs/dev/plans/sync_state.md) §2), so a run that is
stopped or killed anywhere leaves a store the next run finishes, with
nothing to remember in between.

| Kind | Wanted | Held | Owed |
|---|---|---|---|
| a channel's history | everything from `since` on | the `coverage` spans of scope `history:<channel>` | the gaps |
| a thread's replies | each stored root's `latest_reply` | `held_version` in the thread's sidecar | a root with replies whose thread is not held at that version |
| a file's bytes | an edge in `slack_attachments`, written with its message | the edge's `blake3`, and the fetch that landed it | an edge with no `blake3`, when `media` is on |

**History.** `conversations.history` returns a stretch newest first.
Each page is one transaction: its messages, an edge for each file they
carry, and a `coverage` span for the stretch the page settles, from its
oldest message up to where the page before left off (the last page
reaches the bottom of the stretch). A walk cut off after its first page
has covered the top, and the next run walks what is under it. The spans
are over top-level messages only; a reply's `ts` never moves them. The
newest message stored is never read as "fetched up to here".

The range wanted has no top. A walk covers up to the newest message it
saw, not up to "now", so every run asks each channel for what is newer
than that (one request when there is nothing), and a message that only
becomes visible late is not skipped. A channel with no messages has no
span, and is asked from `since` each run.

A span's ends are `ts`es padded to one width (`ts_key`), so they sort as
instants do. The TNG fixtures' stardate `ts`es have an eleventh digit.

**Threads.** After every channel's history, `RawDb::threads_listed` lists
every stored root with replies at the `latest_reply` its payload
carries, and `datalib_etl_web::owed` subtracts the threads held at that
version (`held_version` in `threads_bookkeeping`, keyed like the root).
It is a query over every stored root, not over the roots this run
happened to list, so a root stored by a run that died before its replies
is owed. A thread is one record whose fetch is paged: `owed::drain` asks
for one at a time, `conversations.replies` is walked to its end, and one
transaction stores the thread's row and messages, deletes the stored
replies the walk did not return, and holds the thread at the version it
was listed at. A walk cut off part way stores nothing of the thread.

**Files.** See [Attachments](#attachments).

A message that did not change is rewritten identically, which doltlite
stores as no change.

## Noticing a deleted message

Slack never tells us a message was deleted. There is no tombstone and no
"what changed since" endpoint — a deleted message just stops appearing in
`conversations.history`. The only way to see one go is to re-ask for a
stretch of history we already mirrored and compare.

We do that in two places. Outside them, a deletion is invisible to us,
and that is worth knowing before you rely on it.

### Top-level messages: inside the refresh window, and only there

`refresh_window_days` makes every run re-read the last N days of what
each channel has covered. A top-level message we hold in that range that
the re-read did not return has been deleted upstream, so we delete our
copy. History is strictly ordered, so each page lists a stretch whole,
and what it lacks there is deleted in that page's transaction.

Only top-level messages are judged this way. `conversations.history`
lists a thread's root and never its replies, so a reply missing from the
re-walk is not evidence of anything and is left alone. The one way the
window removes a reply is with its root: when a root is gone, its
replies and the version it was held at go with it, because nothing would
ever ask for that thread again. A reply that was also sent to the
channel (`thread_broadcast`) does appear in history, but it is stored as
a reply and treated as one here.

It defaults to 30 days. Without it we would notice nothing: no deleted
message, no edit, and no new reply on a thread whose root is older than
the newest message we hold. Set it to how far back you want those
caught, or to `0` to turn the pass off:

```toml
[steps.params.api]
refresh_window_days = 7
```

The window costs one extra pass over that many days of every channel, per
run, so it trades API calls against how quickly a deletion is spotted.
Nothing older than the window is ever looked at again — a message deleted
from last year stays in our copy indefinitely.

### Thread replies: when the thread is re-fetched anyway

A thread is read again when its root lists a newer reply than the one
it is held at. `conversations.replies` hands back the whole thread, so
a reply we hold that is missing from it was deleted, and we drop it in
the transaction that stores the thread.

The catch: **deleting a reply does not make a thread look stale.** The
newest reply either stays where it was or moves *backwards*, and neither
reads as "something new here". So a deleted reply is noticed the next
time somebody posts in that thread, and not before.

### A page that claims more and gives no cursor

Slack pages with a cursor. A response with `has_more` and no cursor to
ask with is not a listing of anything. A history page's messages are
stored, but nothing is deleted or covered; a thread's are not stored at
all, since the thread is one record. Either way the channel (or the
thread) is reported as failed, so the next run asks again.

### Deleting our copy is not as final as it sounds

The raw store is versioned, so a pruned row stays in history.
`dolt_diff_messages` names what a run removed and
`dolt_at_messages('HEAD^1')` reads it back — see
[doltlite.md](/docs/dev/doltlite.md). That is why none of this second-
guesses itself: if a prune turns out to be wrong, the rows are still
there.

## A changed config

No record is kept of the config a run had, because nothing needs one:
what is owed is computed from the config a run has now.

| Change | What it does |
|---|---|
| `since` earlier | A gap below what each channel has covered, walked once. |
| `since` later | Nothing. What is stored stays; nothing here deletes it. |
| `media` off → on | Every edge without bytes is owed, and fetched from its stored message. No channel is walked again. |
| `replies` off → on | Every stored root with replies is owed, and its thread is read. No channel is walked again. |
| `common.blob_size_limit_bytes` raised | The edges skipped for their size are still without bytes, so they are fetched. |
| a new channel, or `dms` on | The conversation has no coverage, so it is walked from `since`. |

Turning `dms` on also needs a fresh `conversations.list`: the cached
sweep was taken under the narrower `types` and holds no DM rows at all.
The sweep marker is keyed on `dms`, so flipping the knob misses the
six-hour TTL rather than mirroring nothing until it expires. A sweep
marker only says "do not list again yet"; it is not progress.

## When part of a sync fails

Only `auth.test` failing fails the step, or a channel listing that
fails with no channels stored from an earlier one: without either
there is nothing to walk. Anything else that fails is a `problems` row,
and the sync goes on with the rest.

- **A listing** — `users.list`, `conversations.list`, or one channel's
  `conversations.history` — is a `listing:` row (`listing:users.list`,
  `listing:conversations.history <channel>`). The run walks what an
  earlier listing stored. A run that gets to its end replaces the last
  run's rows, so the next run that lists cleanly clears them.
- **A thread** whose `conversations.replies` fails is an attempt on the
  thread's own row (`threads:<team>#<channel>#<ts>`, the key its root
  message has), an error while its replies have never been read and a
  warning once they have. Render names the root's grid row, so the
  problem shows on the thread's document. The held version did not move,
  so the thread is still owed and the next run asks again; the read that
  succeeds clears the row. Storing the root again does not: the thread's
  row is its own, so a history page cannot touch it.

A channel whose history fails still has its owed threads and files
fetched on that run: they are in the store whatever the walk did.

## The order of a run, and a run without replies

A run walks **every channel's history first**, then goes round the
channels again for what their stored messages owe: threads, then files.
History is a few requests a channel and a thread is one request each,
which Slack holds to about one a second, so on a workspace with tens of
thousands of threads the first pass is minutes and the second is hours.
In this order the whole workspace's top level is in the store, and
sealed for render, before the long part starts. Nothing is carried
from the first pass to the second but a count for the progress line:
the second pass asks the store what is owed.

`replies = false` skips the thread read altogether. The roots are
stored with the `reply_count` and `latest_reply` Slack listed, so every
thread is owed and stays owed; the run counts them (`threads_owed` in
its summary and on the step's line) and a later run with `replies` on
reads them. A root whose replies were never read renders as the root
alone.

## Attachments

Each file a stored message carries that Slack serves (not a tombstone,
not hosted elsewhere) is a `slack_attachments` row, written without a
`blake3` in the transaction that stores the message, whether or not
`media` is on. The bytes are owed from then on.

When `media` is on, each channel's owed files are fetched after its
threads, in the second pass, through `owed::drain` (`ingest/files.rs`), from the
file object the stored message carries: a request is `FILE_BATCH` files,
and storing them puts their bytes into the blob CAS (its own file, which
commits itself) before the one transaction that gives their edges a
`blake3` and holds them, so a kill between leaves only unnamed bytes. A
file whose bytes we already hold, under any message, is not fetched
again.

A file that does not land — the fetch failed, or it is over
`common.blob_size_limit_bytes` — keeps its edge without bytes, with
`last_error` set on `slack_attachments_bookkeeping` and a `problems` row
keyed `slack_attachments:<row id>`: `fetch_failed` (an error — the file
is missing) for a failure, `over_size_limit` (a warning) for a skip. It
is still owed, so every run tries it again: a transient failure
recovers, a size skip is judged against today's limit, and either way
the `problems` row is rewritten or cleared. An edge whose message no
longer carries a file Slack serves is deleted.

The stored `url_private_download` does not expire: it has no signature
in it, and the credential signs each request. Checked against the live
API: files from 2024 still answered 200 by their stored URL. A file that
is gone, or that the account cannot see, answers 302 (to a 404), which
is recorded as a failure.

## The account's own state

Three tables record where the account itself stands, rather than what
was said. All three cover only the conversations this run mirrors, so
`channels`, `dms` and `dm_conversations` narrow them the way they narrow
messages. They are fetched after the conversation listing and before
the message walk. A listing that fails becomes a `problems` row on the
Manage screen (a warning when Slack refused the token, an error
otherwise), leaves that table as it was, and does not stop the sync.

- **`channel_read_states`**, from one `client.counts` call: each
  conversation's `last_read`, `latest`, `has_unreads` and
  `mention_count`. **All of it is volatile**: the content payload is just
  `{"id": …}`, and the state lives in the `volatile_payload` column of
  `channel_read_states_bookkeeping`. Reading a channel is not a change to
  it, so it must not show in `dolt_diff_channel_read_states` or wake the
  render. `RawDb::load_read_states` lays the two halves back together.
  Upserted every run; never pruned.
- **A followed thread's own mark.** `conversations.replies` puts
  `last_read` and `subscribed` on its copy of the root of a thread the
  account follows (the `conversations.history` copy has neither).
  Those two are volatile on `messages` too
  (`MESSAGE_VOLATILE_PATHS`), so reading a thread is not an edit to its
  root, and the history copy does not overwrite the replies copy. A
  thread is only re-fetched when it has a new reply, so its mark is as
  fresh as its last reply, not as the last sync.
- **`saved_items`**, from `saved.list`: in progress, completed and
  archived. With no `filter` Slack returns only the in-progress ones, so
  the walk asks for each of `saved`, `completed` and `archived` (checked
  live: the three add up to the response's `counts.total_count`). The key
  is `{item_type}#{item_id}#{ts}`; a saved message is a pointer to the
  message, not a copy of it. Every run lists the whole set, so an item
  no longer listed is deleted — but only inside mirrored conversations.
- **`bookmarks`**, from `bookmarks.list`, one call per conversation. The
  folder a bookmark sits in is its `parent_id`; the folder itself is not
  listed (its label is in the channel's `properties.tabs`). A
  conversation is asked only when its `properties.tabs` shows a
  `bookmarks` or `folder` tab, or when we already hold bookmarks for it.
  On a real workspace 75 of 128 channels had no such tab, and
  none of the 25 of those we asked held a bookmark. Listed at most once
  per `MANIFEST_TTL`, like the channel list. The listing is not paged,
  so it is the conversation's whole set, and a bookmark it no longer
  names is deleted.

**Slack Lists are not mirrored.** `slackLists.*` answers the session
token with `not_allowed_token_type`, and no List turned up through
`files.list` or search on the workspace we checked.

Render reads both marks: a top-level message after its conversation's
`last_read`, or a reply after its followed thread's, renders unread
([`slack_render/TRANSLATE.md`](../slack_render/TRANSLATE.md)).

## Rate limits

Slack signals a rate limit either as `429 Retry-After` or, on older
methods, as HTTP `200` with an `{"ok":false,"error":"ratelimited"}`
body. Both are handled centrally by the shared `latchkey_curl`
chokepoint — `api::slack_retryability` teaches it to recognize the
200-body form, after which it honors `Retry-After` / backs off and
enforces the source's `common.download_params` give-up policy. When it
gives up, the call surfaces as `SlackError::Permanent`.

## Sample data

[`tests/fixtures/slack_api/`](tests/fixtures/slack_api/) is a TNG-themed
capture of the API, one `raw_api/<method>/` tape per method, replayed by
`:slack_tests` and by the central fixture pipeline.
[`tests/fixtures/slack_api_v2/`](tests/fixtures/slack_api_v2/README.md)
is the same workspace one sync later.
