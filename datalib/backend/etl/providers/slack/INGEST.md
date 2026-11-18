# Slack ingest

The ingest step of a `slack` group mirrors a Slack workspace into
`<data_root>/<group>/ingest/entities.doltlite_db`, with the blob CAS
beside it. The
tables (`src/ingest/schema_raw.rs`) are `workspaces`, `users`,
`channels`, `messages`, `threads` (one id per thread whose replies have
been asked for), `slack_attachments` (the edges to file bytes in the
CAS), and the account's own state: `channel_read_states`, `read_marks`,
`saved_items` and `bookmarks`. Each row is keyed by its upstream Slack identifier — a
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
tables and nothing else (see "The account's own state" below), and is
not asked them again for a week after a refusal ([A call this token
may not make](#a-call-this-token-may-not-make)).

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
| `users.info`                | One author no `users.list` has named yet |
| `conversations.history`     | Each channel's uncovered stretches + refresh window |
| `conversations.replies`     | The replies of each thread the store owes |
| `search.messages`           | Replies posted since the last search, to roots history will not read again; with `scope = "involves_me"`, the messages that involve the account |
| `client.counts`             | How far the account has read, and which conversations have something new; not asked for a week after a refusal |
| `saved.list`                | The account's "Saved for later" items; not asked for a week after a refusal |
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

### Archived channels

Every run lists `conversations.list` with the archived channels in
(`exclude_archived=false`), whatever the config, so each stored
`is_archived` follows Slack's. A listing that left them out would
never say that a channel mirrored while open had since been archived,
and the run would go on walking it. Listing them costs no extra
request: on a real workspace, 850 channels, 469 of them archived, took
5 calls at 1000, as many as its 381 open ones alone.

Which of them a run walks is `channels_for_fetch`'s choice: archived
ones only when the source sets `archived = true`, or names them in
`channels` (a named channel is mirrored archived or not). Without
`all_channels` the member filter above still applies, and Slack mostly
drops membership when it archives a channel: of those 469, the account
was still a member of 7. So **`archived` is meant with
`all_channels`**; on its own it adds next to nothing.

The flag covers every conversation the run mirrors, so with `dms` it
also brings in a DM Slack marks archived. With `scope = "involves_me"`
an archived channel is one more channel search can choose.

**An archived channel is read once.** It takes no new messages and no
edits, but `client.counts` rarely says so: it listed 1 of those 469
archived channels (and 127 of the 381 open ones), so each of the rest
would have cost a history request every run. Instead, the first run
that finds a channel archived walks it whole, as any channel
`client.counts` does not name, and stamps a sweep marker
(`archived:<id>`) when that walk ends. While the marker is there the
channel is quiet: no request for anything newer and no refresh-window
re-read, only a gap under what is held (a `since` moved earlier, a
walk cut off). The marker is dropped the first time a listing shows
the channel open again; from then on the usual rules walk it, and if
Slack archives it again, what was posted in between is read once more.

### Which conversations a run walks

Asking a conversation's history whether it has anything new costs a
rate-limited call apiece, one after another, every run: 126 quiet
channels took 80 seconds. So a run first asks `client.counts` (already
called for the read states, below), which names the newest message of
every conversation open in the account's sidebar in one call. A
conversation it names is walked only when that message is one what we
hold does not reach: newer than what is covered, or with a gap under
it, and not older than `since` (`to_walk` in `ingest/mod.rs`). Every
other one costs no request for new messages and no progress line. Its
threads and files still owed are fetched as usual, its [refresh
window](#top-level-messages-inside-the-refresh-window-and-only-there) is
re-read when one is due, and the reply search still covers it, so a new
reply on an old root is found whether or not the reply moved the
conversation's newest message.

What a conversation `client.counts` does not name depends on what it is:

- **A channel** is walked as before. Every channel the account is a
  member of is in the sidebar, so this is a channel mirrored through
  `all_channels` or `channels` that the account has not joined.
- **A DM** is closed in the sidebar. It is walked whole when its check
  is due: on the first run that finds it closed, and then once a day
  (`CLOSED_DM_CHECK_EVERY`), each check recorded as a sweep
  (`closed_dm:<id>`) when its walk ends. Between checks it is walked
  only for a gap under what is held, which a `since` moved earlier
  leaves, and not even its refresh window. A workspace a few years old
  lists hundreds of DMs, and `client.counts` names only the ones open
  in the sidebar.

A run records what this comes to in `coverage_checked` (`present` in
`ingest/mod.rs`): a conversation `client.counts` shows with nothing newer
than what is covered, or an archived one already read, at planning; one
walked up to its open top, when that walk ends, in the transaction that
stores its last page. Each is stamped with the run's `now`, from before
any of it was asked. A closed DM between checks, a walk that failed or
never ended, and a stretch with a top of its own (`context_after`) keep
whatever stamp they had.

Checked against the live API:

- `client.counts` named all 126 channels an account was a member of,
  each with a `latest`. In 23 of 25 sampled, it was the newest message
  `conversations.history` returned; in the other two that message was a
  join or leave notice, which `latest` skips. Such a notice is picked
  up by the next walk of the channel or its next refresh.
- `conversations.list` gives a DM no `latest`. Its `updated` is not
  activity: of 45 DMs sampled, a dozen had messages newer than
  `updated`, by up to 626 days.
- `client.counts` names only the DMs open in the sidebar (26 of several
  hundred).
- Search cannot prove a DM empty: `is:dm` and `in:<#D…>` both missed
  one of five DMs with messages that week.

**What the daily check is for.** Slack reopens a DM when a message
arrives, so the next run sees it in `client.counts` and walks it. A DM
closed again before that run shows nothing, and the daily check is what
reads it; one finding a message logs `slack_closed_dm_had_news` at
warn, since it means `client.counts` missed it. Without the first
walk, a first sync with a wide `since` would lose every message in the
DMs the account had closed. The cost is one request per closed DM a
day, and one per DM on a first sync.

A DM named in `dm_conversations` is held to the same rules: naming it
narrows which DMs are mirrored, nothing else. A run whose
`client.counts` call fails, or that does not ask it because Slack
refused it within the week, walks every conversation, as before.
`slack_walks_planned` counts the closed DMs and the ones checked.

### Only what involves me

With `scope = "involves_me"`, search decides which channels a run
walks and from when; the walk itself is the one above. A first sync
over two weeks then walks the few dozen channels where the account
was mentioned or posted, instead of every channel it is in.

1. **Search.** Each kind in `involves_me` is one or two
   `search.messages` queries over `[since, now − 30 min]`
   (`ingest/involving.rs`): `mention` is `<@U…>`, `from_me` is
   `from:<@U…>`, and `broadcast` is `@here` and `@channel`. Each query's
   searched time is a `coverage` scope of its own (`involving:<query>`),
   paged and settled like the reply search, and each page stores the
   messages it listed in `involving`, a pointer per message (its
   conversation, `ts` and thread), not a copy.
2. **Choose.** A channel is walked when a stored hit inside `since` is
   in it, or `client.counts` shows a mention of the account there
   (`mention_count`, or a thread mention in
   `threads.mention_count_by_channel`), which search may not list for
   half an hour. Either can add a channel; neither removes one.
3. **Where each starts.** A channel's stretch reaches from
   `context_before` (default a day) ahead of its oldest hit up to now,
   or with `context_after` set, a window around each hit. A
   `client.counts` mention opens it from `context_before` ahead of the
   last message read, up to now. Nothing starts before `since`.
4. **A root no stretch reaches.** A hit that is a reply on an older
   root has the root read on its own (`conversations.history` at
   exactly its `ts`, as the reply search does), so the thread is owed
   like any other.

Checked against the live API: `<@U…>` and `from:<@U…>` match what they
say (`from:me` is the same as the second); `@here` and `@channel` match
those mentions and nothing else, while `<!here>` and `<!channel>`
match the words. A user group's `<!subteam^…>` and `@handle` matched
no message carrying the mention, so user groups are not searched for.

**What it misses, measured.** On one real account over 14 days, a
sync with this scope and one with every channel were compared. Of the
104 channel messages in the full mirror that involve the account (a
mention of it, its own message, or `@here` / `@channel`), 103 were in
the narrow one, and the DM messages were the same in both. The one
missed was the account's own "joined the channel" notice
(`subtype: channel_join`), in a channel where nothing else involved it:
search does not list join notices, so a channel the account only
joined is not walked.

A search match is never stored as the message. It lacks `reactions`,
`edited` and a broadcast's `subtype`, and its `text` loses mrkdwn
about one time in eight; the message comes from history.

DMs follow `dms` and the rules above, whatever the scope: every DM
involves the account, and search cannot prove one empty. A search that
fails fails the run: walking every channel instead would cost the very
sync this scope avoids. Switching the scope either way needs nothing
done to the store. Turning it on leaves what is stored and walks less;
turning it off leaves each chosen channel's `history:` coverage
starting at its floor, so the next run walks down to `since`.

## What bounds how far back we mirror

`since` — and only `since`. It is the oldest message any pass will ask
for (a day, an RFC 3339 instant or an offset from the run's start,
unset: the whole history, `DEFAULT_SINCE` = 1970-01-01), so "mirror just the last week"
is `since = "-7d"`. The processor resolves an offset against the run's
`now` and hands the download an instant, so the window slides; what it
slides past stays stored.

A `since` earlier than the epoch is fine to write — `1900-01-01`, or
`0000-01-01` — and means the same as the epoch, because no Slack message
is older than one. What reaches the API is clamped to the epoch and then
spelled the way `conversations.history` will take it, which is narrower
than it looks. Measured against the live API:

| `oldest=` | answer |
|---|---|
| `-62135596800.000000` (a pre-epoch stamp) | an empty page, so the mirror held nothing (#1048) |
| `0.000000` | `invalid_ts_oldest` |
| `0.0` | `invalid_ts_oldest` |
| `0` | ok |
| `0.000001` | ok |
| `1704067200.000000` | ok |

So Slack refuses a zero that carries a fraction, while taking a bare
zero and every real stamp. Every other bound goes out as
`<seconds>.<micros>`; a bound at the epoch goes out as `0`
(`oldest_param` in `ingest/mod.rs`, with the tests that hold us to it).
This is worth knowing because the epoch is not an edge case here: it is
what a source with no `since` asks from, so getting it wrong means a
source mirrors no messages at all.

`refresh_window_days` is *not* that knob, despite reading like one. It
re-reads the trailing days of what is already mirrored, so edits and
reactions on stored messages get picked up; it never narrows a run's
range. Setting it to 7 on a fresh store still walks everything from
`since`, and setting it on an existing store only adds API calls. The
window is counted back from the run's own clock (`FetchOptions::now`,
which the step pins), as the age of a listing sweep is.

Unset, it is `DEFAULT_REFRESH_WINDOW_DAYS` (30). `0` turns the pass
off.

## What a run still owes

Nothing in the store is a cursor, and nothing records that a channel,
a thread or a file is "done". Each run asks the store what it holds
and fetches the difference
([`data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md#what-is-left-to-fetch-listed-minus-held)), so a run that is
stopped or killed anywhere leaves a store the next run finishes, with
nothing to remember in between.

| Kind | Wanted | Held | Owed |
|---|---|---|---|
| a channel's history | everything from `since` on | the `coverage` spans of scope `history:<channel>` | the gaps reaching down to what `client.counts` says is its newest message ([which conversations a run walks](#which-conversations-a-run-walks)) |
| a thread's replies | each stored root's `latest_reply` | `held_version` in the thread's sidecar | a root with replies whose thread is not held at that version |
| reply time, searched | everything from `since` to half an hour ago | the `coverage` spans of scope `replies:<channel>` | the gaps |
| a file's bytes | an edge in `slack_attachments`, written with its message | the edge's `blake3`, and the fetch that landed it | an edge with no `blake3`, when `media` is on |

**History.** `conversations.history` returns a stretch newest first,
asked for 999 messages a page (`MESSAGES_PER_PAGE`, Slack's most, and
the same for `conversations.replies`). Slack may answer fewer, so a
short page is not read as the end; the cursor says whether there is
more. A page is written together, in the run's open transaction ([one
transaction between two seals](#one-transaction-between-two-seals)): its
messages, an edge for each file they carry, and a `coverage` span for
the stretch the page settles, from its
oldest message up to where the page before left off (the last page
reaches the bottom of the stretch). A walk cut off after its first page
has covered the top, and the next run walks what is under it. The spans
are over top-level messages only; a reply's `ts` never moves them. The
newest message stored is never read as "fetched up to here".

The range wanted has no top. A walk covers up to the newest message it
saw, not up to "now", so a run asks a channel for what is newer than
that, and a message that only becomes visible late is not skipped. It
does so only when `client.counts` says there is something newer, or
says nothing ([which conversations a run
walks](#which-conversations-a-run-walks)). A walk that finds no
message at all covers one point, `lo = hi`, where it began, so a reader
can tell "read, nothing there" from "never read". Above that point is
owed like the stretch above any newest message.

A span's ends are `ts`es padded to one width (`ts_key`), so they sort as
instants do. The TNG fixtures' stardate `ts`es have an eleventh digit.

**Threads.** At the start of the walk, `RawDb::threads_listed` lists
every stored root with replies at the `latest_reply` its payload
carries, and `datalib_etl_web::owed` subtracts the threads held at that
version (`held_version` in `threads_bookkeeping`, keyed like the root).
It is a query over every stored root, not over the roots this run
happened to list, so a root stored by a run that died before its replies
is owed. Each history page then adds the threads its own roots owe. A
thread is one record whose fetch is paged: the walk asks for one thread
per request, `conversations.replies` is walked to its end, and
`owed::write_answer_into` stores the thread's row and messages, deletes
the stored replies the walk did not return, and holds the thread at the
version it was listed at, all in the run's open transaction. A walk cut
off part way stores nothing of the thread.
Until a thread is read, its root's page says how many replies are not
mirrored yet ([`../slack_render/TRANSLATE.md`](../slack_render/TRANSLATE.md)
§"Replies not mirrored yet").

A root's `latest_reply` only moves when history lists the root again,
and outside the refresh window it never does. So before the channels are
walked, the run searches for the replies posted since its last search
(`ingest/reply_search.rs`). Each conversation's reply time is a range
whose searched stretches are `coverage` spans, so a run that has not
happened for months searches the months, and one cut off part way leaves
a smaller gap. The searched range stops half an hour short of the run's
now, because a reply is searchable a little after it is posted.
`search.messages` returns a page of a stretch newest first, and each
page settles its stretch the way a history page does. A reply newer
than its stored root's `latest_reply` sends that root to be read again
(`conversations.history` at exactly the root's `ts`), in the transaction
that settles the page, and the thread is then owed like any other.

- A match carries no `thread_ts`. Its `permalink` does, which is how a
  reply is told from a root.
- `after:` and `before:` take whole days, excluded, in the account's own
  time zone, so a query reaches two days past each end of its stretch.
- One query names up to 20 conversations as `in:<#id>` filters, which
  Slack ORs.
- A conversation whose history nothing has covered yet is not searched:
  the walk ahead reads every root it has fresh. Its reply time up to now
  is settled at the start of the run.
- A store from before the search has walked history and searched
  nothing, so its first run searches back to `since`, once.
- Only a token that may search can call `search.messages`. A browser
  session's can; a refusal is a `problems` row and costs only the search.

**Files.** See [Attachments](#attachments).

A message that did not change is rewritten identically, which doltlite
stores as no change. That needs one field set aside: Slack mints a new
`block_id` on every read for a rich-text block the poster gave none, so
`blocks[].block_id` is volatile (`MESSAGE_VOLATILE_PATHS`) and kept in
`messages_bookkeeping.volatile_payload`. A message stored before that
loses its block ids from the content row the next time it is read, once.

## The order a run fetches in

Newest first, across every conversation the run mirrors, so a run cut
off part way has stored the newest of the workspace rather than the
whole of whichever channels were listed first (`ingest/frontier.rs`).

Everything owed waits in one of three places, each in order of the
newest message fetching it brings:

- each channel's history walks, paused between pages, at the `ts` the
  next page reaches down from;
- the owed threads, at their `latest_reply`;
- the owed files, at their message's `ts`.

The next request is for a thread, then files, owed at or above the
newest page any walk can ask for now, and otherwise for that page. Every
channel's first page is asked for before any channel's second, because
until it answers nothing says where a channel's newest message is. So a
thread with a reply this morning on a root from last year is read
before last month's history, and last year's history comes last.

This changes only when a request is made, never what it asks: every
request is one the walk of a single channel would make, with the same
`oldest`, `latest` and cursor. Nothing about the order is stored. Each
run works out what is owed from the store, as above, and orders that.

### Three requests at once

The walk keeps up to three requests out (`AT_ONCE`), or fewer when the
source's `common.download_params.maximum_requests_at_once` is lower.
Each is the next one the rule above picks from what is not already
asked for. A channel with a page out asks for nothing more of its
history until that page answers, since the next page's cursor comes
from it; meanwhile other channels' pages, threads and files go out. A
first sync therefore asks for three channels' first pages together.
With one request at a time the order is exactly the rule above; with
three, a thread or a file owed below a page that is out can go before
that page answers.

Answers are written by the one writer in the order they were asked
for, whatever order they arrive in. What the walk sees between two
requests, and so every request it makes after, does not depend on
which answer came first, and the store a run leaves is the one a run
making one request at a time leaves (`tests/slack_tests/requests_at_once.rs`).
A request that is out never reads or writes the store: the store has
one connection, the writer's, and a request parked holding it would
stall the write ahead of it. So a batch of files has its file objects
read, through the writer's transaction, before its request goes out. A stop ends the walk at once; what
is out, and an answer that arrived after the stop, is dropped unwritten
and stays owed. Rate limits and retries stay per request, in the
shared transport ([Rate limits](#rate-limits)).

Three is what was measured: on one workspace, pages of 999 three at a
time read history 5.5 times as fast as pages of 200 one at a time, and
Slack rate-limited none of it. Slack limits each method on its own, so
more at once would mostly wait out 429s.

### One transaction between two seals

Everything the walk writes, and the authors looked up before and after
it, goes through one SQL transaction at a time
(`datalib_etl::raw_store::Batch`): history pages, threads and file
edges through `owed::write_answer_into`, the sweep markers, and the
users through `owed::drain_into`. The transaction commits when a seal
is due, and at the end of the run. Every read the walk makes goes
through it too, since it holds the store's one connection.

A doltlite `COMMIT` writes a fresh copy of every page of the tree it
touched, and the copies it replaced stay in the file, since nothing
runs `dolt_gc()` on a raw store ([`docs/dev/doltlite.md`](../../../../../docs/dev/doltlite.md)
§"What a write costs"). A transaction per page of history and per
thread therefore grew the store several times over what it keeps.
Measured on a synthetic workspace through playback (20 channels of
1,000 messages in pages of 200, a thread of 4 replies on every 20th
message; no media, no seal due mid-run):

| writes | store after the first run | after a second run that finds nothing new | first run |
|---|---|---|---|
| a transaction per page and per thread (1,100) | 84.8 MB | +29 KB | 1.7 s |
| one transaction | 16.5 MB | +29 KB | 1.6 s to 2.1 s |

Committing more often bought nothing: the rows between two seals are no
safer for it, since a writer's open discards whatever no seal reached.
A stop still ends the run at a seal, and what is owed is still a query
over the store, so a run cut off anywhere is finished by the next
(`tests/slack_tests/every_cut.rs`). What a run killed between seals
fetched, the next run fetches again.

An open transaction holds its writes in memory, at about twice their
size, and a long first sync's seals come hours apart. So the
transaction also commits, without a seal, once the messages written
into it weigh `TRANSACTION_BYTES` (32 MB of their JSON).

The listings and searches before the walk (users, channels, the
account's own state, the reply and involves-me searches) still commit
as they go: they write a few small pages each.

## Noticing a deleted message

Slack never tells us a message was deleted. There is no tombstone and no
"what changed since" endpoint — a deleted message just stops appearing in
`conversations.history`. The only way to see one go is to re-ask for a
stretch of history we already mirrored and compare.

We do that in two places. Outside them, a deletion is invisible to us,
and that is worth knowing before you rely on it.

### Top-level messages: inside the refresh window, and only there

`refresh_window_days` makes a run re-read the last N days of what each
conversation has covered, at most once every `refresh_every_minutes`
(60 unless set; `0` re-reads on every run). The last re-read of each
conversation that finished is stamped in the store (`sync_scope_state`,
scope `slack:sweep:refresh:<channel>`), so one cut off or failed part
way is due again on the next run. An app that syncs every few minutes
pays for the window once an hour rather than every time. A top-level message we hold in that range that
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

It defaults to 30 days. Without it we would notice no deleted message
and no edit. (A new reply on a thread is found by the reply search
instead; see "Threads" above.) Set it to how far back you want those
caught, or to `0` to turn the pass off:

```toml
[steps.params.api]
refresh_window_days = 7
refresh_every_minutes = 360
```

The window costs one extra pass over that many days of every channel each
time it is due, so `refresh_window_days` and `refresh_every_minutes`
trade API calls against how far back, and how soon, a deletion is
spotted.
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
| `common.blob_size_limit_bytes` raised | The edges skipped for their size are still without bytes, so they are fetched. |
| a new channel, or `dms` on | The conversation has no coverage, so it is walked from `since` once `client.counts` shows a message in range, names nothing, or leaves out a DM whose check is due (see [Which conversations a run walks](#which-conversations-a-run-walks)). |
| `archived` on | Each archived channel the walk now takes in has no coverage, so it is walked from `since`, once. |
| `archived` off | Nothing. What is stored stays; the archived channels are no longer walked. |

## What appeared since the last sync

Slack has no call that answers "what changed since then" for channels
or users, so each is found its own way.

- **Channels.** Every run lists `conversations.list` whole, so a channel
  created a minute ago, or the DMs `dms = true` just asked for, are
  walked by the next run. It asks for 1000 a page, archived channels
  included ([Archived channels](#archived-channels)); on a real
  workspace 850 channels took 5 calls at 1000, where its 381 open ones
  had taken 23 at 200.
- **Users.** `users.list` is listed at most once per `list_every_hours`
  (six by default), since its rows change only when someone edits a
  profile. A
  person who posts for the first time does not wait for it: storing a
  message adds a `users` row with no payload for an author not already
  there, and each run then asks `users.info` for every such row, before
  its walk and again after it, so the authors a stopped run left are
  the next run's first requests. The
  answer is stored as `users.list` lists it (without the custom
  `profile.fields` only `users.info` carries), so the next listing is no
  change. A lookup that fails is an attempt on the user's own row
  (`users:<id>`), asked again each run until it answers or a
  `users.list` names the user.

A sweep marker only says "do not list again yet"; it is not progress.

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
- **An author** whose `users.info` fails is an error on the user's own
  row (`users:<id>`), cleared by the lookup or listing that names them.
  A token that may not call `users.info` at all ends the lookups as
  one `phase:users.info` row.
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

**A mirror with no messages** after a run that walked at least one
channel cleanly is a `silent:channels` warning. Slack answers a
channel with nothing in range by an empty page and `ok: true`, so a
range that holds nothing looks like a successful sync without it. A run that was stopped leaves the row as it was; the first run
that ends with a message in the store clears it.

### A call this token may not make

`client.counts` and `saved.list` answer only some tokens. A token
Slack refuses them (`unknown_method`, `not_allowed_token_type`,
`missing_scope`), or a request latchkey's permissions or its gateway
do not allow (the gateway's own 403), will be refused the same way
every sync until someone changes the token. Asking anyway put a
warning or an error on the Manage row of every sync, though nothing
else was missing.

So a refusal is remembered: a `refused:<method>` sweep marker
(`slack:sweep:refused:client.counts` in `sync_scope_state`), stamped
with the run's `now`. While it is younger than `REFUSAL_REMEMBERED_FOR`
(a week) the call is not made, and the run writes one `info` row,
`listing:<method>`, saying when it was refused, when it will be asked
again, and what goes without it. `info` is not counted on the Manage
row; the problems grid shows it. Once the marker is a week old the
next run asks again: an answer forgets the marker and the row goes, a
refusal stamps it anew. A failure that may pass (an HTTP 5xx, curl
failing, Slack's `internal_error`) is not a refusal: it is the usual
error row and the call is asked again next run. A reset of the
download forgets the marker along with everything else.

Without `client.counts` a run walks every conversation, as when the
call fails, and the read marks it carries stay as they were; without
`saved.list` the saved items stay as they were.

## Attachments

Each file a stored message carries that Slack serves (not a tombstone,
not hosted elsewhere) is a `slack_attachments` row, written without a
`blake3` in the transaction that stores the message, whether or not
`media` is on. The bytes are owed from then on.

When `media` is on, the owed files are fetched in the run's
newest-first order (see [The order a run fetches in](#the-order-a-run-fetches-in))
and stored through `owed::write_answer` (`ingest/files.rs`), from the
file object the stored message carries: a request is `FILE_BATCH` files,
and storing them puts their bytes into the blob CAS (its own file, which
commits itself) before the run's open transaction gives their edges a
`blake3` and holds them, so a kill between leaves only unnamed bytes. A
file whose bytes we already hold, under any message, is not fetched
again, unless two batches out at once both carry it: the CAS keeps one
copy either way.

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
A refused `client.counts` or `saved.list` is not asked again for a week
and is an `info` row instead
([A call this token may not make](#a-call-this-token-may-not-make)).

- **`channel_read_states`**, from one `client.counts` call: each
  conversation's `last_read`, `latest`, `has_unreads` and
  `mention_count`. Its `latest` also decides which DMs are walked
  ([Which conversations a run walks](#which-conversations-a-run-walks)).
  `last_read` goes to `read_marks` (below); **the rest is volatile**:
  the content payload is just `{"id": …}`, and the state lives in the
  `volatile_payload` column of `channel_read_states_bookkeeping`, so a
  new `latest` alone does not wake the render.
  `RawDb::load_read_states` lays the three back together. Upserted every
  run; never pruned.
- **A followed thread's own mark.** `conversations.replies` puts
  `last_read` and `subscribed` on its copy of the root of a thread the
  account follows (the `conversations.history` copy has neither).
  `last_read` goes to `read_marks`; `subscribed` is volatile on
  `messages` (`MESSAGE_VOLATILE_PATHS`), so following a thread is not
  an edit to its root. A copy without a mark leaves the thread's mark as
  it was, and the sidecar is merged by top-level key, so the history
  copy takes neither away. A thread is only re-fetched when it has a
  new reply, so its mark is as fresh as its last reply, not as the last
  sync.
- **`read_marks`** (`datalib_etl::read_marks`), the account's read
  marks, `last_read` in `read_through`, and nowhere else in the store.
  A conversation's mark is keyed by its conversation id, a followed
  thread's by its thread key, `{team}#{channel}#{ts}`; the two cannot
  collide, as a thread key has two `#` and a conversation id none
  (`schema_raw::is_thread_mark`). A mark is content: reading a channel
  moves the store's version and wakes the render, which redraws only
  the threads the mark crossed. A thread's mark goes with the thread
  when history prunes it. A store from before the table has its marks
  moved out of the two sidecars by rung 2 of the raw ladder.
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
  per `list_every_hours`, like the user list. The listing is not paged,
  so it is the conversation's whole set, and a bookmark it no longer
  names is deleted.

**Slack Lists are not mirrored.** `slackLists.*` answers the session
token with `not_allowed_token_type`, and no List turned up through
`files.list` or search on the workspace we checked.

Render reads both marks: a top-level message after its conversation's
`last_read`, or a reply after its followed thread's, renders unread
([`slack_render/TRANSLATE.md`](../slack_render/TRANSLATE.md)).

## What a run asks first

Before any history, a run makes the calls that tell it what to walk.
The ones that need nothing from each other go out together, up to the
source's `common.download_params` limit on requests at once:
`users.list` beside `conversations.list`; then `client.counts`,
`saved.list` and the bookmarks beside the reply search. Each page of
one listing still waits for the page before it, since its cursor
names where the next one starts. The walk after them keeps up to three
requests out ([Three requests at once](#three-requests-at-once)).

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
