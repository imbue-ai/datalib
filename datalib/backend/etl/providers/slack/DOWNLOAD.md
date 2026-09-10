# Slack Extract

`slack-download` mirrors a Slack workspace into a single doltlite db
at `<out>/raw/<name>/entities.doltlite_db`. Per-entity tables (channels, users,
messages, replies, files) are each keyed by their upstream Slack
identifier; payloads are stored as JSONB blobs in a `payload` column
alongside per-run bookkeeping and file/blob bytes. The old
`<out>/raw_api/<method>/events.jsonl` tree was retired with the
doltlite port — see
[`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md).

## Auth

The downloader does not handle Slack tokens directly. It shells out to
[`latchkey curl`](https://github.com/imbue-ai/latchkey), which signs
requests using a token stored in the host keyring under the `slack`
service. `latchkey` must be on `PATH` for the binary to run.

Required Slack OAuth scopes (user token):

  * `channels:history`, `groups:history`, `im:history`, `mpim:history`
  * `channels:read`, `groups:read`, `im:read`, `mpim:read`
  * `users:read`, `auth:test`

### File downloads

File bytes live on `https://files.slack.com/`, which the `slack`
service's `baseApiUrls` covers as of latchkey 2.11.2. No extra service
registration is needed — the same `slack` credential signs both
`slack.com/api/` and `files.slack.com/` requests.

## API surface used

| Method                      | Purpose                                  |
|-----------------------------|------------------------------------------|
| `auth.test`                 | Identify the workspace + the calling user |
| `conversations.list`        | Enumerate channels                       |
| `users.list`                | Enumerate workspace users                |
| `conversations.history`     | Per-channel forward pass + refresh window |
| `conversations.replies`     | Threaded replies for every parent message |

`shapes.rs` is the shape-of-the-response catalog: which path holds the
items, what counts as the cursor key, how to dedup.

## Channels and DMs

`conversations.list` covers four surfaces, selected by its `types`
param. We send `public_channel,private_channel` by default and append
`im,mpim` only when `dms` is set — so **`dms = false` is enforced by
never asking**, not by filtering a response that already contains your
DMs.

The two scopes are independent namespaces and neither filters the
other: `channels` names channels (`#`), `dm_users` names people (`@`).
A DM has no channel name to match, so folding both into one list would
silently drop every DM. `dm_users` set with `dms = false` is a config
error, because both silent readings of it are wrong.

What the surfaces actually look like on the wire (checked against the
live API 2026-08-31 — worth knowing, because the differences are what
the code is shaped around):

| Field | `public/private_channel` | `im` (1:1 DM) | `mpim` (group DM) |
|---|---|---|---|
| `name` | `general` | **absent** | `mpdm-alice--bob--carol-1` |
| `is_member` | yes | **absent** | yes |
| `is_archived` | yes | yes | yes |
| who's in it | — | `user` (just them) | `members` (**includes you**) |

Two consequences, both load-bearing:

  * **`members_only` cannot be applied to a DM.** A 1:1 DM has no
    `is_member` field, so the predicate that selects channels rejects
    every one of them. On a real workspace this was 99 of 203 DM rows.
    Hence the `is_dm` column: the `is_member` predicate runs only
    against `is_dm = 0`. `is_archived` applies to everything.
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

Its default also differs by entry point: the `slack-download` CLI
defaults to `DEFAULT_REFRESH_WINDOW_DAYS` (30), while a config-driven
run (`params.sync`) treats an unset value as 0 — no refresh pass.

## Resume + dedup

The dedup index doubles as the resume cursor:

  * For each channel, take `max(ts)` across all recorded `history`
    pages and start the next forward pass there.
  * For the trailing refresh window, re-query that range — the dedup
    pass collapses no-op refreshes to zero writes.

A page is skipped if every item in it matches a prior capture by
canonical content hash, so re-running soon after a successful run is
cheap.

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
- **`force_full_walk`** — the re-walk triggered by turning `media` on or
  raising the attachment size cap (see the next section). It re-reads
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
| `blob_size_limit_bytes` raised/lifted, with `media` on | Same re-walk, same reason |
| `blob_size_limit_bytes` relaxed, with `media` off | No-op — no blobs are fetched either way |

Only widenings do work; a narrowed knob leaves an on-disk superset and
nothing in the pipeline deletes. `channels` and `refresh_window_days` are
deliberately *not* recorded — a newly listed channel has no rows so it
cold-starts on its own, and the refresh window is re-applied every run.

`dms` and `dm_users` aren't recorded either, for the same reason one
level up: a newly listed DM has no message rows, so it cold-starts from
`since` unaided. What turning `dms` on *does* need is a fresh
`conversations.list` — the cached sweep was taken under the narrower
`types` and holds no DM rows at all. That is handled by keying the
sweep marker on `dms`, so flipping the knob misses the six-hour TTL
rather than silently mirroring nothing until it expires.

Two rules worth knowing when reading the code:

  * **An absent blob plans no work.** Data roots synced before this
    existed have no record, and treating that as "unknown, therefore
    re-download" would backfill every mirror at once on upgrade.
  * **The blob is recorded only when no channel failed.** Per-channel
    errors are warned and stepped over, so a run can return `Ok` without
    having covered everything; recording anyway would drop a scheduled
    backfill permanently, since — unlike the resume cursor — bookkeeping
    doesn't self-heal from stored rows.

## Rate limits

Slack signals a rate limit either as `429 Retry-After` or, on older
methods, as HTTP `200` with an `{"ok":false,"error":"ratelimited"}`
body. Both are handled centrally by the shared `latchkey_curl`
chokepoint — `api::slack_retryability` teaches it to recognize the
200-body form, after which it honors `Retry-After` / backs off and
enforces the source's `extract_params` give-up policy. When it gives
up, the call surfaces as `SlackError::Permanent`.

## Sample data

A curated [Star Trek: TNG-themed
fixture](tests/fixtures/slack_api/) demonstrates the raw wire format
and lives next to the code under test. The Python translator currently
reads it from this location as well.
