# Reading the raw stores

Every source's `ingest` step keeps what the service sent, as close to
the wire as it can: Slack's and Google Calendar's JSON per record,
Gmail's whole original message. That copy is the **raw store**, one
per source, in `<data_root>/<group>/ingest/`, where `<group>` is the
source's id. This guide is for a person or a program building on those
stores: how to open one, how to read it at one moment and follow what
changes, and, source by source, what each table and column holds.

Most questions are better answered from the rendered side first: the
`grid_rows` table, the markdown, the search
([`agent_user.md`](../agent_user.md) § "Reading the mirrored data").
Come here when you need a field the render dropped, an upstream id to
call the service with, or the original bytes.

This guide covers Gmail and the other mail sources (`email`), Slack
(`slack`) and calendars (`calendar`). For any other source,
`.schema` in the shell below lists its tables; their meanings are not
written down here yet. A test (`raw_stores_doc_test.rs` in
`datalib/backend/datalib_step`) checks every table and column this
guide names against the tables the code creates, so a column added or
renamed fails the build until this page says what it holds.

## Opening a store

A source's raw directory holds two files:

| file | what it is |
|---|---|
| `entities.doltlite_db` | every table below, with its history: a doltlite store |
| `blobs.sqlite` | the bytes of attachments and whole emails, by hash: a plain SQLite file |

**Stock `sqlite3` cannot open `entities.doltlite_db`** (`file is not a
database`). Doltlite is SQLite with git-shaped history kept inside the
file. Read it with **`datalib-doltlite`**, which ships beside
`datalib-dag` (as plain `doltlite` in the Docker image) and takes
`sqlite3`'s arguments. The examples assume sources whose ids are
`gmail`, `slack` and `calendar`; put your own in. **Always pass
`-readonly`**: a writable session's commits are overwritten by the
next sync without an error, and a transaction it leaves open stalls
the sync.

```sh
cd <data_root>
datalib-doltlite -readonly gmail/ingest/entities.doltlite_db ".tables"
datalib-doltlite -readonly gmail/ingest/entities.doltlite_db ".schema emails"
datalib-doltlite -readonly -json gmail/ingest/entities.doltlite_db \
  "SELECT id, subject, received_at FROM emails LIMIT 5"
```

From a program, the simplest route is to run `datalib-doltlite
-readonly -json` as a subprocess and parse its output. For a tool that
speaks only SQLite, one pipe writes a plain SQLite copy with the same
tables (the data, not the history):

```sh
datalib-doltlite -readonly gmail/ingest/entities.doltlite_db .dump | sqlite3 gmail.sqlite
```

The copy keeps `payload` as JSONB (below), which SQLite reads from
version 3.45 on.

`blobs.sqlite` is plain SQLite, so any SQLite library reads it. It has
one table, `cas_objects (blake3, byte_len, content_type, bytes)`: each
row is filed under the blake3 hash of its bytes, in lowercase hex, and
never changes once written. A doltlite session can attach it
read-only to join across the two files:

```sh
datalib-doltlite -readonly gmail/ingest/entities.doltlite_db \
  "ATTACH 'file:gmail/ingest/blobs.sqlite?mode=ro' AS cas;
   SELECT e.id, c.content_type, c.byte_len
     FROM emails e JOIN email_blobs b ON b.email_id = e.id
     JOIN cas.cas_objects c ON c.blake3 = b.blake3 LIMIT 5"
```

### What you are reading: sealed state

