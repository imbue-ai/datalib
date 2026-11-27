# Discord ingest

The ingest step of a `discord` group mirrors what one Discord account
can read into `<data_root>/<group>/ingest/entities.doltlite_db`, with
the blob CAS beside it: the channels, threads and forum posts of the
servers its config names, and the account's direct messages.

Discord's API calls a server a *guild*; the tables keep Discord's
words, and everything a person sees says "server".

## Auth, and what Discord says about it

The download signs in as a person, not as a bot. `latchkey auth browser
discord` opens Discord's own login and keeps the session token the app
got; every request goes out as `latchkey curl`, which puts that token on
it ([`latchkey.md`](/docs/dev/latchkey.md)).

**Discord forbids this.** Its support article "Automated user accounts
(self-bots)" says that automating a user account outside its bot API is
forbidden and can get the account terminated. A bot token is the way it
allows, and it cannot do what this source is for: a bot sees only the
servers whose admin adds it and never a person's direct messages, and
the endpoint that lists a channel's threads for a user answers a bot
403. So the wizard says the risk before the sign-in, and the download
stays as small as it can: it only ever sends `GET`s, its API requests go
one at a time, and a conversation with nothing new costs none, but for
its refresh-window re-read ([Edits, reactions and
deletions](#edits-reactions-and-deletions)). It sends
curl's own user agent and no header pretending to be the app.

## API surface used

`https://discord.com/api/v9`, every one a `GET` (`src/ingest/api.rs`):

| Endpoint | Purpose |
|---|---|
| `/users/@me` | who the account is |
| `/users/@me/channels` | its direct messages |
| `/users/@me/guilds` | the servers it is in |
| `/guilds/{id}/channels` | a server's channels, the ones it cannot read included |
| `/guilds/{id}/roles`, `/users/@me/guilds/{id}/member` | what it may read |
| `/channels/{id}/threads/search` | a channel's threads, a forum's posts |
| `/channels/{id}/messages` | a page of a conversation |

Files come from `https://cdn.discordapp.com/`, by links Discord signs
itself, so those go out as plain `curl` with no credential and to no
other host.

## The store

Every table is `(id, payload)` plus a `<table>_bookkeeping` sidecar, and
every key is Discord's own id, a *snowflake*: a 64-bit number whose top
42 bits are the milliseconds since 2015, unique across the service.

| table | key | holds |
|---|---|---|
| `account` | user id | the signed-in account, one row, without its phone number |
| `users` | user id | everyone a stored message or direct message names |
| `guilds` | server id | the servers copied |
| `members` | server id | the account's own membership: its roles |
| `roles` | role id | each copied server's roles |
| `channels` | channel id | channels, categories, threads, direct messages |
| `messages` | `{channel}#{message}` | messages |
| `discord_attachments` | `{message key}#{file id}` | the edge from a message to a file's bytes |
| `coverage` | scope, `lo` | what has been read (below) |

A message's key holds its id padded to 20 digits, because ids have been
17, 18 and 19 digits long: a conversation's messages are one range of
the table, in the order they were sent.

### What is kept out of the content

Discord fills in people's current profiles wherever it names them, and
signs a file's links anew each day. Left in the payload, a person
changing their banner would be an edit to every message they ever wrote.
So the sidecar's `volatile_payload` takes (`schema_raw.rs`):

- a message's copies of who it names, down to their ids. Who they are
  is in `users`. A webhook's author stays whole: its name is the
  message's own, and is nowhere else.
- `referenced_message` (the message it answers) and `thread` (the one it
  started), which are records of their own.
- a file's links, and what Discord works out of a file or a preview
  later (`content_scan_version`, `placeholder`).
- a channel's newest message and its counts, and whether a thread is
  archived just now.

Three lists that come in no order are sorted: a server's `features`
(seen in another order on every request), a message's `mention_roles`
and `mentions`, a channel's `permission_overwrites`.

## Which conversations a run reads

`servers` names servers, by name or id; `all_servers` takes them all;
neither takes none. `channels` narrows them to channels named by id,
name, link or category. `dms` and `dm_conversations` do the same for
direct messages. A name Discord does not have is a `config:` problem,
and costs only itself.

Of a server's channels, text, announcement, voice and stage channels
hold messages; text and announcement channels, forums and media channels
hold threads. A type not among those is not read (the live API answered
a type 20, which the published reference does not list).

**What the account may read is worked out, not tried.** Discord lists
every channel of a server to every member. Its permission rules are
applied to the roles and each channel's overwrites (`visible.rs`), and a
channel the account cannot read is never asked for.

**Threads.** `threads/search` answers a channel's threads, archived or
not, by newest message, 25 at a time. It is asked for each channel and
forum at most every `list_every_hours` (6), down to where an earlier
listing reached. A thread started since shows with the message that
started it, so it is read on the same run; a new message in an older
thread waits for the next listing. A private thread is listed only to
someone in it.

## What a run still owes

Nothing in the store is a cursor, and nothing is marked done
([listed minus held](/docs/dev/data_architecture_ingestion.md#what-is-left-to-fetch-listed-minus-held)).

| Kind | Wanted | Held | Owed |
|---|---|---|---|
| a conversation | everything from `since` up to its newest message, as listed | `coverage` spans of scope `history:<channel>` | the gaps |
| a channel's threads | every thread, by newest message | `coverage` spans of scope `threads:<channel>` | the listing down to the first stretch held, when one is due |
| a file | an edge, written with its message | the edge's `blake3` | an edge with none, when `media` is on |

Every listing names a conversation's newest message
(`last_message_id`), so one whose newest message is covered costs no
request. That id may name a message since deleted (seen on 6 of 14
channels), so a walk covers up to it even when the page lacks it, and
the next run does not ask again.

Pages come newest first, 100 at a time, and are asked for newest first
across every conversation: a run cut off part way has the newest of the
account. A page is stored with the people it names, the edges of its
files and the stretch it settles, in the run's open transaction. A page
shorter than 100 is not read as the end; an empty one is, for everything
older.

`tests/discord_tests/interrupt.rs` cuts a run off at every request, from
an empty store and from an earlier one, and requires the next run to end
where an uninterrupted one does.

**Files.** A page's files are fetched right after it, three at a time.
A link works for about a day (`ex` in its query says until when). A file
owed from an earlier run whose link has run out has its message read
again, with `around`, for a new one: a single message has an endpoint of
its own, which only a bot may call. A file over
`common.blob_size_limit_bytes` is a warning on it and is judged against
the limit again each run.

## Edits, reactions and deletions

Discord lists none of them. A run re-reads the trailing
`refresh_window_days` (7) of each conversation that holds a message that
new, at most every `refresh_every_minutes` (360). A run that reads the
whole window for the first time has refreshed it too. What it returns
replaces what is stored; a stored message inside the stretch that it
does not return was deleted, and goes, with its files' edges, in that
page's transaction. A message edited or deleted after it left the window
stays as it was last read. History keeps what a prune took
([`doltlite.md`](/docs/dev/doltlite.md)).

## When part of a sync fails

A listing or a conversation that fails is a `listing:` row and the sync
goes on. One the account may not read (403, "Missing Access") or that is
gone (404) is a warning: asking again would be answered the same. A
refused credential ends the run. A 429 is waited out, by its
`Retry-After` or the `retry_after` in its body, whichever is longer.

## What it does not do

- **Who reacted.** A message counts its reactions and says whether one
  is the account's own. Who else reacted is a request per emoji per
  message.
- **Read state.** Discord tells only its app, over the gateway, how far
  the account has read.
- **Server nicknames.** A message carries its author's profile, not the
  name they go by on that server.
- **A channel, thread or server that is deleted** stays as it was: its
  conversation is no longer listed, and nothing says why.
- **A direct message closed in the app** leaves the account's list, and
  is read again when a message opens it.
- Pins, calls, events, emoji and sticker pictures.

## Checked against the live API

On 2026-11-24, against one account in two servers, with about 55
requests whose keys, types and counts were looked at and nothing else:

- plain curl is answered, with no browser impersonation;
- `messages` answers newest first, `before` is exclusive, `after`
  answers the messages next to it, and `around` includes the one named;
- `threads/search` with no `archived` answers both, in order of newest
  message; `/guilds/{id}/threads/active` and a single message answer
  403, code 20002, "Only bots can use this endpoint";
- 413 messages read again ten minutes later had not changed at all, and
  neither had 18 channels; a server's `features` came in another order;
- a deleted or unknown channel answers 404, code 10003.

On 2026-11-25, a sync of one day of that account (`discord_live`,
latchkey 3.25.0): 239 conversations listed over 2 servers and 4 direct
messages, 2 of them with anything to read, 6 messages, 2 files, 39
requests, no problem, and both documents rendered.

Not yet seen live: a 429, a webhook's or an app's message, a forward, a
poll, a private thread, a group direct message, a channel hidden from
the account. Their shapes are from Discord's reference.

## Looking at it

```sh
datalib-doltlite -readonly <root>/discord/ingest/entities.doltlite_db \
  "SELECT channel_id, COUNT(*) FROM messages GROUP BY 1 ORDER BY 2 DESC LIMIT 10"
datalib-doltlite -readonly <root>/discord/ingest/entities.doltlite_db \
  "SELECT scope, lo, hi FROM coverage WHERE scope LIKE 'history:%'"
```

## Sample data

`tests/fixtures/discord_tng/` is Picard's account in the
[TNG corpus](/tests/fixtures/README.md): `world.json`, everything it can
be answered, and the files its messages carry.
`tests/fixtures/make_discord_fixtures.py` writes it, and its header lists
the cases it covers. `src/synthesize.rs` turns it into playback tapes.
`tests/discord_tests/world.rs` builds smaller accounts for the walk's own
tests. A snowflake cannot say a date after 2154, so the crew chat in
2024.
