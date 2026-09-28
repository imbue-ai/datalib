# Slack ingest

The ingest step of a `slack` group mirrors a Slack workspace into
`<data_root>/<group>/ingest/entities.doltlite_db`, with the blob CAS
beside it (`slack-ingest` does the same from the command line). The
tables (`src/ingest/schema_raw.rs`) are `workspaces`, `users`,
`channels`, `messages`, `replies_pages`, `slack_attachments` (the edges
to file bytes in the CAS), and the account's own state:
`channel_read_states`, `saved_items` and `bookmarks`. Each row is keyed
by its upstream Slack identifier — a message and a thread by
`{team}#{channel}#{ts}` — with the response stored as JSONB in
`payload` and a `<table>_bookkeeping` sidecar beside each table.

## Auth

The downloader does not handle Slack tokens directly. It shells out to
[`latchkey curl`](https://github.com/imbue-ai/latchkey), which signs
requests using a token stored in the host keyring under the `slack`
service. `latchkey` must be on `PATH` for the binary to run.

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
| `conversations.history`     | Per-channel forward pass + refresh window |
| `conversations.replies`     | Threaded replies for every parent message |
| `client.counts`             | How far the account has read, per conversation |
| `saved.list`                | The account's "Saved for later" items    |
| `bookmarks.list`            | A conversation's header bookmarks        |

`shapes.rs` knows each method's response shape: where its items are and
what each is keyed by.

The first three are also the whole of `datalib-step probe slack`
(`src/probe.rs`), which is what the wizard's "Test connection" runs:
it lists every channel the account can see as a `channel` item and
every DM as a `conversation` item (path = Slack's id, title = what the
sync will call it), and the `channels` / `dm_conversations` pickers
are built from that. It always asks for all four surfaces, whatever
`dms` says — nothing is stored, and the DM picker has to be ready
before the toggle is on.

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
adds a trailing re-query on top of the forward walk so edits and
reactions on already-stored messages get picked up; it never narrows a
run's range. Setting it to 7 on a fresh store still walks everything
from `since`, and setting it on an existing store only adds API calls.

Its default also differs by entry point: the `slack-ingest` CLI
defaults to `DEFAULT_REFRESH_WINDOW_DAYS` (30), while a config-driven
run (`params.sync`) treats an unset value as 0 — no refresh pass.

## Resume

The stored messages are the resume cursor: each channel's forward pass
starts after the newest `ts` it holds in `messages`, and a channel with
none starts at `since`. The trailing refresh window re-queries its range
on top; a message that did not change is rewritten identically, which
doltlite stores as no change.

## Noticing a deleted message

Slack never tells us a message was deleted. There is no tombstone and no
"what changed since" endpoint — a deleted message just stops appearing in
`conversations.history`. The only way to see one go is to re-ask for a
stretch of history we already mirrored and compare.

We do that in two places. Outside them, a deletion is invisible to us,
and that is worth knowing before you rely on it.

### Top-level messages: inside the refresh window, and only there

`refresh_window_days` makes every run re-walk the last N days of each
channel. Anything we hold in that range that the re-walk did not return
has been deleted upstream, so we delete our copy.

**It defaults to off** (`0`), so out of the box we notice nothing. Set it
to how far back you want deletions caught:

```toml
[steps.params.api]
refresh_window_days = 7
```

The window costs one extra pass over that many days of every channel, per
run, so it trades API calls against how quickly a deletion is spotted.
Nothing older than the window is ever looked at again — a message deleted
from last year stays in our copy indefinitely.

### Thread replies: when the thread is re-fetched anyway

A thread is re-walked when its newest reply is newer than the one we
stored. `conversations.replies` hands back the whole thread, so a reply
we hold that is missing from it was deleted, and we drop it.

The catch: **deleting a reply does not make a thread look stale.** The
newest reply either stays where it was or moves *backwards*, and neither
reads as "something new here". So a deleted reply is noticed the next
time somebody posts in that thread, and not before.

### Two cases where we deliberately delete nothing

- **The walk stopped short.** Slack pages with a cursor, and a response
  that claims there is more without supplying one leaves us holding part
  of a range. We keep everything: a range we only partly read looks
  exactly like a range whose messages were all deleted.
- **`force_full_walk`** — the re-walk triggered by turning `media` on
  (see the next section). It re-reads
  everything and would be an ideal moment to reconcile, but it skips the
  refresh-window pass as redundant, and that pass is where the comparison
  lives. A missed detection rather than a wrong one.

### Deleting our copy is not as final as it sounds

The raw store is versioned, so a pruned row stays in history.
`dolt_diff_messages` names what a run removed and
`dolt_at_messages('HEAD^1')` reads it back — see
[doltlite.md](/docs/dev/doltlite.md). That is why none of this second-
guesses itself: if a prune turns out to be wrong, the rows are still
there.

## Config changes the cursor would otherwise swallow

The resume cursor above answers "where do I start?" entirely from stored
data, which means it stops consulting the config that set it. `since` is
only read on the cold-start arm, so widening it would silently do
nothing — and structurally *could* not do anything, because the forward
walk only moves forward while a widened `since` asks to go backwards.

So the scope-affecting params are recorded after each successful run via
`datalib_etl::scope_config` (scope key `slack:download`, stored in
the raw store's `sync_scope_config` table), and the next run diffs them.
This is the one piece of bookkeeping outside the dedup index that
participates in the resume decision.

| Change | Reaction |
|---|---|
| `since` earlier | Walk `[since, min(ts)]` per channel — the window below what's mirrored. The forward resume cursor is untouched. Runs before the reply pass so backfilled thread roots get their replies. |
| `since` later | No-op |
| `media` off → on | Re-walk from `since`, including already-mirrored threads: attachment rows only exist for messages walked while the knob was on, and reply attachments are fetched only inside `paginate_replies`. |

`common.blob_size_limit_bytes` is not recorded: a file skipped for its
size is judged against the limit again on every run (see
[Attachments](#attachments)), so raising the limit needs no re-walk.

Only widenings do work; a narrowed knob leaves an on-disk superset and
nothing in the pipeline deletes. `channels` and `refresh_window_days` are
deliberately *not* recorded — a newly listed channel has no rows so it
cold-starts on its own, and the refresh window is re-applied every run.

`dms` and `dm_conversations` aren't recorded either, for the same reason one
level up: a newly listed DM has no message rows, so it cold-starts from
`since` unaided. What turning `dms` on *does* need is a fresh
`conversations.list` — the cached sweep was taken under the narrower
`types` and holds no DM rows at all. That is handled by keying the
sweep marker on `dms`, so flipping the knob misses the six-hour TTL
rather than silently mirroring nothing until it expires.

Two rules worth knowing when reading the code:

  * **An absent record plans no work.** Treating "no record" as
    "unknown, therefore re-download" would backfill a whole mirror from
    a store that simply predates the record.
  * **The record is written only when no channel failed.** Per-channel
    errors are warned and stepped over, so a run can return `Ok` without
    having covered everything; recording anyway would drop a scheduled
    backfill permanently, since — unlike the resume cursor — bookkeeping
    doesn't self-heal from stored rows.

## Attachments

A message's files are fetched while the walk lists that message, when
`media` is on. The bytes go into the blob CAS, and each (message, file)
pair is a `slack_attachments` row. A file whose bytes we already hold is
never fetched again. A channel's rows are written when its walk ends,
including a walk that failed partway: the messages it stored are behind
the resume cursor, so their files need a row for the retry below to
find.

A file that does not land — the fetch failed, or it is over
`common.blob_size_limit_bytes` — keeps its row with `last_error` set on
`slack_attachments_bookkeeping` and a `problems` row keyed
`slack_attachments:<row id>`: `fetch_failed` (an error — the file is
missing) for a failure, `over_size_limit` (info) for a skip.

The resume cursor passes a message once, so the walk alone would never
come back to that file. After the walk, every run tries again each such
attachment in the conversations it mirrors, from the file object its
stored message carries. A transient failure recovers; a size skip is
judged against today's limit; either way the `problems` row is rewritten
or cleared. One the walk already tried this run is not tried twice.

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