A download writes on a branch of its own and moves the store's `main`
branch only when it **seals**: at the end of a run, and at checkpoints
along the way. A read-only open lands on `main`, so you never see half
of a batch. During a long first sync the store is readable from the
first checkpoint on, and lags the download by up to one checkpoint
interval (see [building on datalib](building_on_datalib.md) § "How
fresh is the mirror?").

## `payload` is JSON, stored as JSONB

Most tables keep the record as the service sent it in a `payload`
column. It is stored as SQLite's binary JSON (JSONB), so selecting it
bare prints bytes. Ask for `json(payload)` to see the text, and use
SQLite's JSON functions on it directly:

```sql
SELECT json(payload) FROM messages LIMIT 1;
SELECT json_extract(payload, '$.text'), payload ->> '$.user' FROM messages LIMIT 5;
SELECT e.id, m.key AS mailbox
  FROM emails e, json_each(e.payload, '$.mailboxIds') m;
```

The other columns of a payload table are **promoted** copies of a few
payload fields, for indexes and cheap filters. The payload is the
record; the columns are conveniences.

A few fields change on every read without the record changing (Slack
bumps a channel's `updated`, Google an event's `etag`). Those are moved
out of `payload` into `volatile_payload` on the table's `_bookkeeping`
row, so the history shows only real changes. Each source's section says
which fields move.

## Reading one commit, and following changes

Every seal is a commit. Its hash names the store exactly as it was, and
the hash of `main` is the store's current version:

```sh
datalib-doltlite -readonly slack/ingest/entities.doltlite_db "SELECT dolt_hashof('main')"
```

**To read several queries at one moment**, open that commit by path
(with `/` in the shell, not `@`). The open is read-only and stays on
that commit however far the sync moves on:

```sh
datalib-doltlite -readonly "slack/ingest/entities.doltlite_db/<hash>" \
  "SELECT count(*) FROM messages"
```

**To follow changes**, keep the last hash you processed as your
cursor. Each time, read the new hash, ask what changed between the two
with the table's `dolt_diff_<table>`, do your work, and only then store
the new hash:

```sh
datalib-doltlite -readonly -json gmail/ingest/entities.doltlite_db \
  "SELECT diff_type, from_id, to_id FROM dolt_diff_emails('<cursor>', '<new hash>')"
```

`diff_type` is `added`, `modified` or `removed`; every column comes
twice, as `from_<column>` and `to_<column>`. A `removed` row is one the
service no longer has and the mirror has dropped; the old commit still
holds it (`SELECT * FROM dolt_at_emails('<cursor>') WHERE id = …`).
Name both commits as arguments, as above: a filter on `from_commit` or
`to_commit` does something else. More recipes, and what each costs,
are in [`doltlite.md`](../dev/doltlite.md#what-changed-between-two-commits).

A raw store's history is never folded away (`datalib-dag --compact`
leaves raw stores alone), so a cursor stays readable. A reset of the
source empties the tables in a new commit, which reads as every row
`removed`. If your cursor no longer resolves (the store was deleted and
synced again), start over from a full read. A `datalib-dag diff` verb
that would do this loop for you is proposed in
[#637](https://github.com/imbue-ai/datalib/issues/637) and not built.

## What every raw store has

Besides each source's own tables, a store holds these. They are the
download's record of its own work. `problems` and `sync_runs` are worth
reading; the others are listed so nothing in `.tables` is a mystery.

### `<table>_bookkeeping`

One sidecar per record table, keyed like it (`emails_bookkeeping` for
`emails`): what fetching that record cost and what is set aside from
its payload.

| column | what it holds |
|---|---|
| `id` | the record's `id` |
| `fetched_at_utc` | when a fetch of it last landed, in UTC; NULL if none ever has |
| `attempt_count` | how many times a fetch of it has been tried |
| `last_attempt_at_utc` | when the last try was, in UTC |
| `last_error` | what the last failed try said |
| `volatile_payload` | the payload fields that change on every read (JSONB), named per source below |
| `tz_offset` | the offset the writer's clock was in, for the two stamps |
| `held_version` | the version of the record its stored content satisfies, where the download tracks one (a Gmail history id, a Slack thread's newest reply, a CalDAV etag) |

### `problems`

What the download could not do, one row per thing: a record that would
not fetch, a label that does not exist, a listing that failed part way.
A row stays until a later run tries the same thing and it works. The
app shows these as the count on the source's row and the banner on a
document.

| column | what it holds |
|---|---|
| `problem_uuid` | a stable id for this problem, the same each time it recurs |
| `source_id` | empty here; the render step fills it when it copies the row |
| `stage` | which stage found it: `fetch` in a raw store |
| `severity` | `error`, `warning` or `info` |
| `outcome` | what became of the record: `dropped` (not in the mirror), `nulled` (in it, missing something), `ok` |
| `reason` | why: `fetch_failed`, `over_size_limit`, `not_found`, `forbidden`, … |
| `scope_kind` | `entity` in a raw store |
| `scope_key` | what it is about, as `<table>:<id>` for a record (`emails:018f…`), or `listing:…`, `phase:…` |
| `item_uuid` | empty here; render fills it with the document's row |
| `field` | the field it is about, when it is one field |
| `path` | where in the payload, as a JSON pointer, when known |
| `rule` | the rule that fired, when the loss was deliberate |
| `sample` | what was said: the error, or the start of the offending value |
| `first_seen_at_utc` | when it was first recorded, in UTC |
| `changed_at_utc` | when it last changed, in UTC |
| `tz_offset` | the offset for the two stamps |
| `render_version` | empty here; set on rows a render records |

### `sync_runs`

One row per download run.

| column | what it holds |
|---|---|
| `run_id` | a number counting up |
| `started_at_utc` | when it started, in UTC |
| `finished_at_utc` | when it ended; NULL while it runs, or if it was killed |
| `tz_offset` | the offset for the two stamps |
| `config` | the step's params as that run saw them, JSON |
| `status` | `running`, `ok` or `error` |
| `summary` | what it did, JSON (counts, and why it stopped early if it did) |

### `sync_scope_state`

The download's own markers, one per scope it names. Despite the
column's name, the value is not always a time: Gmail and JMAP keep
their upstream change token here.

| column | what it holds |
|---|---|
| `scope` | what the marker is for, in the download's own spelling |
| `last_seen_at_utc` | the marker: a time, or an upstream token |
| `tz_offset` | the offset, when the marker is a time |

### `ingested_files`

Which local files a file-reading download has read, by path and hash.
The `.ics` method of `calendar` keeps a row per file. The `email`
store's mbox mode, and a Gmail source that starts from a Takeout, keep
a stamp of their last complete read here instead: one row per input
file, plus a row with an empty `rel_path` for the settings it was read
with, under the scope `email/mbox_read` or `email/takeout_seed`. While
the files and settings match the stamp, the run skips the read.

| column | what it holds |
|---|---|
| `scope` | which input the file belongs to |
| `rel_path` | its path under that input |
| `blake3` | the hash of its bytes when last read |
| `size_bytes` | its size then |
| `last_finished_at_utc` | when the read that took it in finished, in UTC |
| `tz_offset` | the offset for that stamp |

## Gmail, Fastmail and mbox (`email`)

All three ways into mail write one schema. Fastmail and other JMAP
servers send JMAP's own objects; the Gmail API and mbox files are made
to look like them, so `payload` is a JMAP-shaped **envelope**: who,
when, subject, labels. **There is no body in the envelope and no
snippet.** The message itself, headers, body and attachments, is the
original RFC 5322 `.eml`, stored whole in `blobs.sqlite`.

**Which id is which, for Gmail:**

| you have | it is in the mirror as |
|---|---|
| a message id from the Gmail API or a Gmail URL (`18f2a3b4c5d6e7f8`) | `emails.id`, in lowercase hex zero-padded to 16 digits |
| a thread id from the Gmail API or a Gmail URL | `emails.thread_id`, `threads.id`, in the same padded hex |
| a message or thread id in decimal (Takeout's `From ` line or `X-GM-THRID`) | the same number in hex: `printf('%016x', <decimal>)` |
| a label id (`INBOX`, `Label_7`) | `mailboxes.id` = `gmail:<account id>:<label id>` |
| a `Message-ID:` header | `emails.message_id`, without the angle brackets |

A Gmail id is padded so that the text sorts as the number does; the
API spells an id from Gmail's first months with only 15 digits, and the
mirror adds the leading zero. `listed_messages.id` and
`thread_members.id` keep the API's own spelling. `label_ids` is set
only on a message the Gmail API has answered for, so it is NULL for
Fastmail, for an mbox, and for a message a Takeout seeded until the
next sync asks Gmail for its labels.

**Getting the message.** Join `email_blobs` on the email, then
`cas_objects` in `blobs.sqlite` on its `blake3`. The row's
`content_type` is `message/rfc822` and `bytes` is the message as the
server sent it. Its headers are raw: folded across lines and, for
anything not ASCII, RFC 2047-encoded (`=?UTF-8?B?…?=`). Hand the bytes
to a mail parser rather than reading headers by hand. An `.eml` larger
than the source's `blob_size_limit_bytes` is not stored; its
`email_blobs.blake3` stays NULL and a warning in `problems` says so.

**Labels are split two ways**, the way JMAP models them. A label that
files mail (Inbox, Sent, a label you made, the inbox categories) is a
row in `mailboxes`, and an email's labels are its `email_mailboxes`
rows (also `payload -> '$.mailboxIds'`). A label that is a flag is a
keyword in `email_keywords` (also `payload -> '$.keywords'`):

| Gmail | in the mirror |
|---|---|
| no `UNREAD` label | keyword `$seen` (so unread is the *absence* of `$seen`) |
| `STARRED` | keyword `$flagged` |
| `IMPORTANT` | keyword `$important` |
| `INBOX`, `SENT`, `DRAFT`, `TRASH`, `SPAM` | a mailbox with `role` `inbox`, `sent`, `drafts`, `trash`, `junk` |
| `CATEGORY_*`, your own labels | a mailbox with no role |
| muted | not kept |

For Gmail, `emails.label_ids` puts them back together: every label id
the message carries, as the API spells them, `UNREAD`, `STARRED` and
`IMPORTANT` included, as a sorted JSON array (`["INBOX","UNREAD"]`).
`EXISTS (SELECT 1 FROM json_each(label_ids) WHERE value = 'INBOX')`
asks whether a message is in the inbox.

**Traps.**

- **`received_at` is not Gmail's receipt time.** For Gmail and mbox it
  is the message's own `Date:` header (Gmail's receipt time only when
  the header is missing or unreadable), and `sent_at` is the same
  value. For JMAP it is when the server received the message. Either
  way it keeps the sender's UTC offset, so **text order is not time
  order**: compare and sort with `datetime(received_at)`.
- **A label filter keeps whole threads.** With `only_extract_labels`
  set, a message the filter keeps brings every other message of its
  thread, whatever its labels or age, so a conversation whose start was
  archived out of the inbox is all there. A thread none of whose
  messages the filter keeps any more goes. A source with
  `whole_threads = false` keeps only the matching messages; then
  `threads.email_count` (what the mirror holds) can be less than
  `threads.upstream_email_count` (what the service has), and that is a
  thread held in part.
- **`emails.blob_id` is not the CAS key for JMAP** (it is JMAP's own
  blob id). Always go through `email_blobs.blake3`.

### `accounts`

One row per mail account the source mirrors.

| column | what it holds |
|---|---|
| `id` | JMAP's account id; for Gmail the configured `account_id`, by default the address Gmail reports; for mbox the configured `account_id` |
| `payload` | the account object (JMAP's; for Gmail `{id, name, email, isPersonal, _source}`, where `_source` names the API it came from) |
| `name` | its display name |
| `email_address` | the address it signs in as: Gmail's, a JMAP server's sign-in name on the personal account when that is an address, mbox's configured one; NULL when none is known |
| `is_personal` | 1 for the person's own account, 0 for a shared one |
| `is_read_only` | 1 if the server says it is read-only |

### `mailboxes`

One row per mailbox or label.

| column | what it holds |
|---|---|
| `id` | JMAP's mailbox id; for Gmail `gmail:<account id>:<label id>`; for a Takeout label known only by name, `mbox-` and a hash of the name |
| `payload` | the mailbox object; for Gmail `{id, name, role}`. Its message counts are in `volatile_payload` |
| `account_id` | the `accounts.id` it belongs to |
| `name` | its name; for Gmail the full label path (`Work/Projects`) |
| `parent_id` | the parent mailbox's id, where the server nests them |
| `role` | `inbox`, `sent`, `drafts`, `trash`, `junk`, `archive`, or NULL |
| `sort_order` | the server's sort order, where it has one |

### `threads`

One row per conversation that has a message in the mirror.

| column | what it holds |
|---|---|
| `id` | the thread id, as `emails.thread_id` spells it |
| `payload` | `{"id", "emailIds"}`: the ids of the emails held for it, oldest first; with `upstreamEmailCount` where that is known |
| `account_id` | the account it belongs to |
| `email_count` | how many emails the mirror holds for it |
| `upstream_email_count` | how many messages the service (or the mbox input) has in it, filter or not; NULL with no label filter, when every thread is whole, and on Gmail and Fastmail until the first walk of the account (below) has finished |

### `emails`

One row per message.

| column | what it holds |
|---|---|
| `id` | the message's id (see "Which id is which") |
| `payload` | the envelope: `id`, `blobId`, `threadId`, `mailboxIds`, `keywords`, `size`, `hasAttachment`, `receivedAt`, `sentAt`, `subject`, `from`, `to`, `cc`, `messageId`, `inReplyTo`, `references`; for a message the Gmail API answered for, also `_source: {"via": "gmail.googleapis.com"}` |
| `account_id` | the account it belongs to |
| `thread_id` | its thread: Gmail's thread id in the same padded hex as `id`; JMAP's own thread id; for an mbox message with no `X-GM-THRID`, its own `id` |
| `blob_id` | the id of its `.eml`: blake3 of the bytes for Gmail and mbox, JMAP's own blob id for JMAP |
| `message_id` | its `Message-ID:`, without angle brackets |
| `in_reply_to` | its `In-Reply-To:`, without angle brackets |
| `references_header` | its `References:`, space-separated, without angle brackets |
| `received_at` | see "Traps": an RFC 3339 time with the sender's offset |
| `sent_at` | its `Date:` header |
| `size` | the `.eml`'s size in bytes |
| `subject` | the subject, decoded |
| `from_json` | the senders, as JSON text: `[{"name": …, "email": …}]` |
| `to_json` | the `To:` addresses, the same way |
| `cc_json` | the `Cc:` addresses, the same way (there is no Bcc column) |
| `has_attachment` | 1 if the message has an attachment |
| `label_ids` | Gmail only: its label ids as a sorted JSON array (see "Labels") |

### `email_mailboxes`

Which mailboxes (labels) each email is in. Rewritten whole every time
its email is.

| column | what it holds |
|---|---|
| `id` | `<email id>#<mailbox id>` |
| `email_id` | the `emails.id` |
| `mailbox_id` | the `mailboxes.id` |

### `email_keywords`

Which keywords (flags) each email has, rewritten with it the same way.

| column | what it holds |
|---|---|
| `id` | `<email id>#<keyword>` |
| `email_id` | the `emails.id` |
| `keyword` | `$seen`, `$flagged`, `$important`, or another keyword the server or the mbox file names |

### `email_blobs`

Where each email's `.eml` is in `blobs.sqlite`.

| column | what it holds |
|---|---|
| `id` | `<email id>#<blob id>` |
| `email_id` | the `emails.id` |
| `blob_id` | the `emails.blob_id` |
| `blake3` | the `cas_objects.blake3` holding the bytes; NULL until they are stored, or when the message was over the size limit |

### `listed_messages`

Every message the service has named to the download (JMAP and Gmail
API; not mbox), whether or not it has been fetched yet. A message
listed here with no `emails` row, or held at an older version, is still
owed: [building on datalib](building_on_datalib.md) § "How fresh is the
mirror?" has the query.

| column | what it holds |
|---|---|
| `id` | the message's id as the service spells it (Gmail's hex id unpadded, JMAP's email id) |
| `stamp` | the change token that last named it (a Gmail history id, a JMAP state) |

### `listed_whole`

The mailboxes or labels (or `*` for the whole account) that a listing
has walked to its end. Until a scope has a row, the download is still
discovering what is in it.

| column | what it holds |
|---|---|
| `scope` | a JMAP mailbox id, a Gmail label id, `*`, or `#threads` for `thread_members` |

### `thread_members`

Every message in the account, mirrored or not, and the thread it is
in (Gmail and Fastmail, under a label filter). One walk of the whole
account fills it, and the changes each sync reads keep it current;
`listed_whole` has `#threads` once that walk has finished. It is how the
rest of a kept thread is found, and what `threads.upstream_email_count`
counts.

| column | what it holds |
|---|---|
| `id` | the message's id as the service spells it, as in `listed_messages` |
| `thread_id` | its `threads.id` |

## Slack (`slack`)

Slack's objects are kept as its API returned them: `payload` is the
user, channel or message JSON from `users.list`,
`conversations.list`, `conversations.history` and
`conversations.replies`.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a workspace (team) id `T…` | `workspaces.id`, and `team_id` on users and messages |
| a user id `U…` | `users.id`, `messages.user_id` |
| a conversation id `C…`, `D…`, `G…` | `channels.id`, `messages.channel_id` |
| a message's `ts` in a conversation | `messages.ts`; the row's `id` is `<team id>#<channel id>#<ts>` |
| a thread (its root's `ts`) | `messages.thread_root_uuid`, equal to the root's `messages.id` |
| a file id `F…` | `slack_attachments.file_id` |

`workspaces.self_user_id` is the account the mirror signs in as, which
is how to tell "me" from everyone else. A direct message is a
`channels` row with `is_dm = 1`; `dm_user_ids` lists who is in it.

**Threads.** `is_thread_root = 1` is set on **every top-level
message**, whether anyone replied or not; a reply has 0. Every message,
root or reply, has `thread_root_uuid` set to the `id` of the top-level
message it belongs under, so `WHERE thread_root_uuid = <root id>`
gives a whole thread, root included. A reply that was also sent to the
channel is stored as a reply. A root's payload carries Slack's
`reply_count` and `latest_reply`; the mirror holds every reply up to
`latest_reply` when the root's `threads_bookkeeping.held_version`
equals it.

**What has been mirrored: `coverage`.** "No messages in this channel
on Tuesday" and "Tuesday was never fetched" look the same in
`messages`. The `coverage` table tells them apart. Each row is a
stretch of one conversation that the download has walked: everything
Slack listed in it is stored.

- Scope `history:<channel id>` covers top-level messages, from
  `conversations.history`.
- Scope `replies:<channel id>` covers the stretches whose new replies
  the download has searched for (a reply to an old root is found this
  way).
- `lo` and `hi` are `ts` values left-padded with zeros to 19
  characters (`001712345678.123456`), so they sort as times do. Both
  ends are inside the stretch.

A conversation's history reaches back to the source's `since` (2024-01-01
unless the config says otherwise) and up to the newest message the
last walk saw. A `ts` inside a `history:` span is mirrored; one outside
every span is not, and you should ask Slack.

```sql
SELECT 1 FROM coverage
 WHERE scope = 'history:' || :channel
   AND lo <= substr('0000000000000000000' || :ts, -19)
   AND substr('0000000000000000000' || :ts, -19) <= hi;
```

**What is not mirrored**: anything before `since`; direct messages,
unless the source sets `dms = true`; with `dms = true` and no
`dm_conversations` list, a DM closed in the account's Slack sidebar, which
is not walked and has no `history:` span until it is open with a new
message (Slack reopens a DM when a message arrives, and closes it after a
while); channels the account is not in,
unless it sets `all_channels`; files, when it sets `media = false`;
Slack Lists. A message deleted in Slack is dropped from the mirror only
inside the last `refresh_window_days` (30 by default); older deletions
are never noticed. A deleted reply is noticed the next time someone
replies in that thread.

**Set aside from the payload** (in `volatile_payload`): a user's
`updated` and `profile.status_*`, `profile.huddle_*`; a channel's
`updated` and `num_members`; a message's `last_read`, `subscribed` and
the `block_id` of each block; everything in `channel_read_states`.

### `workspaces`

One row per Slack workspace the account signs in to.

| column | what it holds |
|---|---|
| `id` | the team id, `T…` |
| `payload` | Slack's `auth.test` answer |
| `team_name` | the workspace's name |
| `team_url` | its URL (`https://<name>.slack.com/`) |
| `self_user_id` | the account's own user id |

### `users`

One row per person or bot the download has seen.

| column | what it holds |
|---|---|
| `id` | the user id, `U…` |
| `payload` | Slack's user object |
| `team_id` | the workspace it belongs to |
| `name` | the handle |
| `real_name` | the full name |
| `display_name` | the name Slack shows, when they set one |

### `channels`

One row per conversation: public or private channel, DM or group DM.

| column | what it holds |
|---|---|
| `id` | the conversation id |
| `payload` | Slack's conversation object |
| `name` | the channel's name, without `#`; NULL for most DMs |
| `is_member` | 1 if the account is in it |
| `is_archived` | 1 if it is archived |
| `is_dm` | 1 for a DM or a group DM, 0 for a channel |
| `dm_user_ids` | who is in a DM, comma-separated, the account included in a group DM; NULL for a channel |

### `messages`

One row per message, top-level or reply.

| column | what it holds |
|---|---|
| `id` | `<team id>#<channel id>#<ts>` |
| `payload` | Slack's message object |
| `team_id` | the workspace |
| `channel_id` | the conversation |
| `ts` | Slack's `ts`, as text |
| `thread_ts` | the root's `ts` for a reply or a root with replies; NULL for a message no one replied to |
| `thread_root_uuid` | the `id` of the top-level message it belongs under (its own `id` for a top-level message) |
| `is_thread_root` | 1 for every top-level message, 0 for a reply |
| `user_id` | who posted it; NULL for some bot and system messages |

### `threads`

One row per thread whose replies the download has read. How far it
read is `held_version` on `threads_bookkeeping`: the root's
`latest_reply` when it last read the thread whole.

| column | what it holds |
|---|---|
| `id` | the root message's `id` |

### `slack_attachments`

Which files each message carries, and where their bytes are in
`blobs.sqlite`.

| column | what it holds |
|---|---|
| `id` | `<message id>#<file id>` |
| `message_uuid` | the `messages.id` |
| `file_id` | Slack's file id, `F…` |
| `blake3` | the `cas_objects.blake3` holding the bytes; NULL until they are stored, or when `media` is off or the file is over the size limit |

### `channel_read_states`

How far the account has read in each mirrored conversation, from
`client.counts`. Reading a conversation is not a change to it, so the
whole state is in `channel_read_states_bookkeeping.volatile_payload`:
`last_read`, `latest`, `has_unreads`, `mention_count`.

| column | what it holds |
|---|---|
| `id` | the conversation id |
| `payload` | `{"id": …}` only |

### `bookmarks`

The links and pinned items in a conversation's header bar.

| column | what it holds |
|---|---|
| `id` | Slack's bookmark id |
| `payload` | Slack's bookmark object |
| `channel_id` | the conversation it is in |

### `saved_items`

The account's "Saved for later" list: in progress, completed and
archived. A saved message is a pointer to it, not a copy.

| column | what it holds |
|---|---|
| `id` | `<item type>#<item id>#<ts>` |
| `payload` | Slack's saved-item object |
| `item_type` | what was saved (`message`, …) |
| `item_id` | the conversation (or file) it is in |
| `ts` | the saved message's `ts`, when it is a message |

### `coverage`

The stretches of each conversation the download has walked (above).

| column | what it holds |
|---|---|
| `scope` | `history:<channel id>` or `replies:<channel id>` |
| `lo` | the oldest `ts` in the stretch, zero-padded to 19 characters |
| `hi` | the newest `ts` in the stretch, the same way |

## Calendars (`calendar`)

Each way in keeps what its service sends. Google Calendar's events are
Google's JSON, one row per event resource, in `google_events`. CalDAV
servers (Fastmail, iCloud) and `.ics` files keep iCalendar text, one
object per event `UID`, in `ics_objects`. Calendly keeps its JSON, one
row per scheduled event in `calendly_events` and that event's invitees
beside it in `calendly_invitees`.

**A repeating event is one row, not one per date.** Google is asked
with `singleEvents=false`, so a series is one resource with its
`recurrence` rules, and an occurrence that was moved or changed is a
row of its own naming the series in `recurring_event_id`. A cancelled
occurrence is kept, with `status` `cancelled`: it is how the series
says that date is off. A cancelled one-off event or series is deleted
from the mirror, occurrences and all. Turning a series into dates is
left to the reader. An `ics_objects` row holds a series and its changed
occurrences together, as CalDAV stores them.

**Google's `etag` and `updated` are not in `payload`.** Google moves
them on hundreds of events at once with nothing else changed, so they
are in `google_events_bookkeeping.volatile_payload`.

When the source sets `since` or `until`, only events with an
occurrence in that window are mirrored; ask the service for the rest.
A windowed calendar is listed whole every run, and an event the window
has moved past is **dropped** from the mirror, so a relative window
(`since = "-30d"`) keeps only what is inside it now.

### `accounts`

The login this source mirrors. One row.

| column | what it holds |
|---|---|
| `id` | `google` for Google, `calendly` for Calendly, `ics` for files, the server's host for CalDAV |
| `method` | `google`, `calendly`, `caldav` or `ics` |
| `server_url` | the CalDAV server's URL; Calendly's API |
| `principal_href` | the CalDAV principal it found; the Calendly user's URI |
| `login` | whose calendars these are: the CalDAV user, Google's primary calendar id (the account's address), or the Calendly user's address |

### `calendars`

One row per calendar the account lists.

| column | what it holds |
|---|---|
| `id` | Google's calendar id; for CalDAV the last segment of the calendar's URL; for Calendly the user's id (one calendar per user); for a file, its path under the folder without `.ics` |
| `account_id` | the `accounts.id` |
| `href` | the calendar's URL on a CalDAV server |
| `display_name` | its name |
| `description` | its description |
| `color` | its color |
| `time_zone` | the zone a floating time on it is in, where the service says |
| `sync_token` | the token the next incremental listing starts from; NULL when the calendar is listed whole each run |

### `google_events`

One row per Google event resource.

| column | what it holds |
|---|---|
| `id` | `<calendar id>#<event id>` |
| `payload` | Google's event resource, without `etag` and `updated` |
| `calendar_id` | the `calendars.id` |
| `event_id` | Google's event id |
| `recurring_event_id` | for a changed or cancelled occurrence, the series' event id |
| `status` | `confirmed`, `tentative` or `cancelled` |

### `calendly_events`

One row per Calendly scheduled event, cancelled ones included. Every
sync lists them whole, and one the listing no longer names is dropped.

| column | what it holds |
|---|---|
| `id` | `<calendar id>#<event id>`, the event id being the last segment of its `uri` |
| `payload` | Calendly's event resource, with `event_memberships` and `event_guests` sorted |
| `calendar_id` | the `calendars.id` |
| `start_time` | when it starts, as Calendly wrote it (UTC) |
| `status` | `active` or `canceled` |
| `listing_hash` | the blake3 of `payload`: the version its invitees were fetched for |

### `calendly_invitees`

The invitees of one scheduled event, with their answers to the booking
form. Read again only when the event's `listing_hash` moves.

| column | what it holds |
|---|---|
| `id` | the `calendly_events.id` |
| `payload` | `{"invitees": [...]}`, Calendly's invitee resources in booking order |

### `ics_objects`

One iCalendar object per event `UID`.

| column | what it holds |
|---|---|
| `id` | `<calendar id>#<UID>` |
| `payload` | `{"ics": "<the VCALENDAR text>"}` |
| `calendar_id` | the `calendars.id` |
| `uid` | the event's `UID` |
| `href` | where the object lives on the CalDAV server; NULL for a file's event |
| `etag` | the server's version of the object |

### `dav_resources`

What a CalDAV listing named: one row per object, with the version it
was listed at. An object listed here whose `ics_objects` row is missing
or older is still owed.

| column | what it holds |
|---|---|
| `id` | `<calendar id>#<href>` |
| `collection` | the `calendars.id` |
| `href` | the object's URL path |
| `etag` | the version the listing named |

### `dav_unconfirmed`

Objects a listing that stopped part way had not reached yet. Nothing is
deleted for being missing from a listing until a run carries the
listing to its end.

| column | what it holds |
|---|---|
| `collection` | the `calendars.id` |
| `href` | the object's URL path |
