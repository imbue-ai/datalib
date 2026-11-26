# Reading the raw stores

Every source's `ingest` step keeps what the service sent, as close to
the wire as it can: Slack's and Google Calendar's JSON per record,
Gmail's whole original message. That copy is the **raw store**, one
per source, in `<data_root>/<group>/ingest/`, where `<group>` is the
source's id. This guide is for a person or a program building on those
stores: how to open one, how to read it at one moment and follow what
changes, and, source by source, what each table and column holds.

Most questions are better answered from the rendered side first: the
grid, the documents, the search
([`agent_user.md`](../agent_user.md) § "Reading the mirrored data").
Come here when you need a field the render dropped, an upstream id to
call the service with, or the original bytes.

Each source has a section here, named for it and its `type`: what its
tables and columns hold, which upstream id is which, how to tell the
account's own records from everyone else's, and how to check that the
copy is complete. `datalib-step help <type>` prints the same section
after the source's setup and limitations. A test holds every table and
column named here to what the code creates, and every query to the
tables it names.

**Before upgrading a reader, read the release notes.** Each release
lists, under "Changes to the raw stores", every table and column added,
removed or retyped, and every migration that rewrites existing rows,
including one that changes what a value means while its column keeps
its type.

## Opening a store

A source's raw directory holds two files:

| file | what it is |
|---|---|
| `entities.doltlite_db` | every table below, with its history: a doltlite store |
| `blobs.sqlite` | the bytes of attachments and whole emails, by hash: a plain SQLite file |

**Stock `sqlite3` cannot open `entities.doltlite_db`** (`file is not a
database`). Doltlite is SQLite with git-shaped history kept inside the
file. Read it with **`datalib-doltlite`**, which ships beside
`datalib-pipeline` (and in the Docker image) and takes
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

**From Python** (3.9 on, uv's builds included), install the reader at
the version `datalib-pipeline --version` prints. It has wheels for
Apple-silicon Macs and for Linux on x86_64 and aarch64, glibc and musl.

```sh
uv add imbue-datalib-reader==<version>   # or pip install
```

```python
import json
from datalib_reader import Store

with Store("gmail/ingest/entities.doltlite_db") as store:
    rows = store.query("SELECT id, subject FROM emails WHERE subject LIKE ?", "%invoice%")
    payload = json.loads(store.query("SELECT json(payload) FROM emails LIMIT 1")[0][0])
```

A `Store` is read-only, and `Store(path, commit="<hash>")` reads one
commit ([below](#reading-one-commit-and-following-changes)). `query`
returns a list of rows, each a tuple that also answers by column name
(`row["subject"]`). **Select a payload as `json(payload)`**: a bare
`payload` comes back as JSONB `bytes`, which `json.loads` refuses
([below](#payload-is-json-stored-as-jsonb)).
[Reading a store from Python](../dev/doltlite.md#reading-a-store-from-python)
has the rest, including when to `import doltlite` instead (only for code
that needs the standard `sqlite3` module or SQLAlchemy, and never under
the Python builds `uv python install` fetches).

**From anything else**, run `datalib-doltlite -readonly -json` as a subprocess
and parse its output. That costs a process start and a store open per query.

For a tool that speaks only SQLite, one pipe writes a plain SQLite copy
with the same tables (the data, not the history):

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
interval (see [building on Datalib](building_on_datalib.md) § "How
fresh is the mirror?").

## `payload` is JSON, stored as JSONB

Most tables keep the record as the service sent it in a `payload`
column. It is stored as SQLite's binary JSON (JSONB), so selecting it
bare prints bytes, and from Python returns `bytes` that `json.loads`
cannot decode. Ask for `json(payload)` to get the text, and use
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
`main` is the latest:

```sh
datalib-doltlite -readonly slack/ingest/entities.doltlite_db "SELECT dolt_hashof('main')"
```

The store's *version*, the commit the loop hands a step that reads it,
is the branch `datalib_content`: the last commit at which the source's
content or its problems moved. Every sync commits its own record
(`sync_runs`, its stamps) whether or not it found anything, where the
source keeps `sync_runs`, so `main` moves on every run and
`datalib_content` only when there is something new to read.

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

A raw store's history is never folded away: `datalib-pipeline
free-disk-space`, even with `--drop-old-versions-on-derived-data`,
keeps every commit of a download, so a cursor stays readable. A reset of
the source empties the tables in a new commit, which reads as every row
`removed`. If your cursor no longer resolves (the store was deleted and
synced again), start over from a full read.

## What every raw store has

Besides each source's own tables, a store holds these. They are the
download's record of its own work. `problems` and `sync_runs` are worth
reading; the others are listed so nothing in `.tables` is a mystery.

### `<table>_bookkeeping`

One sidecar per record table, keyed like it (`emails_bookkeeping` for
`emails`): what fetching that record cost and what is set aside from
its payload. The tables made from an export's files, as Facebook's and
LinkedIn's are, have none.

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
| `source_id` | the source's id, its group |
| `stage` | which stage found it: `fetch` in a raw store |
| `severity` | `error`, `warning` or `info` |
| `outcome` | what became of the record: `dropped` (not in the mirror), `nulled` (in it, missing something), `ok` |
| `reason` | why: `fetch_failed`, `over_size_limit`, `not_found`, `forbidden`, … |
| `scope_kind` | `entity` in a raw store |
| `scope_key` | what it is about: mostly `<table>:<id>` for a record (`emails:018f…`), `listing:…` for a listing, `phase:…` for a part of the run; some sources write other forms (`record:<table>:<id>`, `config:<setting>:<value>`, `file:…`), and each source's section says which of its rows mean something is missing |
| `item_uuid` | empty here; the app's problems list fills it from what render recorded of the record's grid row |
| `field` | the field it is about, when it is one field |
| `path` | where in the payload, as a JSON pointer, when known |
| `rule` | the rule that fired, when the loss was deliberate |
| `sample` | what was said: the error, or the start of the offending value |
| `first_seen_at_utc` | when it was first recorded, in UTC |
| `changed_at_utc` | when it last changed, in UTC |
| `tz_offset` | the offset for the two stamps |
| `render_version` | empty here; set on rows a render records |
| `upstream_entity_kind` | the kind of upstream record the problem is about, as a grid row names it, where the download said; empty otherwise |
| `upstream_id` | that record's upstream id, likewise |
| `upstream_account` | the upstream account it is in, likewise |

### `sync_runs`

One row per download run, for the sources that reach a service over the
network (not Garmin, YoLink or contacts), and for a few that read local
files. The start of the newest `ok` row is how recent the source
is as a whole: [building on Datalib](building_on_datalib.md) § "How
fresh is the mirror?".

| column | what it holds |
|---|---|
| `run_id` | a number counting up |
| `started_at_utc` | when it started, in UTC |
| `finished_at_utc` | when it ended; NULL while it runs, or if it was killed |
| `tz_offset` | the offset for the two stamps |
| `config` | the step's params as that run saw them, JSON |
| `status` | `running`, then how it ended: `ok` if it read everything it set out to; `partial` if it ended early, told to stop, giving up on a service that stopped answering, or over a budget such as `message_budget`; `error` if it failed; `interrupted` if it was killed before it could say, set when the next run starts. An old row may say `ok` for a run that ended early, written by a build that had no `partial` |
| `summary` | what it did, JSON (counts, and why it stopped early if it did). `cursors` lists each sync position the run moved, before and after; where more than five of one kind moved (a first Slack run checks every closed DM), they are one entry in `cursors_by_kind` instead: the kind, how many moved, how many are new, and three examples |

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

## Gmail, Fastmail, Apple Mail and mbox (`email`)

Every way into mail writes one schema. Fastmail and other JMAP
servers send JMAP's own objects; the Gmail API, Apple Mail and mbox
files are made to look like them, so `payload` is a JMAP-shaped **envelope**: who,
when, subject, labels. **There is no body in the envelope.** The message
itself, headers, body and attachments, is the original RFC 5322 `.eml`,
stored whole in `blobs.sqlite`. To show a line of a message without
opening that, read `emails.preview`.

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
Fastmail, for Apple Mail, for an mbox, and for a message a Takeout seeded until the
next sync asks Gmail for its labels.

**From the mirror's spelling to the API's, and back.** The Gmail API and
Gmail's URLs write an id without leading zeros; the mirror pads it to
16 digits. In SQL:

```sql
-- the API's spelling of each message's id and thread id
SELECT id, ltrim(id, '0') AS api_id, ltrim(thread_id, '0') AS api_thread_id
  FROM emails;
-- a message by the id the API gave, bound to the ?
SELECT * FROM emails WHERE id = substr('0000000000000000' || lower(?), -16);
```

To open a message in Gmail, link to
`https://mail.google.com/mail/u/0/#search/rfc822msgid:` followed by its
`message_id`, percent-encoded: the link the app's own ↗ uses. `u/0` is
the first Google account the browser is signed in to.

**Getting the message.** Join `email_blobs` on the email, then
`cas_objects` in `blobs.sqlite` on its `blake3`. The row's
`content_type` is `message/rfc822` and `bytes` is the message as the
server sent it. Its headers are raw: folded across lines and, for
anything not ASCII, RFC 2047-encoded (`=?UTF-8?B?…?=`). Hand the bytes
to a mail parser rather than reading headers by hand. For Gmail and
JMAP, an `.eml` larger than the source's `blob_size_limit_bytes` is not
stored; its `email_blobs.blake3` stays NULL and a warning in `problems`
says so. The mbox and Apple Mail methods store every message whole. A
message whose `.eml` is not stored renders with no sender and no body,
because render reads both from the `.eml`.

**Who "me" is.** The account's own address is `accounts.email_address`.
It is set for Gmail, and for JMAP when the sign-in name is an address;
for mbox and Apple Mail, only when the config sets it. The messages you
sent:

```sql
SELECT e.id, e.subject, e.received_at
  FROM emails e JOIN accounts a ON a.id = e.account_id
 WHERE a.email_address IS NOT NULL
   AND EXISTS (SELECT 1 FROM json_each(e.from_json) f
                WHERE lower(json_extract(f.value, '$.email')) = lower(a.email_address))
 ORDER BY datetime(e.received_at) DESC;
```

**Which id is which, for mbox and Apple Mail.** A message's `emails.id`
from an mbox that is not Gmail's is its `Message-ID` without angle
brackets, or the blake3 of the `.eml` when it has none; a second message
with the same id in one read is dropped. From Apple Mail it is
`<account id>:<Message-ID>`, or `<account id>:<blake3 of the .eml>`, and
its `thread_id` is the `emails.id` of the earliest message in Mail's
conversation.

**Labels are split two ways**, the way JMAP models them. A label that
files mail (Inbox, Sent, a label you made, the inbox categories) is a
row in `mailboxes`, and an email's labels are its `email_mailboxes`
rows (also `payload -> '$.mailboxIds'`). A label that is a flag is a
keyword in `email_keywords` (also `payload -> '$.keywords'`). Whether
a message was read, and whether its thread is muted, is its row in
`read_marks`:

| Gmail | in the mirror |
|---|---|
| `UNREAD` | `read_marks.read` = 0 (and 1 without it) |
| `STARRED` | keyword `$flagged` |
| `IMPORTANT` | keyword `$important` |
| `INBOX`, `SENT`, `DRAFT`, `TRASH`, `SPAM` | a mailbox with `role` `inbox`, `sent`, `drafts`, `trash`, `junk` |
| `CATEGORY_*`, your own labels | a mailbox with no role |
| muted (no label: the search `is:muted`) | `read_marks.muted` = 1 (and 0 without it) |

For Gmail, `emails.label_ids` puts them back together: every label id
the message carries, as the API spells them, `STARRED` and `IMPORTANT`
included, as a sorted JSON array (`["INBOX","STARRED"]`). `UNREAD` is
not among them: it is in `read_marks`.
`EXISTS (SELECT 1 FROM json_each(label_ids) WHERE value = 'INBOX')`
asks whether a message is in the inbox.

**Traps.**

- **`received_at` is not Gmail's receipt time.** For Gmail, Apple Mail and mbox it
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
- **`emails.preview` is for showing, not for searching or quoting.**
  Gmail's snippet and a JMAP server's preview are cut where the service
  chose and may skip part of the message, and it is NULL for a message
  whose `.eml` is not held yet. For the text itself, read the `.eml`.
- **A muted thread is any of its messages being muted.** Muting belongs
  to a thread, and the mark is on each email: ask
  `EXISTS (SELECT 1 FROM read_marks r JOIN emails e ON e.id = r.id
  WHERE e.thread_id = ? AND r.muted = 1)`. On Gmail a mute shows after
  the next sync, which asks every time.

### Is the copy complete?

Three checks, in this order.

1. **What the last sync could not do.** Every listing, phase or message
   that failed is a row in `problems` until a later sync does it.

   ```sql
   SELECT severity, reason, scope_key, sample
     FROM problems
    WHERE severity IN ('error', 'warning')
    ORDER BY scope_key;
   ```

   The `scope_key` says what is missing:

   | `scope_key` starts with | means |
   |---|---|
   | `listing:` | a listing failed (`Mailbox/get`, `Email/changes`, `Email/query`, `messages.list <label>`, `messages.list threads`, `messages.list muted`, `mbox <file>`, `files`). Mail may exist upstream that the copy has not heard of. `listing:removed_records` (mbox) counts deletions held back |
   | `phase:` | a whole phase stopped early (`Email/get`, `eml_download`, `messages.get`, `apple_mail_files`). The sample says how many were fetched and how many are left |
   | `listed_messages:` | one message that would not fetch (JMAP, Gmail) |
   | `email_blobs:` | one `.eml` that would not download (JMAP), or one over the size limit (`reason = 'over_size_limit'`) |
   | `emails:` | one message stored with something missing (Apple Mail: attachment parts Mail never downloaded) |
   | `skipped:` | messages that would not parse (`skipped:mbox:…`, `skipped:apple_mail`), or Takeout messages with no Gmail id (`skipped:takeout`) |
   | `config:` | a label in `only_extract_labels` that the account does not have |

2. **What is still owed (Gmail and JMAP only).** A message upstream has
   named is owed until its row holds the version it was named at:

   ```sql
   SELECT count(*) AS messages_owed
     FROM listed_messages l
     LEFT JOIN listed_messages_bookkeeping b ON b.id = l.id
    WHERE b.fetched_at_utc IS NULL OR b.held_version IS NOT l.stamp;
   ```

   And a message whose full `.eml` is not stored yet (this also counts
   those kept out by `blob_size_limit_bytes`, which have an
   `over_size_limit` problem):

   ```sql
   SELECT count(*) AS emls_missing
     FROM emails e
    WHERE NOT EXISTS (SELECT 1 FROM email_blobs b
                       WHERE b.blob_id = e.blob_id AND b.blake3 IS NOT NULL);
   ```

   Whether the listing itself has finished: with no label filter,
   `listed_whole` has a `*` row once the whole account has been listed;
   with one, a row per label or mailbox id, plus `#threads` once the
   conversation index is whole. A missing row means messages exist that
   are not even listed yet:

   ```sql
   SELECT scope FROM listed_whole;
   ```

3. **For mbox and Apple Mail**, which keep no listing, read the newest
   run instead: `ok` means it read everything it set out to.

   ```sql
   SELECT status, started_at_utc, finished_at_utc, summary
     FROM sync_runs ORDER BY run_id DESC LIMIT 1;
   ```

   For an mbox, `ingested_files` holds an `email/mbox_read` row set
   only after a clean read of every file. For Apple Mail, a
   `phase:apple_mail_files` problem counts the messages Mail has not
   downloaded.

### `accounts`

One row per mail account the source mirrors.

| column | what it holds |
|---|---|
| `id` | JMAP's account id; for Gmail the configured `account_id`, by default the address Gmail reports; for mbox the configured `account_id`, by default the last part of `mbox.path` without its extension, lowercase with underscores (`takeout` for one `.zip` or `.tgz`); for Apple Mail `apple_mail_` and the start of Mail's account UUID, unless `apple_mail.account_id` is set |
| `payload` | the account object (JMAP's; for Gmail `{id, name, email, isPersonal, _source}`, where `_source` names the API it came from) |
| `name` | its display name |
| `email_address` | the address it signs in as: Gmail's, a JMAP server's sign-in name on the personal account when that is an address, the one mbox's or Apple Mail's config sets; NULL when none is known |
| `is_personal` | 1 for the person's own account, 0 for a shared one |
| `is_read_only` | 1 if the server says it is read-only |

### `mailboxes`

One row per mailbox or label.

| column | what it holds |
|---|---|
| `id` | JMAP's mailbox id; for Gmail `gmail:<account id>:<label id>`; for a Takeout label known only by name, and for every Apple Mail mailbox, `mbox-` and a hash of the name |
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
| `payload` | the envelope: `id`, `blobId`, `threadId`, `mailboxIds`, `keywords`, `size`, `hasAttachment`, `receivedAt`, `sentAt`, `subject`, `from`, `to`, `cc`, `messageId`, `inReplyTo`, `references`; for a message the Gmail API answered for, also `_source: {"via": "gmail.googleapis.com"}`. JMAP's has no `to`, `cc`, `inReplyTo` or `references`, which the download does not ask for, and has `attachments`, JMAP's list of the message's parts |
| `account_id` | the account it belongs to |
| `thread_id` | its thread: Gmail's thread id in the same padded hex as `id`; JMAP's own thread id; for an mbox message with no `X-GM-THRID`, its own `id` |
| `blob_id` | the id of its `.eml`: blake3 of the bytes for Gmail, mbox and Apple Mail, JMAP's own blob id for JMAP |
| `message_id` | its `Message-ID:`, without angle brackets |
| `in_reply_to` | its `In-Reply-To:`, without angle brackets; NULL for JMAP (read the `.eml`) |
| `references_header` | its `References:`, space-separated, without angle brackets; NULL for JMAP |
| `received_at` | see "Traps": an RFC 3339 time with the sender's offset |
| `sent_at` | its `Date:` header |
| `size` | the `.eml`'s size in bytes |
| `subject` | the subject, decoded |
| `from_json` | the senders, as JSON text: `[{"name": …, "email": …}]` |
| `to_json` | the `To:` addresses, the same way; NULL for JMAP (read the `.eml`) |
| `cc_json` | the `Cc:` addresses, the same way (there is no Bcc column); NULL for JMAP |
| `has_attachment` | 1 if the message has an attachment |
| `label_ids` | Gmail only: its label ids as a sorted JSON array (see "Labels") |
| `preview` | a line of the body as plain text, at most about 256 characters: Gmail's snippet with its HTML entities decoded, a JMAP server's `preview`, or, where those give none, made from the `.eml`. Empty for a message with no text, NULL until the `.eml` is held or if it will not parse. Not in `payload` |

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
| `keyword` | `$flagged`, `$important`, or another keyword the server or the mbox file names; never `$seen` or `$muted`, which are in `read_marks` |

### `read_marks`

Whether each email was read and whether its thread is muted, one row per
email, rewritten with it and gone with it. JMAP's `$seen`, Gmail's
`UNREAD` and an mbox's `Unread` label land here and nowhere else, and so
do JMAP's `$muted`, an mbox's `Muted` label and Gmail's `is:muted` search.

| column | what it holds |
|---|---|
| `id` | the `emails.id` |
| `read` | 1 if it was read, 0 if not |
| `read_through` | NULL for email (another source's mark on a whole conversation) |
| `muted` | 1 if it is in a muted thread, 0 if not; NULL where nothing has said yet (Gmail, when the search failed) |

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
owed: [building on Datalib](building_on_datalib.md) § "How fresh is the
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
user, bot, channel or message JSON from `users.list`, `bots.info`,
`conversations.list`, `conversations.history` and
`conversations.replies`.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a workspace (team) id `T…` | `workspaces.id`, and `team_id` on users and messages |
| a user id `U…` | `users.id`, `messages.user_id` |
| a bot id `B…` | `bots.id`, and `bot_id` in a message's payload |
| a conversation id `C…`, `D…`, `G…` | `channels.id`, `messages.channel_id` |
| a message's `ts` in a conversation | `messages.ts`; the row's `id` is `<team id>#<channel id>#<ts>` |
| a thread (its root's `ts`) | `messages.thread_root_uuid`, equal to the root's `messages.id` |
| a file id `F…` | `slack_attachments.file_id` |

**A message's permalink**, as Slack's `chat.getPermalink` builds it: on
the workspace's own host, and for a reply, with the thread it is in.

```sql
SELECT m.id,
       rtrim(w.team_url, '/') || '/archives/' || m.channel_id
         || '/p' || replace(m.ts, '.', '')
         || CASE WHEN m.thread_ts IS NOT NULL AND m.thread_ts != m.ts
                 THEN '?thread_ts=' || m.thread_ts || '&cid=' || m.channel_id
                 ELSE '' END AS permalink
  FROM messages m JOIN workspaces w ON w.id = m.team_id;
```

`workspaces.self_user_id` is the account the mirror signs in as, which
is how to tell "me" from everyone else. A direct message is a
`channels` row with `is_dm = 1`; `dm_user_ids` lists who is in it.
`channels` holds every channel Slack listed, archived ones
(`is_archived = 1`) too, whether or not their messages are mirrored.

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

- Scope `history:<channel id>` covers top-level messages, and the
  replies also sent to the channel, from `conversations.history`.
- Scope `replies:<channel id>` covers the stretches whose new replies
  the download has searched for (a reply to an old root is found this
  way).
- Scope `involving:<search>` (with `scope = "involves_me"`) covers the
  stretches of time one search for messages that involve the account
  has searched, across the whole workspace.
- `lo` and `hi` are `ts` values left-padded with zeros to 19
  characters (`001712345678.123456`), so they sort as times do. Both
  ends are inside the stretch.

A conversation's history reaches back to the source's `since` (to its
first message when the config sets none) and up to the newest top-level
message the last walk saw. With `scope = "involves_me"`, a channel's reaches back
only to `context_before` ahead of the oldest message that involves the
account, and a channel where nothing does is not walked at all. A conversation walked and found empty has one row with
`lo = hi`, the point the walk began.

**`hi` is not a time, and a stored message can lie past it.** It stays
put through any number of walks that find nothing new, and a reply newer
than the channel's last top-level message is stored above it. When the
download last looked is in `coverage_checked`: at `checked_at_utc`, Slack
had no top-level message in that conversation newer than its spans,
because a walk reached the top or `client.counts` showed nothing newer
(`client.counts` skips a join or leave notice, which the next walk picks
up). The time is this machine's clock as the sync began, so it errs early. A
conversation that fell behind keeps its older time: a DM closed in the
sidebar between its daily checks, a walk that failed, a sync stopped
before reaching it. With `context_after` set, a stretch has a top of its
own and says nothing about the present, so there is usually no row.

So, for what `conversations.history` would list: a `ts` inside a
`history:` span is mirrored. One past every span and no later than
`checked_at_utc` is mirrored too, there being no top-level message there.
Anything else, ask Slack.

```sql
WITH q(scope, ts, key) AS (
  SELECT 'history:' || :channel, :ts, substr('0000000000000000000' || :ts, -19))
SELECT EXISTS (SELECT 1 FROM coverage c, q
                WHERE c.scope = q.scope AND c.lo <= q.key AND q.key <= c.hi)
    OR EXISTS (SELECT 1 FROM coverage_checked k, q
                WHERE k.scope = q.scope
                  AND CAST(q.ts AS REAL) <= unixepoch(k.checked_at_utc)
                  AND q.key > COALESCE(
                        (SELECT max(hi) FROM coverage WHERE scope = q.scope), ''));
```

This does not say a thread's replies are held; that is the root's
`threads_bookkeeping.held_version` against its `latest_reply` (above).

**What is not mirrored**: anything before `since`; direct messages,
unless the source sets `dms = true`; for up to a day, the newest
messages of a DM closed in the account's Slack sidebar (Slack reopens a
DM when a message arrives and the next sync reads it, but one closed
again before that sync is only read by the daily check of closed DMs);
channels the account is not in,
unless it sets `all_channels`; archived channels, unless it sets
`archived = true` or names them in `channels`; with `scope = "involves_me"`, channels
where nothing involves the account, and the part of each channel
before `context_before` (and after `context_after`, when set); files, when it sets `media = false`;
Slack Lists.

**Edits and deletions are seen late, or never.** A top-level message's
edit, reaction or deletion is seen only while the message was posted
inside the last `refresh_window_days` (30 by default), the next time
that window is re-read (at most once every `refresh_every_minutes`, 60
by default), or when its thread is read again. The window is not
re-read in a DM closed in the sidebar, which only the daily check of
closed DMs reads, nor in an archived channel already read. A reply's
edit, reaction or deletion is seen only when its thread is read again,
which happens only when the root's `latest_reply` changes: when someone
replies, or when the newest reply is deleted and the root is inside the
window. A reply also sent to the channel is the exception: the
channel's history lists it, so it is seen like a top-level message.

**Set aside from the payload** (in `volatile_payload`): a user's
`updated` and `profile.status_*`, `profile.huddle_*`; a bot's
`updated`; a channel's
`updated` and `num_members`; a message's `subscribed` and the
`block_id` of each block; everything in `channel_read_states` but
`last_read`, which is in `read_marks`.

A problem with a channel's history is keyed by the conversation's
name, not its id: `listing:conversations.history <name>`.

### Is the copy complete?

Run each against `<data_root>/<id>/ingest/entities.doltlite_db` with
`datalib-doltlite -readonly`.

1. **The last sync's outcome.** `ok` means it read everything it set
   out to; `partial` means it stopped early.

   ```sql
   SELECT run_id, started_at_utc, status, summary FROM sync_runs
    ORDER BY run_id DESC LIMIT 1;
   ```

2. **What failed.** Prefixes that mean part of the copy is missing:
   `listing:conversations.list` (the channel list is stale),
   `listing:conversations.history <name>` (that conversation's newest
   history was not read), `listing:search.messages` (new replies to old
   threads may be missing), `threads:<team>#<channel>#<ts>` (that
   thread's replies were not read), `slack_attachments:<id>` with
   reason `fetch_failed` (a file's bytes are missing) or
   `over_size_limit` (skipped on purpose), `users:<id>` and `bots:<id>`
   (a name is missing), `silent:channels` (every channel read came back
   empty: check `since` and `channels`), `config:channels:<name>` and
   `config:dm_conversations:<id>` (a name in the config matches nothing).
   An `info` row for `listing:client.counts` or `listing:saved.list`
   means the token may not make that call; it is asked again a week
   later.

   ```sql
   SELECT severity, reason, scope_key, sample FROM problems
    WHERE severity IN ('error', 'warning')
    ORDER BY severity, scope_key;
   ```

3. **Threads owed.** A thread is held when its sidecar has a landed
   fetch at the root's current `latest_reply`. The query agents use is
   right, as the download computes it; grouped by conversation it also
   names where they are. It counts threads in every stored conversation,
   including ones no longer mirrored (a channel taken out of `channels`),
   which the download no longer reads.

   ```sql
   SELECT m.channel_id, c.name, count(*) AS threads_owed
     FROM messages m
     LEFT JOIN threads_bookkeeping b ON b.id = m.id
     LEFT JOIN channels c ON c.id = m.channel_id
    WHERE m.is_thread_root = 1
      AND json_extract(m.payload, '$.reply_count') > 0
      AND (b.fetched_at_utc IS NULL
           OR b.held_version IS NOT json_extract(m.payload, '$.latest_reply'))
    GROUP BY m.channel_id;
   ```

   It cannot see a reply Slack has that the stored root does not know
   of yet; those are found by the reply search, so also check for a
   `listing:search.messages` problem.

4. **Replies missing from a thread**, as the document's "not mirrored
   yet" line counts them:

   ```sql
   SELECT m.id,
          json_extract(m.payload, '$.reply_count') AS listed,
          (SELECT count(*) FROM messages r
            WHERE r.thread_root_uuid = m.id AND r.is_thread_root = 0) AS held
     FROM messages m
    WHERE m.is_thread_root = 1
      AND json_extract(m.payload, '$.reply_count') >
          (SELECT count(*) FROM messages r
            WHERE r.thread_root_uuid = m.id AND r.is_thread_root = 0);
   ```

5. **Files without bytes, and authors not yet named.** With `media`
   on, a `slack_attachments` row with no `blake3` is owed, unless a
   `problems` row says it is over the size limit.

   ```sql
   SELECT (SELECT count(*) FROM slack_attachments WHERE blake3 IS NULL) AS files_owed,
          (SELECT count(*) FROM users WHERE payload IS NULL) AS users_unnamed,
          (SELECT count(*) FROM bots WHERE payload IS NULL) AS bots_unnamed;
   ```

6. **Conversations never read.** A mirrored conversation with no
   `history:` coverage has not been walked yet. For the default scope
   (joined, open channels):

   ```sql
   SELECT c.id, c.name FROM channels c
    WHERE c.is_dm = 0 AND c.is_member = 1 AND c.is_archived = 0
      AND NOT EXISTS (SELECT 1 FROM coverage v WHERE v.scope = 'history:' || c.id);
   ```

   Whether a given message time is covered is the `coverage` /
   `coverage_checked` query in the existing raw-store section.

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

One row per person or bot the download has seen. A row with a NULL
`payload` is an author a stored message named before any listing did;
the next sync asks `users.info` for it.

| column | what it holds |
|---|---|
| `id` | the user id, `U…` |
| `payload` | Slack's user object |
| `team_id` | the workspace it belongs to |
| `name` | the handle |
| `real_name` | the full name |
| `display_name` | the name Slack shows, when they set one |

### `bots`

One row per bot a stored message names: an app or an integration,
which posts with a `bot_id` and often no user, so its messages do not
carry its name. A row with a NULL `payload` has not been looked up yet;
the next sync asks `bots.info` for it.

| column | what it holds |
|---|---|
| `id` | the bot id, `B…` |
| `payload` | Slack's bot object, from `bots.info` |
| `name` | the bot's name |

A message's author, then, is `users` by `messages.user_id` for a
person, and for a bot the payload's `username` (an integration's name
for that message), else its `bot_profile.name`, else `bots.name` by its
`bot_id`.

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

The account's state in each mirrored conversation, from
`client.counts`. How far it has read is in `read_marks`; the rest is in
`channel_read_states_bookkeeping.volatile_payload`: `latest`,
`updated`, `history_invalid`, `has_unreads`, `mention_count`.

| column | what it holds |
|---|---|
| `id` | the conversation id |
| `payload` | `{"id": …}` only |

### `read_marks`

How far the account has read: each mirrored conversation, and each
thread it follows.

| column | what it holds |
|---|---|
| `id` | the conversation id, or a thread's `{team}#{channel}#{ts}` |
| `read_through` | the `ts` of the last message read |
| `read` | NULL; Slack marks nothing message by message |
| `muted` | NULL; the download does not ask |

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

### `involving`

With `scope = "involves_me"`: each message a search listed as
involving the account, one row per search that found it. A row is a
pointer to the message, not a copy; the message itself is in
`messages` once its channel is walked.

| column | what it holds |
|---|---|
| `id` | `<search>#<team id>#<channel id>#<ts>` |
| `payload` | `ts`, `channel.id` and `permalink` from the search match |
| `query` | which search found it: `mention`, `from_me`, `here` or `channel` |
| `channel_id` | the conversation it is in |
| `ts` | the message's `ts` |
| `thread_ts` | the root's `ts` when it is in a thread (its own for a root); NULL otherwise |

### `coverage`

The stretches of each conversation the download has walked (above).

| column | what it holds |
|---|---|
| `scope` | `history:<channel id>`, `replies:<channel id>` or `involving:<search>` |
| `lo` | the oldest `ts` in the stretch, zero-padded to 19 characters |
| `hi` | the newest `ts` in the stretch, the same way |

### `coverage_checked`

When each conversation's history was last known to hold everything
Slack listed (above). No row means it never has been.

| column | what it holds |
|---|---|
| `scope` | `history:<channel id>` |
| `checked_at_utc` | when, in UTC |
| `tz_offset` | the offset for that stamp |

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
from the mirror, occurrences and all. An `ics_objects` row holds a
series and its changed occurrences together, as CalDAV stores them.
For one row per date, read the `render_occurrences` step's store
instead (§ "A calendar's occurrences" below).

**Google's `etag` and `updated` are not in `payload`.** Google moves
them on hundreds of events at once with nothing else changed, so they
are in `google_events_bookkeeping.volatile_payload` (a row an older
build wrote may still carry them in `payload`).

When the source sets `since` or `until`, only events with an
occurrence in that window are mirrored; ask the service for the rest.
The window is whole UTC days. Calendly keeps a meeting only when it
starts in the window; `.ics` files take no window.
A windowed calendar is listed whole every run, and an event the window
has moved past is **dropped** from the mirror, so a relative window
(`since = "-30d"`) keeps only what is inside it now.

### Is the copy complete?

Open the raw store with
`datalib-doltlite -readonly <data_root>/<group id>/ingest/entities.doltlite_db "<sql>"`.

1. **Problems.** Every row means something is missing or stale:

   ```sql
   SELECT severity, scope_key, reason, sample FROM problems ORDER BY scope_key
   ```

   What the `scope_key` prefixes mean for a calendar:

   - `listing:calendar <name>`: that calendar's listing failed, or
     stopped part way, so it may lack recent changes and nothing it did
     not reach was deleted.
   - `listing:calendars` (Google): the calendar list held a calendar
     with no id, so no calendar was deleted this run.
   - `listing:meetings` (Calendly): the meeting listing failed part
     way; nothing was dropped.
   - `listing:ics <file>`, `listing:files`, `record:files:<file>`
     (`.ics`): a file could not be read or walked; its calendar is as
     the last good read left it.
   - `file:calendar/ics:<file>` (`.ics`): events with no `UID` were
     left out of that file.
   - `phase:multiget <calendar>` (CalDAV), `phase:invitees` (Calendly):
     the fetching gave up for this run; the rest is still owed.
   - `dav_resources:<calendar id>#<href>` (CalDAV): one object that
     would not come (`error`), or came with no event `UID` (`warning`,
     `no_identity`; it will not be asked for again until it changes).
   - `calendly_invitees:<event id>` (Calendly): that meeting's invitees
     would not come.
   - `config:calendars:<name>`: a calendar named in `calendars` that
     the account does not have.

   The occurrences store
   (`<group id>/render_occurrences/occurrences.doltlite_db`) has a
   `problems` table of its own, one row per event in `unlisted`.

2. **What is still owed.**

   CalDAV and Fastmail: objects a listing named and the store does not
   hold at that version.

   ```sql
   SELECT r.collection, r.href, r.etag, b.held_version, b.last_error
     FROM dav_resources r
     LEFT JOIN dav_resources_bookkeeping b ON b.id = r.id
    WHERE b.fetched_at_utc IS NULL OR b.held_version IS NOT r.etag
   ```

   A whole listing that has not reached its end yet, so deletions are
   still pending:

   ```sql
   SELECT collection, count(*) AS not_reached FROM dav_unconfirmed GROUP BY collection
   ```

   Calendly: meetings whose invitees are not held for the meeting as
   last listed.

   ```sql
   SELECT e.id, e.start_time, b.last_error
     FROM calendly_events e
     LEFT JOIN calendly_invitees_bookkeeping b ON b.id = e.id
    WHERE e.listing_hash IS NOT NULL
      AND (b.fetched_at_utc IS NULL OR b.held_version IS NOT e.listing_hash)
   ```

   Google and `.ics` keep no owed records: a Google listing carries
   whole events, and a file is read whole. For Google, a calendar with
   no window whose `sync_token` is NULL after a run has never finished
   a whole listing:

   ```sql
   SELECT id, display_name FROM calendars WHERE account_id = 'google' AND sync_token IS NULL
   ```

   (This also lists calendars left out by `calendars`, and every
   calendar when the source sets `since` or `until`.)

3. **The newest run.** For every method:

   ```sql
   SELECT run_id, started_at_utc, finished_at_utc, status, summary
     FROM sync_runs ORDER BY run_id DESC LIMIT 1
   ```

   `ok` means the run read everything it set out to; `partial`,
   `error` or `interrupted` means it did not.

### `accounts`

The login this source mirrors. One row.

| column | what it holds |
|---|---|
| `id` | `google` for Google, `calendly` for Calendly, `ics` for files, the server's host for CalDAV |
| `method` | `google`, `calendly`, `caldav` or `ics` |
| `server_url` | the CalDAV server's URL; Calendly's API; for Google `https://www.googleapis.com/calendar/v3`; for `.ics` `file://` and the configured path |
| `principal_href` | the CalDAV principal it found; the Calendly user's URI |
| `login` | whose calendars these are: for CalDAV the first `mailto:` address the principal lists, else the last part of its URL; Google's primary calendar id (the account's address); the Calendly user's address; NULL for `.ics` |

### `calendars`

One row per calendar the account lists.

| column | what it holds |
|---|---|
| `id` | Google's calendar id; for CalDAV the last segment of the calendar's URL; for Calendly the user's id (one calendar per user); for a file, its path under the folder without `.ics` |
| `account_id` | the `accounts.id` |
| `href` | the calendar's URL on a CalDAV server; the user's URI for Calendly; the file's path under the folder for `.ics`; NULL for Google |
| `display_name` | its name: for Google your own name for it when you gave one, else its `summary`; for a file its `X-WR-CALNAME`, else the file's name; always `Calendly` for Calendly |
| `description` | its description; for Calendly the user's scheduling page URL |
| `color` | its color |
| `time_zone` | the zone a floating time on it is in, where the service says: a file's `X-WR-TIMEZONE`, the Calendly user's `timezone` |
| `sync_token` | the token the next incremental listing starts from; NULL when the calendar is listed whole each run, for every `.ics` and Calendly calendar, and for a Google calendar whose first whole listing has not finished |

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
form. Read again when the event's `listing_hash` moves, or when the
last try failed: the version held is
`calendly_invitees_bookkeeping.held_version`. `payload` is NULL for a
meeting whose invitees have never been fetched.

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
was listed at. An object is owed when `dav_resources_bookkeeping` has
no landed fetch of it, or holds it at a different `held_version` than
this row's `etag`. An object with no event `UID` has no `ics_objects`
row and is not owed: it is held at its etag, with a `no_identity`
warning in `problems`.

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

## A calendar's occurrences

The raw store keeps a repeating event as one series. A `calendar`
source's `render_occurrences` step turns that into what Google's
`events.list?singleEvents=true` answers: one row per occurrence, with
moved, changed and cancelled dates applied. It lists every occurrence
that overlaps a window of whole days around its last run (30 back and
400 ahead by default, set by the step's `days_back` and `days_ahead`),
and replaces what it held each time it runs. It runs when `ingest`
stores something new, so the window of a calendar nobody has touched
for a week is a week old: read `occurrences_window` before trusting a
missing row, and ask the service for dates outside it.

It is a store of its own, `<group>/render_occurrences/occurrences.doltlite_db`,
opened like a raw store but not one: nothing is kept in it that the
raw store does not hold.

**Two clocks.** `payload`, for Google, is written the way Google's API
writes it: every `dateTime` in `start`, `end` and `originalStartTime` is
in the **calendar's** zone (`calendars.time_zone`), with the zone the
event was made in named beside it in `timeZone`. A London meeting on a
Vienna calendar is `{"dateTime": "2026-10-28T15:00:00+01:00",
"timeZone": "Europe/London"}`. The `start_time`, `end_time` and
`original_start_time` columns are in the **event's** own zone, the same
meeting's `2026-10-28T14:00:00+00:00`. Both name the same instant; sort
and compare on `start_utc` and `end_utc`.

### `occurrences`

One row per occurrence.

| column | what it holds |
|---|---|
| `id` | `<calendar_id>#<instance_id>`, the key |
| `calendar_id` | the calendar, as the raw store's `calendars.id` |
| `instance_id` | Calendly: the meeting's Calendly uuid. Google: Google's own id — `<series id>_20261022T160000Z`, or `<series id>_20261224` for an all-day one, the original start in UTC; a one-off event's own id. CalDAV and `.ics`: the `UID`, and for an occurrence of a series `<UID>_<its original start>` spelled the same way |
| `series_id` | the series it belongs to (Google's `recurringEventId`, the iCalendar `UID`); null for a one-off event |
| `original_start_time` | where the rule put it, before any move, in the event's zone; null for a one-off |
| `start_time`, `end_time` | in the event's own zone: RFC 3339 with the offset that zone has that day (`2026-11-02T10:00:00-08:00`), or `YYYY-MM-DD` for an all-day event, whose end is the day after its last, as Google writes it |
| `start_utc`, `end_utc` | the same instants in UTC, to sort and to compare a window against. An all-day event begins at midnight in the calendar's zone |
| `all_day` | 1 for an all-day event |
| `time_zone` | the IANA zone the event is written in, else the calendar's |
| `status` | `confirmed` or `tentative`; `confirmed` when the event does not say. Cancelled occurrences are not listed |
| `title` | its summary; a changed occurrence with none takes its series' |
| `event_row` | the raw store's row it was read from (`google_events.id`, `ics_objects.id`) |
| `payload` | Google: the occurrence as `singleEvents=true` returns it, times in the calendar's zone — the series' resource under the occurrence's id and times, without `recurrence`, `updated` and `etag` included; a changed occurrence's own resource. CalDAV and `.ics`: `{"ics": "<the VEVENT>"}`, the series' own for a date its rule gives. Calendly: the scheduled event, with its `invitees` |

### `occurrences_window`

The days the rows cover. One row.

| column | what it holds |
|---|---|
| `id` | always 1 |
| `from_utc` | midnight UTC `days_back` days before the run's day |
| `until_utc` | the end of the day `days_ahead` days after it |

### `unlisted`

An event whose occurrences could not be worked out, and why: a `TZID`
the zone database does not know, a rule part this build does not follow
(`BYWEEKNO`, a sub-daily `FREQ`), no start. Nothing is guessed. Each
event here is also a warning in the store's `problems`, with the
columns a raw store's has (§ "What every raw store has"), `stage`
`occurrences` and the raw row as `scope_key`; that is the row the
problems list shows and the Manage row counts.

| column | what it holds |
|---|---|
| `event_row` | the raw store's row |
| `calendar_id` | its calendar |
| `reason` | why, in a sentence |

## Apple Messages (`apple_messages`)

The store holds a copy of the Messages app's `chat.db`, the SQLite file
on a Mac at `~/Library/Messages/chat.db`, **table for table, with no
tables of its own**. Every table `chat.db` has is mirrored under its own
name and with its own columns, as Apple's schema has them on the Mac it
was read from, so the tables differ between macOS versions;
`.tables` and `.schema <table>` in `datalib-doltlite` list them. There
is no `payload` column: each row is the row `chat.db` had. Besides the
mirrored tables the store has only the shared `problems`, `sync_runs`
and `sync_scope_state`; this source writes no `sync_runs` rows.

What is not copied: indexes, triggers and views; a virtual table's
backing tables; generated columns; column defaults and CHECK or foreign
key rules. With `skip_churn` on (the default) the app's counters and
settings (`_SqliteDatabaseProperties`, `kvtable`), the work queues
(`index_state_metrics`, `message_processing_task`, `persistent_tasks`,
`scheduled_messages_pending_cloudkit_delete`), the tables named
`sync_*` and `unsynced_*`, and every `index_state` column are left out.
`include_tables`, `exclude_tables` and `exclude_columns` narrow it
further.

**Keys.** Each table is keyed as `chat.db` declares it: by `ROWID` for
`message`, `chat`, `handle` and `attachment`. A table with no declared
key and exactly one UNIQUE index with no NULLs in it is keyed on that
index; `chat_message_join` and `chat_handle_join` are keyed this way, on
their pair of ids. A table with neither is kept keyless.

**Nothing is ever removed.** Each sync writes every row `chat.db` has
over the row with the same key, so an edited row is replaced and a row
`chat.db` no longer has stays. A table `chat.db` dropped stays, and a
column it dropped stays, NULL in new rows; a column it gained is added.
A keyless table gains every row it does not already hold whole, so an
edited row there is kept twice. Each sync that changed anything is one
commit; `dolt_log()` lists them and `dolt_history_<table>` holds every
earlier version of a row.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a chat's guid (`iMessage;-;+1…` for a one-to-one chat, `iMessage;+;chat…` for a group) | `chat.guid` |
| a message's guid | `message.guid` |
| a phone number or Apple ID address | `handle.id` |
| a tapback's target | `message.associated_message_guid`, as `p:<part>/<guid>` or `bp:<guid>` |
| the thread an inline reply was sent in | `message.thread_originator_guid`, the guid of the thread's first message |
| an attachment's file | `attachment.filename`, a path under `~/Library/Messages/Attachments/` |

**Me** is `message.is_from_me = 1`; anyone else is `message.handle_id`
joined to `handle.ROWID`. A tapback is a `message` row whose
`associated_message_type` is between 2000 and 2006 (3000–3006 withdraws
one); `item_type` other than 0 is a group event.

A chat's messages, oldest first, with who sent each:

```sql
SELECT m.guid,
       datetime(m.date / 1000000000 + 978307200, 'unixepoch') AS sent_utc,
       CASE WHEN m.is_from_me = 1 THEN 'me' ELSE h.id END AS sender,
       m.text
  FROM chat c
  JOIN chat_message_join j ON j.chat_id = c.ROWID
  JOIN message m ON m.ROWID = j.message_id
  LEFT JOIN handle h ON h.ROWID = m.handle_id
 WHERE c.guid = :chat_guid
   AND m.associated_message_type = 0
 ORDER BY m.date, m.ROWID;
```

`text` is NULL for most messages written since macOS Ventura; the body
is in `attributedBody`, which only the rendered document decodes. The
`sent_utc` arithmetic assumes nanoseconds; a database from before about
2011 stores seconds.

Who is in each chat:

```sql
SELECT c.guid, c.display_name, h.id
  FROM chat c
  JOIN chat_handle_join ch ON ch.chat_id = c.ROWID
  JOIN handle h ON h.ROWID = ch.handle_id
 ORDER BY c.ROWID;
```

### Is the copy complete?

This source keeps no record of what is owed: each sync reads the whole
of `chat.db`. The copy is as complete as the last sync that ended
without a problem.

1. **Problems the download recorded.** A `phase:source` row means the
   last sync read nothing: `chat.db` held no table (a 0-byte copy, the
   wrong file), the filters matched none, or a table in it is named like
   one the store keeps for itself. The copy is then as the sync before
   left it.

   ```sql
   SELECT scope_key, severity, sample, changed_at_utc
     FROM problems
    ORDER BY changed_at_utc DESC;
   ```

   A sync that failed outright (no access to `~/Library/Messages`, a
   file that is not SQLite) records nothing here and commits nothing; it
   shows as a failed step on the Manage row.

2. **When the copy last changed.** `sync_runs` stays empty for this
   source. The newest commit is the last sync that changed anything:

   ```sql
   SELECT commit_hash, date, message FROM dolt_log() LIMIT 1;
   ```

   A sync that found nothing new makes no commit, so this date says
   when Messages last changed, not when Datalib last looked. When the
   last sync ran and whether it succeeded is the step's row in
   `<data_root>/system/supervisor.sqlite`
   (`SELECT last_status, last_success_at_utc, last_error FROM steps WHERE step = '<source id>/ingest'`).

3. **What render left out** is in the render step's own store, not this
   one: a `message:<rowid>` problem for each message in no chat, and a
   problem on each message whose body would not decode. Both count on
   the source's Manage row.

## Apple Photos (`apple_photos`)

The store holds a copy of an Apple Photos library's database,
`<library>.photoslibrary/database/Photos.sqlite`, **table for table,
with no tables of its own**. Every table in it is mirrored under its own
name with its own columns, as Core Data laid them out on the Mac it was
read from: about ninety tables, whose number and join-table names
change between macOS versions. `.tables` and `.schema <table>` in
`datalib-doltlite` list them. There is no `payload` column: each row is
the row `Photos.sqlite` had. Besides the mirrored tables the store has
only the shared `problems`, `sync_runs` and `sync_scope_state`; this
source writes no `sync_runs` rows.

What is not copied: indexes, triggers and views; the R-tree's backing
tables (`Z_RT_Asset_boundedByRect_node`, `_parent`, `_rowid`), while
`Z_RT_Asset_boundedByRect` itself is copied as a plain table of rows;
any virtual table this build cannot read; generated columns; column
defaults and CHECK or foreign key rules. With `skip_history` on (the
default) the tables `ACHANGE`, `ATRANSACTION`, `ATRANSACTIONSTRING`,
`ZBACKGROUNDJOBWORKITEM`, `Z_PRIMARYKEY`, `Z_METADATA`, `Z_MODELCACHE`
and every `Z_OPT` column are left out. `include_tables`,
`exclude_tables` and `exclude_columns` narrow it further.

**Each sync replaces the whole copy.** The mirror drops every table and
fills it again from the library; the storage engine stores only what
changed, so a sync of an unchanged library makes no commit. A table the
library no longer has is gone from the current copy. Earlier states stay
readable: `dolt_history_<table>` holds every version of each row,
`dolt_diff_<table>` what each commit changed, and `<file>@<commit>`
opened read-only is the whole library as it was.

**Keys.** A table with a `ZUUID` column that is distinct and non-NULL
in every row is keyed on `ZUUID`; others keep their declared `Z_PK`. A
table with no declared key and exactly one UNIQUE index with no NULLs in
it is keyed on that index; with neither it is keyless and diffs by row
position. The commit message of each sync records `stable_keys=<n>`,
how many tables were keyed on `ZUUID`.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a photo's or video's UUID (the name of its file in `originals/`) | `ZASSET.ZUUID` |
| an asset's row number in Photos | `ZASSET.Z_PK`, which other tables reference (`ZADDITIONALASSETATTRIBUTES.ZASSET`, the join tables) |
| the file | `originals/<ZASSET.ZDIRECTORY>/<ZASSET.ZFILENAME>` under the bundle |
| its name when imported | `ZADDITIONALASSETATTRIBUTES.ZORIGINALFILENAME` |
| an album | `ZGENERICALBUM.Z_PK`, its title `ZGENERICALBUM.ZTITLE` |

The column names above are from a macOS 26 library; check them with
`.schema` on yours.

Photos favourited or un-favourited by the latest sync:

```sql
SELECT to_ZUUID, from_ZFAVORITE, to_ZFAVORITE
  FROM dolt_diff_ZASSET
 WHERE diff_type = 'modified' AND to_commit = 'HEAD'
   AND from_ZFAVORITE IS NOT to_ZFAVORITE;
```

Every asset with its file and capture time (Core Data seconds since
2001-01-01 UTC):

```sql
SELECT ZUUID, ZDIRECTORY || '/' || ZFILENAME AS file,
       datetime(ZDATECREATED + 978307200, 'unixepoch') AS created_utc,
       ZFAVORITE, ZHIDDEN, ZTRASHEDSTATE
  FROM ZASSET
 ORDER BY ZDATECREATED;
```

A photo the library no longer has, as it last was:

```sql
SELECT commit_hash, commit_date, ZFILENAME
  FROM dolt_history_ZASSET
 WHERE ZUUID = :uuid
 ORDER BY commit_date DESC LIMIT 1;
```

### Is the copy complete?

This source keeps no record of what is owed: each sync reads the whole
of `Photos.sqlite`. The copy is as complete as the last sync that ended
without a problem.

1. **Problems the download recorded.** A `phase:source` row means the
   last sync read nothing: the file held no table, the filters matched
   none, or a table in it is named like one the store keeps for itself.
   The copy is then as the sync before left it.

   ```sql
   SELECT scope_key, severity, sample, changed_at_utc
     FROM problems
    ORDER BY changed_at_utc DESC;
   ```

   A sync that failed outright (no access to the library, a path that is
   not a Photos database) records nothing here and commits nothing; it
   shows as a failed step on the Manage row.

2. **When the copy last changed, and what the sync skipped.**
   `sync_runs` stays empty for this source. The newest commit is the last
   sync that changed anything, and its message carries the run's counts,
   among them `virtual_tables_skipped` (tables that could not be read
   and are not in the copy):

   ```sql
   SELECT commit_hash, date, message FROM dolt_log() LIMIT 1;
   ```

   A sync that found nothing new makes no commit, so this date says when
   the library last changed, not when Datalib last looked. When the last
   sync ran and whether it succeeded is the step's row in
   `<data_root>/system/supervisor.sqlite`
   (`SELECT last_status, last_success_at_utc, last_error FROM steps WHERE step = '<source id>/ingest'`).

## Baby Connect (`baby_connect`)

Baby Connect publishes no API. The download makes the two requests
Baby Connect's own web page at `app.babyconnect.com` makes: `GET /home2`,
whose inline script lists the account's children, and
`POST /CmdListW?cmd=StatusList` once per day, whose answer lists that
day's entries (`list`) and the people who logged them (`users`). Each
`payload` is one object from those answers, as sent, less the fields
set aside below. Every row is keyed by Baby Connect's own id.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a child's id | `kids.id`, and `payload.Kid` on an entry |
| the id of whoever logged an entry | `users.id`, and `payload.By` on an entry |
| an entry's id | `entries.id` |
| a day that has been read | inside a `coverage` row with `scope = 'days'` |

Baby Connect's ids are JSON numbers larger than 2^53. The `id` columns
keep their digits as text. Inside a payload they are integers, which
SQLite reads exactly, so compare them as text: `CAST(json_extract(payload,
'$.Kid') AS TEXT) = kids.id`.

**Times.** An entry's `Pdt` is its day as a number `YYMMDD`, the year
counted from 2000 (`310317` is 2031-03-17), and `Utm` its clock time as a
number `HHMM` (`905` is 09:05). Neither names a time zone, and nothing
else Baby Connect sends does; the render step's `time_zone` is what
makes them a moment. As written down from the web page's own scripts,
an entry's `e` is when it was typed, often hours later, and `ms` is not
the entry's moment either.

**"Me".** Nothing in the store marks which `users` row is the account
that signed in.

Every entry of one child, in order, with who logged it:

```sql
SELECT e.id,
       printf('20%02d-%02d-%02d',
              json_extract(e.payload, '$.Pdt') / 10000,
              json_extract(e.payload, '$.Pdt') / 100 % 100,
              json_extract(e.payload, '$.Pdt') % 100) AS day,
       json_extract(e.payload, '$.Utm') AS hhmm,
       json_extract(e.payload, '$.Cat') AS category,
       json_extract(e.payload, '$.Txt') AS sentence,
       json_extract(e.payload, '$.n') AS note,
       json_extract(u.payload, '$.Name') AS logged_by
  FROM entries e
  JOIN kids k ON k.id = CAST(json_extract(e.payload, '$.Kid') AS TEXT)
  LEFT JOIN users u ON u.id = CAST(json_extract(e.payload, '$.By') AS TEXT)
 WHERE json_extract(k.payload, '$.Name') = 'Wesley'
 ORDER BY json_extract(e.payload, '$.Pdt'), json_extract(e.payload, '$.Utm');
```

`Cat` is the web page's category code: a kind of entry is a multiple of
50 and its variants sit just above it (400 diaper, 401 to 403 wet, BM
and mixed; 350 nursing; 500 sleep; 2600 diary). How many of each:

```sql
SELECT json_extract(payload, '$.Cat') / 50 * 50 AS kind, count(*)
  FROM entries GROUP BY kind ORDER BY 2 DESC;
```

**Set aside from the payload** (in `volatile_payload`): `url_t`, a
signed picture address that changes on every load, on `kids` and
`users`; `readby`, the list of who has opened an entry, on `entries`.

The sidecars' `fetched_at_utc` is when a row was **first** seen: a day
read again with nothing changed writes nothing, and an edited entry's
payload is replaced without moving the stamp. `held_version` is not used.

### Is the copy complete?

1. **Problems.** A day whose request failed leaves a `listing:entries`
   row (an error). Several failed days share that one row, and its
   `sample` names only one of them; use the `coverage` query below to
   list them all. `phase:entries` means no child has a birthday and
   `since` is not set, so nothing was read. A refused session fails the
   run instead (see `sync_runs`).

   ```sql
   SELECT scope_key, severity, sample FROM problems
    WHERE scope_key IN ('listing:entries', 'phase:entries');
   ```

2. **Days not yet read.** The store keeps which days it has read, so
   what is owed is every day of the window that no `coverage` span
   holds, except today and yesterday, which are read on every sync and
   never recorded. Gaps between spans are days a sync skipped:

   ```sql
   SELECT date(hi, '+1 day') AS first_missing, date(next_lo, '-1 day') AS last_missing
     FROM (SELECT lo, hi, lead(lo) OVER (ORDER BY lo) AS next_lo
             FROM coverage WHERE scope = 'days')
    WHERE next_lo IS NOT NULL AND date(hi, '+1 day') < next_lo;
   ```

   And the two ends: the copy reaches back far enough when `min(lo)` is
   no later than `since` (or the eldest child's birthday), and is up to
   date when `max(hi)` is the day before yesterday (or `until`):

   ```sql
   SELECT min(lo) AS first_day_read, max(hi) AS last_day_settled
     FROM coverage WHERE scope = 'days';
   ```

3. **The last run.** The newest `sync_runs` row's `status` is `ok` when
   it read every day it set out to; `partial` when it stopped early.

   ```sql
   SELECT started_at_utc, status, summary FROM sync_runs ORDER BY run_id DESC LIMIT 1;
   ```

   None of this can show an entry edited or deleted on a day already
   settled: the store never asks for that day again.

### `kids`

One row per child the account's home page lists.

| column | what it holds |
|---|---|
| `id` | the child's Baby Connect id |
| `payload` | the child as the home page's `myKids` lists them: `Id`, `Name`, `BDay` (`M/D/YYYY`) and more |

### `users`

One row per person a day's answer named as having logged an entry: a
parent, a caregiver.

| column | what it holds |
|---|---|
| `id` | the person's Baby Connect id |
| `payload` | the person as the day's `users` named them: `Name`, `Email` and more |

### `entries`

One row per logged entry: a feed, a sleep, a diaper, a dose, a
measurement, a diary entry. A day's answer also carries a few entries
from the days beside it; only the day's own are stored from it, so each
entry is stored from its own day's answer.

| column | what it holds |
|---|---|
| `id` | the entry's Baby Connect id |
| `payload` | the entry as sent: `Kid` (the child), `Pdt` (the day), `Utm` (the time), `Cat` (the kind), `By` (who logged it), `Txt` (the app's sentence for it), `n` (the typed note), `d` (a duration in minutes), `photos`, `comments` |

### `coverage`

The days the download has read for good, as spans. A day is recorded
here once it is more than a day old; today and yesterday never are, so
every sync reads them again. A day whose request failed is not here.

| column | what it holds |
|---|---|
| `scope` | always `days` |
| `lo` | the first day of the span, `YYYY-MM-DD` |
| `hi` | the last day of the span, `YYYY-MM-DD`, inclusive |

## Beeper (`beeper`)

The store is a copy of Beeper Texts' own cache, so it has no `payload`
column. Its main tables are the app's own SQLite tables, copied table
for table by the shared SQLite mirror. They are not listed below because
their columns are whatever the installed version of the app writes, and
Datalib creates them on the first sync rather than when the store is
made: **the store has no tables of its own for the messages**, only
`beeper_media_files`. `.schema` in `datalib-doltlite` shows the columns
a given store has.

How the mirrored tables are named and kept:

- **A mirrored table has the app's name for it, unchanged**, and the
  app's column names and declared types, with no prefix. There are six:
  five from `index.db` and one from the on-device bridges.
- **Only these tables are copied**; the app's other tables are not.
- **Rows are only added or replaced, never deleted.** A row is replaced
  when the app rewrites it under the same key, and kept when the app
  drops it. A column the app adds later is added to the mirror as a
  nullable column; a column the app drops stays, NULL in new rows.
  Column defaults are not copied.
- **Every on-device bridge writes the same `message` schema**, so all
  bridges' rows share one `message` table, told apart by `bridge_id`.
  Cloud bridges (Slack, Google Chat, Telegram) have no bridge database
  and no `message` rows.

| mirrored table | from | one row is | key in the mirror |
|---|---|---|---|
| threads | `index.db` | one chat; `thread` is JSON with its title, type and the network's own ids under `extra.bridge.channel` | `threadID` |
| participants | `index.db` | one person in one chat: `full_name`, `nickname`, `img_url`, `is_self` | `room_id`, `id` |
| participant_identifiers | `index.db` | one phone number, address or username the app knows for a person (`identifier_type`: `phone`, `email`, `username`) | `account_id`, `participant_id`, `identifier` |
| mx_room_messages | `index.db` | one Matrix event in a chat: `type`, `timestamp` (ms), `senderContactID`, `text_content`, and the full event as JSON in `message` | `roomID`, `eventID` |
| mx_reactions | `index.db` | one reaction: `eventID` is the message it is on, `description` the emoji | `roomID`, `reactionID` |
| message | each `local-*/megabridge.db` | an on-device bridge's record of one message: the network's own id (`id`, `part_id`) beside its Matrix event (`mxid`) | `bridge_id`, `mxid` |

The app's `mx_room_messages.id` column (a local row number the app
renumbers on a re-index) and the bridge's `message.rowid` are left out.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a chat (Matrix room id, `!…`) | `threads.threadID`, `participants.room_id`, `mx_room_messages.roomID`, `mx_reactions.roomID` |
| a message (Matrix event id, `$…`) | `mx_room_messages.eventID`; `message.mxid` for an on-device bridge |
| a person (Matrix user id, `@…`) | `participants.id`, `participant_identifiers.participant_id`, `mx_room_messages.senderContactID`, `mx_reactions.senderID` |
| the network's account | `threads.accountID`; its first part names the bridge (`local-signal_…`, `slackgo.…`, `googlechat`) |
| Signal's own id for a message | `message.id`, `<sender ACI>\|<sent ms>` |
| an attachment `mxc://<server>/<id>` | `beeper_media_files.rel_path = '<server>/<id>'` |
| an on-device attachment `localmxc://<bridge>/<id>` | `beeper_media_files.rel_path = 'localhost<bridge>/<id>'` |

The JSON columns (`threads.thread`, `mx_room_messages.message`) are
stored as text, as the app wrote them, so `json_extract` reads them
directly.

A chat's messages, oldest first, with who sent them:

```sql
SELECT m.timestamp, coalesce(p.full_name, p.nickname, m.senderContactID) AS sender,
       m.type, coalesce(m.text_content, json_extract(m.message, '$.text')) AS text
  FROM mx_room_messages m
  LEFT JOIN participants p ON p.room_id = m.roomID AND p.id = m.senderContactID
 WHERE m.roomID = :thread_id
 ORDER BY m.timestamp, m.hsOrder;
```

Chats by network account, with their titles:

```sql
SELECT threadID, accountID, json_extract(thread, '$.title') AS title
  FROM threads ORDER BY accountID;
```

An attachment's bytes: the attachment ids are in
`json_extract(mx_room_messages.message, '$.attachments')`; map each to a
`rel_path` as in the table above, then look its `blake3` up in
`blobs.sqlite`.

### Is the copy complete?

Beeper keeps no record of what is owed: it copies whatever the app has
cached, and writes no `sync_runs` rows, so there is no run status to
read. What can be checked is `problems`:

```sql
SELECT scope_key, severity, sample, changed_at_utc
  FROM problems
 WHERE scope_key LIKE 'phase:%'
    OR scope_key LIKE 'record:beeper_media_files:%'
 ORDER BY changed_at_utc DESC;
```

- `phase:source`: the app's `index.db` held no table to copy (an empty
  or wrong file). The copy kept what it had and took in nothing new.
- `phase:megabridge local-<bridge>`: that on-device bridge's database
  did not read; its earlier `message` rows stay. `phase:megabridge`
  alone: the app's data directory could not be listed.
- `phase:media`: some folders under `media/` would not list, so files
  under them were not copied.
- `record:beeper_media_files:<path>`: one cached file would not read.

A row stays until a later run does the same thing without failing. A
missing `index.db` fails the step outright and writes nothing. Chats
left out of the documents because their network is not recognized are
reported by the render step (`unlisted_bridges:<bridge>`), in the
render's problems, not in this store.

To see whether every cached file is held, compare `beeper_media_files`
with a listing of the app's `media/` folder.

### `beeper_media_files`

One row per file copied out of the app's `media/` folder. A file the app
later evicts keeps its row.

| column | what it holds |
|---|---|
| `rel_path` | the file's path under `media/`, `<server>/<media id>` |
| `blake3` | the hash of its bytes, the key of its row in `blobs.sqlite`'s `cas_objects` |
| `size_bytes` | its size in bytes |

## CardDAV, Fastmail and `.vcf` contacts (`contacts`)

Each card is kept as the vCard text the server or the file gave,
unchanged: `payload` is `{"vcard": "<BEGIN:VCARD … END:VCARD>"}`, read
with `json_extract(payload, '$.vcard')`. A `.vcf` file is split into
one row per card. A card's inline photo stays in that text, and is also
decoded into the source's blob store, `blobs.sqlite` beside the store,
under the blake3 hash of its bytes.

The tables `contact_group_members` and `contact_categories` are derived from the cards:
they are rewritten every time their card is written, and removed with
it.

This source writes no `sync_runs` rows; the table is there and stays
empty.

**Which id is which**

| upstream | where it is |
|---|---|
| the server's host, or `local` for files | `accounts.id`, `addressbooks.account_id` |
| an address book's URL path on the server; a file's path under the folder | `addressbooks.href` |
| an address book | `addressbooks.id` = `<account id>!<href>`; `contacts.addressbook_id` |
| a card's `UID` (or, for a `.vcf` card with none, an id made from its name) | `contacts.uid` |
| a card | `contacts.id` = `<addressbook id>#<uid>` |
| a card's URL path on the server; for a file, the file's path, with `#<n>` for its n-th card after the first | `contacts.href`, `dav_resources.href` |
| a card's version on the server (its etag) | `contacts.etag`, `dav_resources.etag` |

**There is no "me".** An address book does not say which card is yours.

Every card with its address book's name:

```sql
SELECT c.id, c.display_name, COALESCE(a.display_name, a.href) AS address_book
  FROM contacts c
  LEFT JOIN addressbooks a ON a.id = c.addressbook_id
 ORDER BY address_book, c.display_name;
```

A group's members, with their names where the member's card is held:

```sql
SELECT g.display_name AS grp, m.member, c.display_name AS member_name
  FROM contact_group_members m
  JOIN contacts g ON g.id = m.group_id
  LEFT JOIN contacts c ON c.id = m.member_id
 ORDER BY grp, member_name;
```

Every contact filed under one label (Google's `starred`, here):

```sql
SELECT c.display_name
  FROM contact_categories k
  JOIN contacts c ON c.id = k.contact_id
 WHERE k.category = 'starred';
```

`display_name` is the card's `FN` line as written, with vCard escapes
(`\,`) left in; read the card itself for anything else.

### Is the copy complete?

How to tell whether a `contacts` copy is complete:

1. **Problems.** Any row here means something is missing or failed:

   ```sql
   SELECT scope_key, severity, reason, sample FROM problems ORDER BY scope_key;
   ```

   What the `scope_key` prefixes mean:

   - `dav_resources:<addressbook id>#<href>`: one card is not in the
     copy. `reason` `no_identity` (a warning) is a card with no `UID`,
     which will not be copied until it is edited upstream; `fetch_failed`
     is a card the server listed and did not return, which the next sync
     asks for again.
   - `listing:addressbook <name>`: that address book's sync failed, or
     was stopped short by the server or the 50-page limit; its
     deletions have not been applied.
   - `phase:multiget <name>`: fetching that book's cards stopped after
     repeated failures; the rest are owed.
   - `config:addressbooks:<name>`: a configured name no address book
     has.
   - `listing:vcf <path>`, `record:files:<path>`, `listing:files`: a
     `.vcf` file that could not be read or parsed, or a folder that could
     not be walked (nothing is deleted on a run with a walk error).

2. **What is still owed** (CardDAV only). A card is owed when the last
   listing named it at a version the store does not hold:

   ```sql
   SELECT r.collection AS addressbook_id, r.href, r.etag
     FROM dav_resources r
     LEFT JOIN dav_resources_bookkeeping b ON b.id = r.id
    WHERE b.fetched_at_utc IS NULL OR b.held_version IS NOT r.etag;
   ```

   And a listing still under way, whose deletions are not applied yet:

   ```sql
   SELECT collection, count(*) AS not_yet_confirmed
     FROM dav_unconfirmed GROUP BY collection;
   ```

   Both empty, with no problems, means the copy matches what the server
   last listed. A card with no `UID` is not owed (it is held, at its
   version, as unusable): only the problems query shows it.

3. **Whether the last run ended well.** This source writes no
   `sync_runs` rows. A run that failed outright (the server could not be
   reached, discovery failed, every configured name was wrong) may leave
   no problem row either. Read the loop's record instead:

   ```sh
   sqlite3 <data_root>/system/supervisor.sqlite \
     "SELECT step, state, last_status, last_failure_kind, last_success_at_utc
        FROM steps WHERE step = '<source id>/ingest'"
   ```

   For `.vcf` files there is no owed record; the problems query and the
   loop's record are the whole answer.

### `accounts`

The server this source mirrors, or `local` for a folder of files. One
row per host; a source whose server changed host keeps the old row.

| column | what it holds |
|---|---|
| `id` | the server's host (`carddav.fastmail.com`), or `local` for files |
| `server_url` | the configured server URL without a trailing `/`; `file://<path>` for files |
| `principal_href` | the full URL of the account's CardDAV principal that discovery found; NULL for files |
| `addressbook_home_set` | the full URL under which the server lists the address books; NULL for files |

### `addressbooks`

One row per address book the server lists, including those the
`addressbooks` setting leaves out, or one per `.vcf` file.

| column | what it holds |
|---|---|
| `id` | `<account id>!<href>` |
| `account_id` | the `accounts.id` |
| `href` | the address book's URL path as the server wrote it; for a file, its path under the folder |
| `display_name` | its name as the server shows it; for a file, the file name without `.vcf` |
| `description` | the server's description of it, if any |
| `ctag` | the server's `getctag` version of the whole book, if it sends one |
| `sync_token` | the `sync-collection` token the next sync starts from; NULL means the next sync lists the book whole |

### `contacts`

One row per card.

| column | what it holds |
|---|---|
| `id` | `<addressbook id>#<uid>` |
| `payload` | `{"vcard": "<the card's text>"}` (JSONB) |
| `addressbook_id` | the `addressbooks.id` |
| `uid` | the card's `UID`; for a `.vcf` card without one, an id made from its name, or `<file stem>:<file stem>:<position>` when it has no name either |
| `href` | the card's URL path on the server; for a file, the file's path, with `#<n>` after it for the n-th card after the first |
| `etag` | the server's version of the card; NULL for a file |
| `display_name` | the `FN` line, as written |
| `revision` | the `REV` line, as written |

### `contact_group_members`

One row per member a group card names.

| column | what it holds |
|---|---|
| `id` | `<group id>#<member>` |
| `group_id` | the `contacts.id` of the group's card |
| `addressbook_id` | the `addressbooks.id` |
| `member` | the member as the card writes it: `urn:uuid:<UID>`, a bare UID, or another URI such as `mailto:` |
| `member_id` | the `contacts.id` a member named by UID would have, whether or not that row exists; NULL for a member named any other way |

### `contact_categories`

One row per name in a card's `CATEGORIES`.

| column | what it holds |
|---|---|
| `id` | `<contact id>#<category>` |
| `contact_id` | the `contacts.id` |
| `addressbook_id` | the `addressbooks.id` |
| `category` | the name as the card writes it (`myContacts`, `starred`, or one the person made) |

### `dav_resources`

What a CardDAV listing named: one row per card on the server, with the
version it was listed at. A card listed here and not held at that
version is still owed. Empty for files.

| column | what it holds |
|---|---|
| `id` | `<addressbook id>#<href>` |
| `collection` | the `addressbooks.id` |
| `href` | the card's URL path |
| `etag` | the version the listing named |

### `dav_unconfirmed`

Cards a whole listing that stopped part way had not reached yet. Nothing
is deleted for being missing from a listing until a run carries the
listing to its end.

| column | what it holds |
|---|---|
| `collection` | the `addressbooks.id` |
| `href` | the card's URL path |

## ChatGPT (`chatgpt`)

`payload` is chatgpt.com's own JSON. A conversation is the answer of
`/backend-api/conversation/{id}`, with two changes so that an unchanged
conversation stores the same bytes: the top-level lists chatgpt.com
returns in no fixed order (`safe_urls`, `blocked_urls`,
`disabled_tool_ids`, `plugin_ids`) are sorted, and each message's
`metadata.model_switcher_deny` is set aside (below). A conversation
from a data export is the export's entry stored the same way; it lacks
tool and system messages and every node's `children`. `me` is the
answer of `/backend-api/me`, or the export's `user.json`.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a conversation id (`https://chatgpt.com/c/<id>`) | `conversations.id`, `chatgpt_attachments.conversation_id` |
| a message id | a key of `conversations.payload -> '$.mapping'`, and that node's `message.id`; unique only within one conversation |
| the account id `user-…` | `me.id` |
| a file id `file-…` | `chatgpt_attachments.file_id`; `id` of an entry in a message's `metadata.attachments`, or the end of an image part's `asset_pointer` |
| a Project or custom GPT id `g-…` | `$.gizmo_id` in a conversation's payload (a Project's starts `g-p-`) |

**Me.** `me` holds the account. A message whose `author.role` is `user`
is the account's; `assistant` is ChatGPT, and `tool` and `system` are
the machinery's.

**Times.** `conversations.update_time` is the conversation's own
`update_time`, a Unix time in seconds with a fraction, stored as text
(`1710959331.420159`). `datetime(CAST(update_time AS REAL), 'unixepoch')`
reads it as a time. Each message's `create_time` is the same kind of
number.

**A conversation's messages, on the branch the page shows:**

```sql
WITH RECURSIVE node(id, parent, role, at, msg) AS (
  SELECT n.key, n.value ->> '$.parent', n.value ->> '$.message.author.role',
         n.value ->> '$.message.create_time', json_extract(n.value, '$.message')
    FROM conversations c, json_each(c.payload, '$.mapping') n
   WHERE c.id = :conversation),
branch(id, parent, role, at, msg, depth) AS (
  SELECT node.*, 0 FROM node, conversations c
   WHERE c.id = :conversation AND node.id = c.payload ->> '$.current_node'
  UNION ALL
  SELECT node.*, branch.depth + 1 FROM node JOIN branch ON node.id = branch.parent)
SELECT datetime(at, 'unixepoch') AS at, role,
       msg ->> '$.content.content_type' AS content_type,
       msg -> '$.content.parts' AS parts
  FROM branch WHERE msg IS NOT NULL ORDER BY depth DESC;
```

**Files and their bytes:**

```sql
ATTACH 'file:chatgpt/ingest/blobs.sqlite?mode=ro' AS cas;
SELECT a.conversation_id, a.file_id, o.content_type, o.byte_len
  FROM chatgpt_attachments a JOIN cas.cas_objects o ON o.blake3 = a.blake3;
```

A file's name and type are in the message that names it, not here.

**A conversation whose every fetch failed** is a row with only its
`id`: `payload`, `title` and `update_time` are NULL.

**Set aside from the payload** (in `volatile_payload`): each message's
`metadata.model_switcher_deny`, which grows whenever OpenAI ships a
model with the conversation unchanged; and the account's `first_name`,
which `/me` answers on some calls and not others. A row stored before
these were set aside keeps them in its payload until it is fetched
again.

### Is the copy complete?

How to tell whether the copy is complete, from the store alone.

**1. What the last runs could not do.** Every row here is something
missing or stale:

```sql
SELECT severity, scope_key, reason, sample FROM problems ORDER BY severity, scope_key;
```

The `scope_key` says what is missing:

- `conversations:<id>` — a conversation that would not fetch. `error`
  means the copy has never held it; `warning` means it holds an older
  copy.
- `chatgpt_attachments:<conversation>#<file>` — a file without its
  bytes. `reason = 'not_found'` is a file chatgpt.com answered 404 or
  410 for; it is not asked for again until its conversation changes.
- `listing:conversations` — a listing page failed. Conversations past
  that page were not seen this run, and no deletion was followed.
- `phase:conversations`, `phase:attachments` — the run stopped there
  (the rate limit, or 25 failures in a row); the rest is left for the
  next run.
- `config:conv_uuids:<value>` — a named conversation chatgpt.com does
  not have.
- `phase:attachments` from an `export`-only source — files the export
  has no copy of; they stay owed.
- `phase:export`, `phase:conversations.json` — entries of the export
  with no id, which were not stored.

**2. What is still owed.** Each sidecar's `held_version` is the version
held: a conversation's `update_time` in whole seconds, a file the
version its conversation was held at. What chatgpt.com lists now is not
in the store, so a conversation changed upstream since the last run
cannot be seen from here. What the store does show:

```sql
SELECT 'conversations never fetched' AS what, count(*) AS n
  FROM conversations WHERE payload IS NULL
UNION ALL
SELECT 'conversations whose last fetch failed', count(*)
  FROM conversations_bookkeeping WHERE last_error IS NOT NULL
UNION ALL
SELECT 'files not held', count(*)
  FROM chatgpt_attachments a
  LEFT JOIN chatgpt_attachments_bookkeeping b ON b.id = a.id
  LEFT JOIN conversations_bookkeeping c ON c.id = a.conversation_id
 WHERE b.fetched_at_utc IS NULL OR b.held_version IS NOT c.held_version;
```

A file chatgpt.com answered 404 for is held (with its `not_found`
problem) and is not counted.

**3. How the last API run ended:**

```sql
SELECT run_id, started_at_utc, finished_at_utc, status,
       json_extract(summary, '$.listing') AS listed,
       json_extract(summary, '$.fetched') AS fetched,
       json_extract(summary, '$.errors') AS errors,
       json_extract(summary, '$.pruned') AS deleted,
       json_extract(summary, '$.failed_blobs') AS failed_files
  FROM sync_runs ORDER BY run_id DESC LIMIT 1;
```

`partial` means the run stopped early and the next carries on. A run
that stopped at `max_conversations_per_run` still says `ok`: when that
is set and `fetched + errors` equals it, more may be owed. A source with
only an `export` table writes no `sync_runs` row; there, a row in
`ingested_files` with scope `chatgpt:export` means the last read of the
export finished whole.

### `me`

The account the store mirrors: one row.

| column | what it holds |
|---|---|
| `id` | the account id, `user-…` |
| `payload` | `/backend-api/me`'s answer, or the export's `user.json`, without `first_name` |
| `email` | the account's email address |
| `name` | the account's name |

### `conversations`

One row per conversation.

| column | what it holds |
|---|---|
| `id` | the conversation id |
| `payload` | the whole conversation (above); NULL for one whose every fetch failed |
| `title` | its title |
| `update_time` | its `update_time` as the conversation itself gives it: Unix seconds with a fraction, as text |

### `chatgpt_attachments`

One row per file a conversation names: the link from the conversation
to the bytes in `blobs.sqlite`.

| column | what it holds |
|---|---|
| `id` | `<conversation id>#<file id>` |
| `conversation_id` | the conversation that names it |
| `file_id` | the file's id, its `sediment://` or `file-service://` prefix removed |
| `blake3` | the hash of its bytes in `blobs.sqlite`; NULL until they land, and for a file chatgpt.com no longer has |

`ingested_files` (§ "What every raw store has") holds, under the scope
`chatgpt:export`, the fingerprint of each file of the data export as it
was when last read whole.

## Claude (`claude`)

`payload` is claude.ai's own JSON. A conversation is the answer of
`/api/organizations/{org}/chat_conversations/{id}?tree=True&rendering_mode=messages&render_all_tools=true`
with every key whose value is `null` left out. A project is its entry in
`/api/organizations/{org}/projects`, a knowledge document its entry in
`…/projects/{id}/docs`. A Cowork task is its entry in claude.ai's
sessions listing (`/v1/code/sessions?tags=cowork-remote`), and each
event of its log is one entry of `…/sessions/{id}/events`. A store
filled by the `export` method holds the export's JSON as the export
wrote it instead: `conversations.json`, `users.json` and
`projects/*.json`, a project's nested `docs` split out into
`project_docs`.

A conversation from the API differs from an exported one in three ways
render smooths over: it has no `account`, a message's `text` is empty
(the words are in `content[]`), and its org columns are set. An exported
conversation has `org_uuid` and `org_name` NULL, since an export names
no organization.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a conversation UUID (`https://claude.ai/chat/<uuid>`) | `conversations.id` |
| a message UUID | `uuid` of an entry in `conversations.payload -> '$.chat_messages'` |
| a project UUID (`https://claude.ai/project/<uuid>`) | `projects.id`, `project_docs.project_uuid`, `project_docs_listings.id`, and `$.project.uuid` in a conversation's payload |
| a knowledge document UUID | `project_docs.id` |
| an organization UUID | `orgs.id`, `conversations.org_uuid`, `projects.org_uuid`, `cowork_sessions.org_uuid` |
| the account UUID | `users.id` |
| a Cowork task id `cse_…` (`https://claude.ai/cowork/<id>`) | `cowork_sessions.id`, `cowork_events.session_id` |
| a file UUID | `claude_attachments.file_uuid`, and `file_uuid` in a message's `files[]` |

**Me.** `users` holds the account, and a message whose `sender` is
`human` is the account's; `assistant` is Claude.

**A conversation's messages, on the branch the page shows.**
`chat_messages` holds every version of every message. This walks back
from `current_leaf_message_uuid`:

```sql
WITH RECURSIVE msg(uuid, parent, sender, at, j) AS (
  SELECT m.value ->> '$.uuid', m.value ->> '$.parent_message_uuid',
         m.value ->> '$.sender', m.value ->> '$.created_at', m.value
    FROM conversations c, json_each(c.payload, '$.chat_messages') m
   WHERE c.id = :conversation),
branch(uuid, parent, sender, at, j, depth) AS (
  SELECT msg.*, 0 FROM msg, conversations c
   WHERE c.id = :conversation
     AND msg.uuid = c.payload ->> '$.current_leaf_message_uuid'
  UNION ALL
  SELECT msg.*, branch.depth + 1 FROM msg JOIN branch ON msg.uuid = branch.parent)
SELECT at, sender, uuid FROM branch ORDER BY depth DESC;
```

**The words of each message.** In a conversation from the API a
message's `text` is empty; read its `text` blocks:

```sql
SELECT m.value ->> '$.created_at' AS at, m.value ->> '$.sender' AS sender,
       b.value ->> '$.text' AS text
  FROM conversations c, json_each(c.payload, '$.chat_messages') m,
       json_each(m.value, '$.content') b
 WHERE c.id = :conversation AND b.value ->> '$.type' = 'text'
 ORDER BY at, CAST(b.key AS INTEGER);
```

**Conversations by Project:**

```sql
SELECT c.id, c.name, p.name AS project
  FROM conversations c
  LEFT JOIN projects p ON p.id = c.payload ->> '$.project.uuid'
 ORDER BY c.updated_at DESC;
```

**A message's two kinds of attachment.** `files[]` are downloadable
files; their bytes are in `blobs.sqlite`, through `claude_attachments`.
`attachments[]` are text Claude extracted from an upload, inline in the
payload as `extracted_content`; claude.ai keeps no file for them.

```sql
ATTACH 'file:claude/ingest/blobs.sqlite?mode=ro' AS cas;
SELECT a.conversation_uuid, a.file_uuid, o.content_type, o.byte_len
  FROM claude_attachments a JOIN cas.cas_objects o ON o.blake3 = a.blake3;
```

**A conversation whose every fetch failed** is a row with only its
`id`: `payload`, `org_uuid` and the rest are NULL.

### Is the copy complete?

How to tell whether the copy is complete, from the store alone.

**1. What the last runs could not do.** Every row here is something
missing or stale:

```sql
SELECT severity, scope_key, reason, sample FROM problems ORDER BY severity, scope_key;
```

The `scope_key` says what is missing:

- `conversations:<id>` — a conversation that would not fetch. `error`
  means the copy has never held it; `warning` means it holds an older
  copy.
- `claude_attachments:<owner>#<file>` — a file without its bytes.
  `reason = 'not_found'` is a file claude.ai answered 404 or 410 for; it
  is not asked for again until its conversation changes.
- `cowork_sessions:<id>` — a Cowork task whose log would not fetch.
- `project_docs_listings:<project>` — a Project's documents that would
  not list (a `forbidden` warning when claude.ai refused).
- `listing:conversations org:<org>`, `listing:projects org:<org>`,
  `listing:cowork org:<org>` — one organization's listing failed:
  nothing new of it was fetched and none of its deletions followed.
- `listing:org:<org>` — the organization refuses the credential: nothing
  of it is mirrored.
- `phase:conversations`, `phase:projects`, `phase:cowork`,
  `phase:attachments` — the run stopped there (the rate limit, or 25
  failures in a row); the rest is left for the next run.
- `phase:account` — the account could not be read.
- `config:conv_uuids:<value>`, `config:project_uuids:<value>`,
  `config:cowork_session_ids:<value>` — a configured id claude.ai does
  not have.
- `phase:export`, `phase:users.json`, `phase:projects`,
  `phase:conversations.json` — with the `export` method: entries with
  no `uuid`, which were not stored.

**2. What is still owed.** The download keeps a `held_version` in each
sidecar: a conversation, project or Cowork task is held at the version
the listing named, a project's documents at the project's `updated_at`,
a file at its owner's version. What claude.ai lists now is not in the
store, so a conversation changed upstream since the last run cannot be
seen from here. What the store does show:

```sql
SELECT 'conversations never fetched' AS what, count(*) AS n
  FROM conversations WHERE payload IS NULL
UNION ALL
SELECT 'conversations whose last fetch failed', count(*)
  FROM conversations_bookkeeping WHERE last_error IS NOT NULL
UNION ALL
SELECT 'Cowork tasks never fetched', count(*)
  FROM cowork_sessions WHERE payload IS NULL
UNION ALL
SELECT 'files not held', count(*)
  FROM claude_attachments a
  LEFT JOIN claude_attachments_bookkeeping b ON b.id = a.id
  LEFT JOIN conversations_bookkeeping c ON c.id = a.conversation_uuid
  LEFT JOIN cowork_sessions_bookkeeping s ON s.id = a.conversation_uuid
 WHERE b.fetched_at_utc IS NULL
    OR b.held_version IS NOT coalesce(c.held_version, s.held_version)
UNION ALL
SELECT 'projects whose documents are not listed at its version', count(*)
  FROM projects p
  LEFT JOIN project_docs_listings_bookkeeping b ON b.id = p.id
 WHERE b.fetched_at_utc IS NULL OR b.held_version IS NOT p.updated_at;
```

"Files not held" counts files still to download; a file claude.ai
answered 404 for is held (with its `not_found` problem) and is not
counted. A store filled by the `export` method has no `claude_attachments`
rows at all, so its files are not counted, though none were downloaded.

**3. How the last run ended** (the `api` method only; the `export`
method writes no `sync_runs` row):

```sql
SELECT run_id, started_at_utc, finished_at_utc, status,
       json_extract(summary, '$.fetched') AS fetched,
       json_extract(summary, '$.errors') AS errors,
       json_extract(summary, '$.failed_blobs') AS failed_files
  FROM sync_runs ORDER BY run_id DESC LIMIT 1;
```

`partial` means the run stopped early (asked to, or at the rate limit)
and the next run carries on. A run that stopped at
`max_conversations_per_run` still says `ok`: when that is set and
`fetched + errors` equals it, more may be owed.

### `users`

The account the store mirrors: one row, read once from `/account` (or
the export's `users.json`, which may list more).

| column | what it holds |
|---|---|
| `id` | the account UUID |
| `payload` | `uuid`, `email_address` and `full_name`, from `/account` or `users.json` |
| `email` | the account's email address |
| `full_name` | the account's name |

### `orgs`

One row per organization the account belongs to, as `/organizations`
listed it (the API method only).

| column | what it holds |
|---|---|
| `id` | the organization UUID |
| `payload` | the organization's entry in `/organizations`, its `capabilities` sorted |
| `name` | its name |

### `conversations`

One row per conversation.

| column | what it holds |
|---|---|
| `id` | the conversation UUID |
| `payload` | the whole conversation (above); NULL for a conversation whose every fetch failed |
| `org_uuid` | the organization it was fetched from; NULL for one from an export, and for a failed one |
| `org_name` | that organization's name |
| `name` | the conversation's title |
| `updated_at` | its `updated_at`, as claude.ai wrote it |

### `claude_attachments`

One row per downloadable file a conversation or Cowork task names: the
link from its owner to the bytes in `blobs.sqlite`. Empty for a store
filled by the `export` method.

| column | what it holds |
|---|---|
| `id` | `<owner id>#<file UUID>` |
| `conversation_uuid` | the owner: a `conversations.id`, or a `cowork_sessions.id` |
| `file_uuid` | the file's UUID, as the message names it |
| `blake3` | the hash of its bytes in `blobs.sqlite`; NULL until they land, and for a file claude.ai no longer has |

### `projects`

One row per Claude Project: its entry in the project listing, which is
the whole of its metadata.

| column | what it holds |
|---|---|
| `id` | the project UUID |
| `payload` | the listing entry (name, description, custom instructions as `prompt_template`, creator, …), its `permissions` sorted |
| `org_uuid` | the organization it is in |
| `org_name` | that organization's name |
| `name` | the project's name |
| `updated_at` | its `updated_at`, as claude.ai wrote it |

### `project_docs`

One row per knowledge document of a project, with its full text.

| column | what it holds |
|---|---|
| `id` | the document UUID |
| `payload` | the document's entry, its text in `content` |
| `project_uuid` | the project it belongs to |
| `file_name` | its file name |
| `created_at` | when it was added, as claude.ai wrote it |

### `project_docs_listings`

One row per project whose knowledge documents have been listed. The
row is only the id; what it was listed at is
`project_docs_listings_bookkeeping.held_version`, the project's
`updated_at` then.

| column | what it holds |
|---|---|
| `id` | the project UUID |

### `cowork_sessions`

One row per Cowork task run in the cloud that claude.ai keeps as a task
rather than a chat. A task claude.ai has turned into a chat is a
`conversations` row instead.

| column | what it holds |
|---|---|
| `id` | the task id, `cse_…` |
| `payload` | the task's entry in the sessions listing, its `tags` sorted |
| `org_uuid` | the organization it is in |
| `org_name` | that organization's name |
| `title` | the task's title |
| `last_event_at` | when its log last grew |

### `cowork_events`

One row per event of a Cowork task's log, read whole each time the task
changes.

| column | what it holds |
|---|---|
| `id` | the event's `event_id`, or `<task id>#<sequence_num>` for an event with none |
| `payload` | the event as the log sent it; what was said is under `payload`, with its `type` (`user`, `assistant`, `system`, `result`, …) |
| `session_id` | the task it belongs to, a `cowork_sessions.id` |
| `sequence_num` | its place in the log |

**Set aside from the payload:** nothing; `volatile_payload` is empty in
every sidecar. Instead, keys whose value is `null` are left out of an
API conversation's payload, because claude.ai sends them both ways for
an unchanged conversation.

## Claude Code (`claude_code`)

The store holds the session files Claude Code keeps on this machine.
`records.payload` is one line of a file, as Claude Code wrote it: the
`user`, `assistant` and `system` lines, each with its `uuid`,
`parentUuid`, `sessionId`, `timestamp`, `cwd`, `gitBranch` and the
`version` of Claude Code that wrote it. An `assistant` line's
`message.content` is the Messages API's list of blocks (`text`,
`thinking`, `tool_use`); a `user` line's is either what the person typed
(a string, or `text` and `image` blocks) or a `tool_result` block, with
the structured result beside it as `toolUseResult`. The other kinds of
line are not stored; `transcripts.payload` sums them up.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a session id (the file name `<session id>.jsonl`) | `transcripts.id` and `transcripts.session_id`; `records.transcript_id` of its lines |
| a subagent's agent id (`subagents/agent-<id>.jsonl`) | `transcripts.agent_id`; the subagent's `transcripts.id` is `<session id>#<agent id>` |
| a line's `uuid` | `records.id`; `parentUuid` in another line's payload |
| a tool call id `toolu_…` | `id` of a `tool_use` block, and `tool_use_id` of the `tool_result` block that answers it |
| a claude.ai cloud session id `cse_…` | `$.cloud_session_id` in `transcripts.payload` |
| the file on disk | `transcripts.rel_path`, under the folder the source reads |

**Me.** A `user` line is the person's when its content is text and
Claude Code did not write it: see the query below. There is no account
row; the account a session was bridged to claude.ai under is
`$.account_uuid` in `transcripts.payload`.

**What the person typed in one session** (close to what the page calls
`User Input`; the page also moves `<system-reminder>` spans and a few
tagged notices to `Harness Message`):

```sql
SELECT payload ->> '$.timestamp' AS at, payload -> '$.message.content' AS said
  FROM records
 WHERE transcript_id = :session
   AND payload ->> '$.type' = 'user'
   AND coalesce(payload ->> '$.isMeta', 0) = 0
   AND coalesce(payload ->> '$.isCompactSummary', 0) = 0
   AND coalesce(payload ->> '$.isSidechain', 0) = 0
   AND coalesce(payload ->> '$.origin.kind', 'human') = 'human'
   AND CASE json_type(payload, '$.message.content')
         WHEN 'text' THEN 1
         WHEN 'array' THEN NOT EXISTS (
           SELECT 1 FROM json_each(payload, '$.message.content') b
            WHERE b.value ->> '$.type' = 'tool_result')
         ELSE 0 END
 ORDER BY at;
```

**A session's subagents:**

```sql
SELECT id, agent_id, title, started_at, updated_at
  FROM transcripts WHERE session_id = :session AND agent_id IS NOT NULL;
```

**Sessions by working directory:**

```sql
SELECT cwd, count(*) AS sessions, max(updated_at) AS last
  FROM transcripts WHERE agent_id IS NULL GROUP BY cwd ORDER BY last DESC;
```

This store has no `<table>_bookkeeping` sidecars, and writes no
`sync_runs` rows. `ingested_files` (§ "What every raw store has") holds
each file under the scope `claude_code/sessions`, its `rel_path` equal
to `transcripts.rel_path`.

### Is the copy complete?

How to tell whether the copy is complete, from the store alone. This
source keeps no record of what is owed: every run reads every file that
changed, and nothing is fetched over the network.

**1. What the last run could not read:**

```sql
SELECT severity, scope_key, sample FROM problems ORDER BY scope_key;
```

- `file:claude_code/sessions:<path>` — lines inside that file were
  skipped (not JSON, no `type`, or a content line with no `uuid`); the
  rest of the file is in the copy. It stays until the file changes.
- `record:claude_code/sessions:<path>` and `record:transcripts:<path>`
  — a file that could not be opened or read; nothing of its latest
  content is in the copy. Every sync tries it again.
- `listing:claude_code/sessions` — the folder could not be walked in
  full (a folder it may not list, a dangling link), or is gone; what
  was read before is kept.

**2. Whether a file on disk has lines the copy lacks.** There is no
`sync_runs` row. `ingested_files` holds each file's size and hash as it
was when last read; a file on disk of another size has changed since:

```sql
SELECT t.id, t.rel_path, i.size_bytes, i.last_finished_at_utc
  FROM transcripts t
  JOIN ingested_files i ON i.scope = 'claude_code/sessions' AND i.rel_path = t.rel_path
 ORDER BY i.last_finished_at_utc DESC;
```

Compare `size_bytes` with the file's size under the source's folder
(`~/.claude/projects` unless `path` says otherwise). A transcript with
no `ingested_files` row is one whose file is gone from disk; its rows
are kept. `max(last_finished_at_utc)` is when a changed file was last
read, not when the last sync ran: an unchanged file's stamp does not
move.

### `transcripts`

One row per session file: a session, or one of its subagents. The row
says what the file says about itself as a whole.

| column | what it holds |
|---|---|
| `id` | the session id, or `<session id>#<agent id>` for a subagent |
| `payload` | the file's summary: `title` and where it came from (`title_source`: `custom`, `ai`, `agent` or `prompt`), `first_prompt`, `cwd`, `git_branch` and every `git_branches`, `entrypoints`, the Claude Code `versions` and `models` seen, `cloud_session_id`, `org_uuid` and `account_uuid` of a session bridged to claude.ai, `pr_links`, and `record_counts`, a count of every kind of line |
| `session_id` | the session id; for a subagent, the session it ran in |
| `agent_id` | the subagent's id; NULL for a session |
| `cwd` | the working directory of the file's first line that names one |
| `git_branch` | the git branch of the file's first line that names one |
| `title` | the person's name for it, else Claude Code's, else the subagent's name, else the first line of the first prompt |
| `started_at` | the earliest line's `timestamp` (RFC 3339, UTC) |
| `updated_at` | the latest line's `timestamp` |
| `rel_path` | the file's path under the folder the source reads |

### `records`

One row per `user`, `assistant` or `system` line of a session file.

| column | what it holds |
|---|---|
| `id` | the line's `uuid` |
| `payload` | the line as Claude Code wrote it; `payload ->> '$.type'` is its kind |
| `transcript_id` | the `transcripts.id` of the file it is in |

## Codex (`codex`)

The store holds the rollout files Codex keeps on this machine.
`records.payload` is one line of a rollout, as Codex wrote it:
`{timestamp, type, payload}`, where `type` is `session_meta` (once,
first), `turn_context` (once per turn: the model, the working
directory, the policies), `response_item` (what the model saw and said:
`payload.type` is `message`, `reasoning`, `function_call`,
`custom_tool_call`, `function_call_output`, …), `event_msg` (what the
UI was told), `compacted`, and from 0.155 `world_state` and
`token_usage_record`. Every line is kept.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a thread id (the end of `rollout-<time>-<thread id>.jsonl`) | `transcripts.id`, `records.transcript_id`; `payload.id` of the `session_meta` line |
| a sub-agent's parent thread | `transcripts.parent_thread_id` |
| the thread a fork was made from | `$.forked_from_id` in `transcripts.payload` |
| a line of a rollout | `records.id`, `<thread id>#<line number>`, and `records.line_no` |
| a tool call id | `payload.call_id` of a `function_call` or `custom_tool_call` line, and of the output line that answers it |
| a turn | `payload.turn_id` of a `turn_context` line |
| the file on disk | `transcripts.rel_path`, under the Codex home (`sessions/…` or `archived_sessions/…`) |

**Me.** What the person typed is, from Codex 0.155, a `message` whose
parts are tagged `user.text`; before that, an `event_msg` of type
`user_message`. There is no account: a rollout names none.

**What the person typed in one thread, Codex 0.155 and later:**

```sql
SELECT r.line_no, r.payload ->> '$.timestamp' AS at,
       r.payload -> '$.payload.content' AS said
  FROM records r
 WHERE r.transcript_id = :thread
   AND r.payload ->> '$.type' = 'response_item'
   AND r.payload ->> '$.payload.type' = 'message'
   AND EXISTS (SELECT 1 FROM json_each(r.payload,
                 '$.payload.internal_chat_message_metadata_passthrough.content_item_kinds') k
                WHERE k.value = 'user.text')
 ORDER BY r.line_no;
```

**The same, through Codex 0.115:**

```sql
SELECT r.line_no, r.payload ->> '$.timestamp' AS at,
       r.payload ->> '$.payload.message' AS said
  FROM records r
 WHERE r.transcript_id = :thread
   AND r.payload ->> '$.type' = 'event_msg'
   AND r.payload ->> '$.payload.type' = 'user_message'
 ORDER BY r.line_no;
```

**A thread's sub-agent threads:**

```sql
SELECT id, title, started_at, updated_at FROM transcripts WHERE parent_thread_id = :thread;
```

This store has no `<table>_bookkeeping` sidecars, and writes no
`sync_runs` rows. `ingested_files` (§ "What every raw store has") holds
each rollout under the scope `codex/sessions` or
`codex/archived_sessions`, with `rel_path` relative to that folder:
`transcripts.rel_path` without its first part.

### Is the copy complete?

How to tell whether the copy is complete, from the store alone. This
source keeps no record of what is owed: every run reads every file that
changed, and nothing is fetched over the network.

**1. What the last run could not read:**

```sql
SELECT severity, scope_key, sample FROM problems ORDER BY scope_key;
```

- `file:codex/sessions:<path>` (or `file:codex/archived_sessions:…`) —
  lines inside that rollout were skipped (not JSON, or no `type`); the
  rest of it is in the copy. It stays until the file changes.
- `record:codex/sessions:<path>` and `record:transcripts:sessions/<path>`
  — a rollout that could not be opened or read; nothing of its latest
  content is in the copy. Every sync tries it again.
- `listing:codex/sessions` — the folder could not be walked in full, or
  is gone; what was read before is kept. A missing `archived_sessions/`
  is no problem.

**2. Whether a rollout on disk has lines the copy lacks.** There is no
`sync_runs` row. `ingested_files` holds each file's size and hash as it
was when last read; a file on disk of another size has changed since:

```sql
SELECT t.id, t.rel_path, i.size_bytes, i.last_finished_at_utc
  FROM transcripts t
  JOIN ingested_files i
    ON i.scope = 'codex/' || substr(t.rel_path, 1, instr(t.rel_path, '/') - 1)
   AND i.rel_path = substr(t.rel_path, instr(t.rel_path, '/') + 1)
 ORDER BY i.last_finished_at_utc DESC;
```

Compare `size_bytes` with the file's size under the Codex home
(`~/.codex` unless `path` says otherwise). A thread with no
`ingested_files` row is one whose file is gone from disk; its rows are
kept. `max(last_finished_at_utc)` is when a changed file was last read,
not when the last sync ran.

### `transcripts`

One row per rollout file, which is one thread. The row says what the
file says about itself as a whole.

| column | what it holds |
|---|---|
| `id` | the thread id Codex minted |
| `payload` | the file's summary: the `session_meta` fields (`cwd`, `originator`, `cli_version`, `source`, `model_provider`, `forked_from_id`, `agent_nickname`, `agent_role`, `git_commit`, `git_origin_url`), every `models` a turn ran on, `title` and `title_source` (`agent` or `prompt`), `first_prompt`, the number of `turns`, and counts of lines by `type` (`record_counts`) and of `response_item` lines by their type (`item_counts`) |
| `parent_thread_id` | the thread that spawned this one, for a sub-agent thread; NULL otherwise |
| `cwd` | the working directory `session_meta` names, else the first turn's |
| `git_branch` | the git branch `session_meta` names |
| `title` | a sub-agent's nickname, else the first line of the first prompt the person typed, at most 100 characters |
| `started_at` | the earliest of the lines' `timestamp`s and the session's own start (RFC 3339, UTC) |
| `updated_at` | the latest line's `timestamp` |
| `rel_path` | the file's path under the Codex home |

### `records`

One row per line of a rollout, every kind of line.

| column | what it holds |
|---|---|
| `id` | `<thread id>#<line number>` |
| `payload` | the line as Codex wrote it; `payload ->> '$.type'` is its kind, `payload ->> '$.payload.type'` an item's |
| `transcript_id` | the thread it is in, a `transcripts.id` |
| `line_no` | its line number in the file, counting from 1; the order to read a thread in |

## Discord (`discord`)

Discord's objects are kept as its API returned them, less what changes
from one answer to the next (below): `payload` is the user, server,
member, role, channel or message JSON from `/users/@me`,
`/users/@me/guilds`, `/users/@me/guilds/{id}/member`,
`/guilds/{id}/roles`, `/guilds/{id}/channels`, `/users/@me/channels`,
`/channels/{id}/threads/search` and `/channels/{id}/messages`. Discord's
API calls a server a *guild*; the tables keep that word.

Every id is a Discord *snowflake*: a 64-bit number, unique across the
service, whose top 42 bits are the milliseconds since 2015-01-01 UTC. So
an id says when its record was made, and ids sort as times do. They are
17 to 19 digits long, so a message's key pads its id to 20 digits to
keep a conversation's messages in the order they were sent.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| the account's user id | `account.id`, and a `users` row |
| a user id | `users.id`, a message's `$.author.id` |
| a server (guild) id | `guilds.id`, `members.id`, `roles.guild_id`, `channels.guild_id` |
| a role id | `roles.id` |
| a channel, thread, forum post or DM id | `channels.id`, `messages.channel_id` |
| a message id | `$.id` in `messages.payload`; the row's `id` is `<channel id>#<message id padded to 20 digits>` |
| a file (attachment) id | `discord_attachments.attachment_id` |

**Me** is `account.id`. **A message's time** is its payload's
`timestamp`, or from its id:

```sql
SELECT id,
       json_extract(payload, '$.timestamp') AS sent,
       datetime(((CAST(json_extract(payload, '$.id') AS INTEGER) >> 22)
                 + 1420070400000) / 1000, 'unixepoch') AS from_id
  FROM messages LIMIT 5;
```

**Who wrote a message.** The payload keeps only the author's `id` and
`bot`; the rest of the profile is in `users`. A message sent through a
webhook keeps its `author` whole: its name is the message's own.

```sql
SELECT m.id,
       coalesce(json_extract(u.payload, '$.global_name'),
                json_extract(u.payload, '$.username'),
                json_extract(m.payload, '$.author.username')) AS author,
       json_extract(m.payload, '$.author.id') = (SELECT id FROM account) AS is_me,
       json_extract(m.payload, '$.content') AS text
  FROM messages m
  LEFT JOIN users u ON u.id = json_extract(m.payload, '$.author.id')
 WHERE m.channel_id = :channel
 ORDER BY m.id;
```

**A reply and what it answers.** `$.message_reference` names the
message answered (or forwarded) by its channel and id; the copy of it
Discord embeds, `referenced_message`, is set aside (below).

```sql
SELECT m.id, r.id AS answers
  FROM messages m
  JOIN messages r
    ON r.id = json_extract(m.payload, '$.message_reference.channel_id') || '#'
              || printf('%020d', CAST(json_extract(m.payload, '$.message_reference.message_id') AS INTEGER))
 WHERE m.channel_id = :channel;
```

**Threads and DMs.** A thread or a forum post is a `channels` row with
`kind` 10, 11 or 12 and `parent_id` naming its channel or forum; its
messages carry the thread's own id as `channel_id`. A DM is `kind` 1, a
group DM 3, with no `guild_id`; its people are its payload's
`recipients`, by id. `channels` holds every channel of a copied server,
including categories (`kind` 4) and the channels the account may not
read, which have no messages.

**Set aside from the payload** (in `volatile_payload` on the table's
`_bookkeeping` row): a user's profile beyond `id`, `username`,
`global_name`, `discriminator`, `avatar`, `bot`, `system`; the
account's likewise (it keeps `email` and `locale`; its phone number is
never stored); a server's approximate counts; the account's member
record's `user`, `bio` and decorations; a channel's `last_message_id`,
`last_pin_timestamp`, counts, `position`, `owner`, whether a thread is
archived now, and the profiles of a DM's people; a message's
`referenced_message`, `thread`, `resolved`, the profiles of its author
and of who it mentions, its files' links (`url`, `proxy_url`, which
Discord signs anew each day) and what Discord works out of a file or a
preview later. A server's `features`, a member's `roles`, a message's
`mentions` and `mention_roles`, and a channel's `permission_overwrites`
and `recipients` are sorted.

### Is the copy complete?

Run each against `<data_root>/<id>/ingest/entities.doltlite_db` with
`datalib-doltlite -readonly`. The download keeps no `held_version` for
messages; what is owed is the part of each conversation from `since` up
to its newest message that `coverage` does not hold.

1. **The last sync's outcome.** `ok` means it read everything it set
   out to; `partial` that it stopped early; `error` that it failed (a
   refused sign-in ends the run).

   ```sql
   SELECT run_id, started_at_utc, status, summary FROM sync_runs
    ORDER BY run_id DESC LIMIT 1;
   ```

2. **What failed.** Keys that mean part of the copy is missing:
   `listing:messages of <channel id>` (that conversation was not read
   through), `listing:threads of <channel id>` (its threads were not
   listed), `listing:channels of server <id>` and `listing:roles of
   server <id>` (that server's channels were not read this sync),
   `listing:direct messages`, `listing:servers`,
   `discord_attachments:<id>` with reason `fetch_failed` (a file's bytes
   are missing) or `over_size_limit` (skipped on purpose), and
   `config:servers:<entry>`, `config:channels:<entry>`,
   `config:dm_conversations:<entry>` (a name in the config matches
   nothing). A `listing:` row at `warning` is one Discord refused (403)
   or no longer has (404): asking again gets the same answer.

   ```sql
   SELECT severity, reason, scope_key, sample FROM problems
    WHERE severity IN ('error', 'warning')
    ORDER BY severity, scope_key;
   ```

3. **Conversations whose newest messages are not read.** Each channel's
   newest message, as last listed, is `last_message_id` in its sidecar;
   a walked conversation is held up to the top of its `history:`
   coverage. This lists the walked conversations that are behind. A
   conversation never walked at all has no coverage row; among the
   copied server's channels, that is usually one the account may not
   read.

   ```sql
   SELECT c.id, json_extract(c.payload, '$.name') AS name,
          json_extract(b.volatile_payload, '$.last_message_id') AS newest,
          (SELECT max(hi) FROM coverage v WHERE v.scope = 'history:' || c.id) AS held_to
     FROM channels c JOIN channels_bookkeeping b ON b.id = c.id
    WHERE json_extract(b.volatile_payload, '$.last_message_id') IS NOT NULL
      AND EXISTS (SELECT 1 FROM coverage v WHERE v.scope = 'history:' || c.id)
      AND (SELECT max(hi) FROM coverage v WHERE v.scope = 'history:' || c.id)
          < printf('%020d', CAST(json_extract(b.volatile_payload, '$.last_message_id') AS INTEGER));
   ```

   A thread's `last_message_id` is as fresh as its last listing (at
   most `list_every_hours` old), so a thread can be behind with this
   query returning nothing.

4. **Files without bytes.** With `media` on, a row with no `blake3` is
   owed, unless a `problems` row says it is over the size limit.

   ```sql
   SELECT count(*) AS files_owed FROM discord_attachments WHERE blake3 IS NULL;
   ```

### `account`

The signed-in account, one row.

| column | what it holds |
|---|---|
| `id` | the account's user id |
| `payload` | Discord's `/users/@me` answer, without its phone number and how it signs in |

### `users`

One row per person a stored message or DM names: its author, who it
mentions, who ran the command it answers, a DM's people.

| column | what it holds |
|---|---|
| `id` | the user id |
| `payload` | who they are: `id`, `username`, `global_name`, `discriminator`, `avatar`, `bot`, `system` |

### `guilds`

One row per copied server.

| column | what it holds |
|---|---|
| `id` | the server id |
| `payload` | the server as `/users/@me/guilds` lists it |

### `members`

The account's own membership of each copied server: its roles.

| column | what it holds |
|---|---|
| `id` | the server id |
| `payload` | Discord's member object for the account, roles sorted |

### `roles`

One row per role of a copied server; a role the server no longer has
is deleted.

| column | what it holds |
|---|---|
| `id` | the role id |
| `payload` | Discord's role object |
| `guild_id` | the server it belongs to |

### `channels`

One row per channel, category, thread, forum post or DM.

| column | what it holds |
|---|---|
| `id` | the channel id |
| `payload` | Discord's channel object |
| `guild_id` | the server it is in; NULL for a DM |
| `parent_id` | a thread's channel, or a channel's category |
| `kind` | Discord's channel type: 0 text, 1 DM, 2 voice, 3 group DM, 4 category, 5 announcement, 10 to 12 thread, 13 stage, 15 forum, 16 media |

### `messages`

One row per message.

| column | what it holds |
|---|---|
| `id` | `<channel id>#<message id padded to 20 digits>` |
| `payload` | Discord's message object |
| `channel_id` | the conversation it is in: a channel, a thread or a DM |

### `discord_attachments`

Which files each message carries, a forwarded message's among them, and
where their bytes are in `blobs.sqlite`.

| column | what it holds |
|---|---|
| `id` | `<message key>#<attachment id>` |
| `message_key` | the `messages.id` |
| `attachment_id` | Discord's attachment id |
| `blake3` | the `cas_objects.blake3` holding the bytes; NULL until they are stored, or when `media` is off or the file is over the size limit |

### `coverage`

The stretches the download has read. Scope `history:<channel id>` is a
stretch of one conversation's messages: everything Discord listed there
is stored. Scope `threads:<channel id>` is a stretch of one channel's
thread listing, by each thread's newest message. Both ends are
included, and are message ids padded to 20 digits, so they sort as
times do. A conversation's history reaches from `since` up to the
newest message its listing named.

| column | what it holds |
|---|---|
| `scope` | `history:<channel id>` or `threads:<channel id>` |
| `lo` | the oldest message id in the stretch, padded to 20 digits |
| `hi` | the newest message id in the stretch, padded to 20 digits |

## Facebook (`facebook`)

The store mirrors Facebook's "Download your information" export in its
JSON format, read from the folder `export.path` names. Its tables are
not fixed: **every JSON file of the export becomes a table of its own**,
created the first time a sync meets that file, so `current.json` lists
no record tables for this source. Use `.tables` in the shell to see
which an export produced. Of the tables listed in `current.json`, only
`media_blobs` (below) is this source's own; the record tables have no
`_bookkeeping` sidecar.

A table's name is the file's path relative to the export folder,
lowercased, with the `.json` dropped, every run of other characters
turned into `_`, and a trailing `_<digits>` removed, since Facebook
splits a long file into numbered parts that all land in one table
(`your_facebook_activity/posts/album/0.json` and `…/1.json` are both
`your_facebook_activity_posts_album`). A name that would start with a
digit gets a `t_` prefix. Each table has two columns:

| column | what it holds |
|---|---|
| `id` | the record's `fbid` where it has one; otherwise a uuidv5 of the table name and the record's JSON |
| `payload` | the record as the export wrote it (JSONB), with Facebook's mis-encoded non-ASCII text repaired |

One record is one element of the file's top-level array, or of the
single array an object wraps (`{"comments_v2": […]}`); a file that is
one object (an album, the profile) is one row. Where one `fbid` names
two records that differ, both are kept, each keyed by its content.

The tables render reads:

| table | what one row is |
|---|---|
| `your_facebook_activity_posts_your_posts_check_ins_photos_and_videos` | one of your timeline posts |
| `your_facebook_activity_posts_posts_on_other_pages_and_profiles` | a post you made on another page or profile |
| `your_facebook_activity_groups_group_posts_and_comments` | a post you made in a group |
| `your_facebook_activity_groups_your_group_membership_activity` | a group you joined |
| `your_facebook_activity_posts_album` | one photo album |
| `your_facebook_activity_comments_and_reactions_comments` | one comment you made |
| `your_facebook_activity_groups_your_comments_in_groups` | one comment you made in a group |
| `your_facebook_activity_comments_and_reactions_likes_and_reactions` | one reaction (from both of Facebook's reaction files) |
| `your_facebook_activity_posts_edits_you_made_to_posts` | one saved version of a post |
| `your_facebook_activity_comments_and_reactions_your_comment_edits` | one saved version of a comment |
| `connections_friends_your_friends` | one friend: a name and the time the friendship began |
| `personal_information_profile_information_profile_information` | the profile, one row |
| `messenger_threads` | one Messenger conversation |
| `messenger_messages` | one Messenger message |

**Messenger is the exception to one table per file.** Every
conversation file (`messages/<folder>/<dir>/message_<n>.json`) lands in
the two Messenger tables. A `messenger_threads` row has `id` = the
conversation id and `payload` = `{thread_id, folder, thread}`, where
`folder` is where Facebook filed it (`inbox`, `filtered_threads`,
`message_requests`, `e2ee_cutover`, …) and `thread` is the file without
its messages (`participants`, `title`, `is_still_participant`, …). A
`messenger_messages` row has `id` =
`<thread id>:<timestamp_ms, 13 digits>:<n>` and `payload` =
`{thread_id, message}`, where `message` is Facebook's own object
(`sender_name`, `timestamp_ms`, `content`, `photos`, `share`,
`reactions`, `is_unsent`, …) and `n` counts messages sent in the same
millisecond, from the oldest.

**Which id is which**

| upstream id | where it is |
|---|---|
| a record's `fbid` (reactions, posts on other pages, edits) | `<table>.id` |
| a Messenger conversation id (the digits ending its folder name, `williamriker_1000000001` → `1000000001`) | `messenger_threads.id`, `json_extract(payload, '$.thread_id')` in both Messenger tables, and the first part of `messenger_messages.id` |
| an export-relative media path (`uri`) | `media_blobs.uri` |
| a record that references a media file | `media_blobs.owner_id` = that record's `id` |

Nothing in the export gives a person an id: a person is a name. **"Me"**
is the profile's `profile_v2.name.full_name`; your messages are the ones
whose `sender_name` is that name.

Your messages in one conversation, oldest first:

```sql
SELECT m.id,
       json_extract(m.payload, '$.message.timestamp_ms') AS sent_ms,
       json_extract(m.payload, '$.message.content') AS content
  FROM messenger_messages m
 WHERE json_extract(m.payload, '$.thread_id') = '1000000001'
   AND json_extract(m.payload, '$.message.sender_name') = (
         SELECT json_extract(payload, '$.profile_v2.name.full_name')
           FROM personal_information_profile_information_profile_information)
 ORDER BY m.id
```

Conversations with their folder and how many messages each holds:

```sql
SELECT t.id, json_extract(t.payload, '$.folder') AS folder,
       json_extract(t.payload, '$.thread.title') AS title,
       count(m.id) AS messages
  FROM messenger_threads t
  LEFT JOIN messenger_messages m ON json_extract(m.payload, '$.thread_id') = t.id
 GROUP BY t.id
 ORDER BY messages DESC
```

Every table the export produced and how many rows each has is
`.tables` followed by `SELECT count(*) FROM <table>`; a table with
rows that render does not read is named in the render step's
`unrendered_tables:all` info row.

The media bytes are in the store's sibling `blobs.sqlite`, addressed by
`media_blobs.blake3`.

### Is the copy complete?

This source keeps no record of anything owed: each sync reads the whole
export, so the copy is complete for that export unless a sync recorded a
problem. It does not write `sync_runs` (the table is there and stays
empty); the step's status on the Manage screen is the record of the
last run.

1. What the last read of the export could not do. A `listing:` row means
   part of the export was not read and nothing in the affected tables
   was deleted that run; a `media_blobs:` row is one photo or video not
   stored.

   ```sql
   SELECT severity, reason, scope_key, sample FROM problems
    WHERE scope_key LIKE 'listing:%' OR scope_key LIKE 'media_blobs:%'
    ORDER BY severity, scope_key
   ```

   - `listing:file <path>`: that file would not read or parse, or it is
     a numbered part of a table whose other parts are present but it is
     not (an export unzipped only in part). Its table kept last sync's
     rows.
   - `listing:files`: a folder under the export could not be listed, or
     a symbolic link was found (links are not followed). No table was
     pruned that run.
   - `media_blobs:<record id>#<uri>`, `not_found`, warning: the export
     names a media file it does not contain. An `error` instead means
     the file is there and would not read.

2. Media named and not stored, counted:

   ```sql
   SELECT count(*) AS media_missing FROM media_blobs WHERE blake3 IS NULL
   ```

3. What render could not show, in the render step's own store
   (`<data_root>/<source id>/render_markdown/indexed_markdown.doltlite_db`):

   ```sql
   SELECT severity, field, sample, count(*) FROM problems
    GROUP BY 1, 2, 3 ORDER BY 4 DESC
   ```

   A warning there names a field Facebook added that render does not
   read, or a message the export withholds; the value, where the export
   has one, is still in the raw store.

### `media_blobs`

One row per pair of a record and a media file it names by `uri`; a file
that two records name (an album and the post that added its photos) is
two rows and stored once.

| column | what it holds |
|---|---|
| `id` | `<owner_id>#<uri>` |
| `owner_id` | the `id` of the record that names the file, in whichever table it is |
| `uri` | the file's path relative to the export folder, as the JSON spells it (`your_facebook_activity/posts/media/…`) |
| `blake3` | the hash of the file's bytes in `blobs.sqlite`; NULL when the export did not include the file or it would not read |

## File inventory (`file_inventory`)

`file_inventory` keeps no upstream payload: there is no `payload` column on any table. Each row is what the scan learned from the filesystem for one entry under the scan root (`fswalk.path`): its path, kind, size and the blake3 hash of its contents. The store has no `blobs.sqlite`; it hashes bytes and never stores them.

Every scan walks the whole tree, rewrites a row for each entry it finds and deletes the rows of entries it did not find. Rewriting an unchanged row is no change to the store, so each commit is exactly what changed on disk since the last scan, and a scan of an unchanged tree makes no commit. `files`, `dirs` and `scan_meta` hold the tree as of the last scan; earlier states are earlier commits.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a file or symbolic link's path under the scan root | `files.id` |
| a folder's path under the scan root | `dirs.id`; the root itself is `''` |
| the scan root's absolute path | `scan_meta.abs_path` |
| the source's id (its group) | `scan_meta.id` |
| a folder's breadcrumb id from its `.file_inventory.toml` | `dirs.identity_uuid` |
| a file's blake3 hash, as lowercase hex | `lower(hex(files.blake3))` |

**Files with the same contents** (the same bytes under two or more paths):

```sql
SELECT lower(hex(blake3)) AS blake3, size, count(*) AS copies,
       group_concat(id, ' | ') AS paths
  FROM files
 WHERE kind = 'file'
 GROUP BY blake3
HAVING count(*) > 1
 ORDER BY size * (count(*) - 1) DESC
 LIMIT 20;
```

**The largest folders**, by the bytes of everything below them:

```sql
SELECT id, size, entries FROM dirs ORDER BY size DESC LIMIT 20;
```

**Folders that are exact copies of each other** (the same tree, by contents and names):

```sql
SELECT lower(hex(blake3)) AS tree_hash, size, entries, group_concat(id, ' | ') AS folders
  FROM dirs
 WHERE entries > 0
 GROUP BY blake3
HAVING count(*) > 1
 ORDER BY size DESC;
```

**Folders that claim the same breadcrumb id**, which happens when a stamped folder was copied:

```sql
SELECT identity_uuid, count(*) AS n, group_concat(id, ' | ') AS folders
  FROM dirs
 WHERE identity_uuid IS NOT NULL
 GROUP BY identity_uuid
HAVING count(*) > 1;
```

**What the last scan changed**, folders first. Each folder row names a folder that anything changed under:

```sql
SELECT diff_type, from_id, to_id, to_entries, to_size
  FROM dolt_diff_dirs('HEAD^1', 'HEAD');
```

### Is the copy complete?

`file_inventory` keeps no record of anything owed: every scan walks the whole tree, so the copy is complete as of the last scan except for the entries that scan could not read. Each of those is one row in `problems`, keyed `record:files:<path>` or `record:dirs:<path>`, and the row is replaced by the next scan's verdict (it goes once the entry reads):

```sql
SELECT scope_key, sample, changed_at_utc
  FROM problems
 WHERE scope_key LIKE 'record:%'
 ORDER BY scope_key;
```

The `sample` says what failed: `readdir: …` (a folder that would not list: its old rows, and every row under it, are kept from the last scan that read it), `stat: …`, `hash: …` or `read_link: …` (an entry whose old row is kept), `the name is not UTF-8, …` (an entry not recorded at all), `… did not parse, so it was not applied` (a `.file_inventory.toml` whose `ignore` rules were not used, so entries it meant to hide may be in the copy), or `could not stamp: …`.

`sync_runs` stays empty for this source, so it cannot say when the last scan ran or whether it finished. A scan commits once, at its end, and only when something changed, so the newest commit (`SELECT date, message FROM dolt_log() LIMIT 1`) is the last scan that changed something; its message starts `download <source id>: entries=… errors=…`. A scan that was killed leaves no commit and no `problems` rows.

### `files`

One row per file or symbolic link under the scan root that no `ignore` pattern hides.

| column | what it holds |
|---|---|
| `id` | its path relative to the scan root, `/`-separated |
| `kind` | `file` or `symlink` |
| `size` | a file's size in bytes; for a symbolic link, the length in bytes of its target text |
| `blake3` | the blake3 hash of a file's contents, or of a link's target text, as 32 raw bytes |
| `symlink_target` | a symbolic link's target, as the link spells it (not resolved); NULL for a file |

### `dirs`

One row per folder under the scan root, and one for the root itself.

| column | what it holds |
|---|---|
| `id` | its path relative to the scan root; `''` for the root |
| `size` | the bytes of every file and symbolic link below it, at any depth |
| `entries` | how many files, symbolic links and folders are below it, at any depth, itself not counted |
| `blake3` | a hash over its direct children's names, kinds and hashes, as 32 raw bytes, so it changes when anything below it changes and is the same for two identical trees |
| `identity_uuid` | the id in its own `.file_inventory.toml`'s `[identity]` table, when it has one; NULL otherwise |

### `scan_meta`

One row: the root this source scans. Rows left by another source's id are deleted at each scan.

| column | what it holds |
|---|---|
| `id` | the source's id |
| `abs_path` | the absolute path of the scan root, with symbolic links in it resolved |
| `os` | the operating system the scan ran on (`macos`, `linux`, …) |
| `case_sensitive` | 0 on macOS, 1 elsewhere; a guess, not a check of the volume |
| `inode_stable` | always 1; not measured |
| `options_fingerprint` | a hash of the `ignore` patterns and `stamp_me_with_uuid` setting in the root's own `.file_inventory.toml` |
| `scanner_version` | the version of the scanner that wrote the row |

`scan_meta_bookkeeping.fetched_at_utc` is when this source's root was first scanned; later scans do not change it.

## Garmin Connect (`garmin`)

Garmin's records are kept as Garmin Connect's API (the one the phone
app uses, `connectapi.garmin.com`) returned them. `payload` is the JSON
of one answer, or of one entry of a listing:

- `garmin_daily`: the whole answer of one metric's endpoint for one
  day (`/usersummary-service/usersummary/daily/…`,
  `/wellness-service/wellness/dailySleepData/…`, `/hrv-service/hrv/…`,
  and so on), or JSON `null` when Garmin answered with nothing.
- `garmin_activities`: one entry of
  `/activitylist-service/activities/search/activities`.
- `garmin_activity_details`: `/activity-service/activity/<id>`, or
  JSON `null` when Garmin had no detail.
- `garmin_weigh_ins`: one entry of `allWeightMetrics` inside
  `/weight-service/weight/range/…`.
- `garmin_devices`: one entry of
  `/device-service/deviceregistration/devices`.
- `garmin_items`: one entry of the personal-record, gear, badge,
  workout or goal listing.
- `garmin_account`: `/userprofile-service/socialProfile` and
  `/userprofile-service/userprofile/user-settings`.

No field is set aside as volatile. A device's `lastSyncTimestampGMT`
moves whenever the watch syncs, so a device row changes often.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| an activity id (`activityId`) | `garmin_activities.id`, `garmin_activity_details.id`, `garmin_activity_files.activity_id` (the file row's `id` is `<activity id>#fit`) |
| a device id (`deviceId`) | `garmin_devices.id` |
| a weigh-in (`samplePk`) | `garmin_weigh_ins.id` |
| a metric on a day | `garmin_daily.id` = `<metric>#<YYYY-MM-DD>`, with `metric` and `calendar_date` as columns |
| a day's wellness bundle | `garmin_wellness_files.id` = `<YYYY-MM-DD>#wellness_zip` |
| a personal record, gear, badge, workout or goal | `garmin_items.upstream_id`; the row's `id` is `<kind>#<upstream id>` |
| the account's `displayName` | `garmin_account.display_name` |
| a FIT file or bundle's bytes | `blobs.sqlite` `cas_objects.blake3` = `garmin_activity_files.blake3` or `garmin_wellness_files.blake3` |

**"Me."** The store holds one account, and every row is that person's.
Their name is in the social profile:

```sql
SELECT display_name, json_extract(payload, '$.fullName') AS full_name
  FROM garmin_account WHERE id = 'social_profile';
```

**A metric over days.** The field paths the render uses are in
`garmin_render/src/render/daily.rs` (`FIELDS`); for example the sleep
score and the resting heart rate:

```sql
SELECT calendar_date, json_extract(payload, '$.dailySleepDTO.sleepScores.overall.value') AS sleep_score
  FROM garmin_daily WHERE metric = 'sleep' ORDER BY calendar_date;
SELECT calendar_date, json_extract(payload, '$.restingHeartRate') AS resting_hr
  FROM garmin_daily WHERE metric = 'daily_summary' ORDER BY calendar_date;
```

**How many days each metric holds, and how many had data:**

```sql
SELECT metric, count(*) AS days, sum(json(payload) <> 'null') AS days_with_data,
       min(calendar_date) AS first, max(calendar_date) AS last
  FROM garmin_daily GROUP BY metric;
```

**Weigh-ins in kilograms.** `weight_g` is grams and `timestamp_gmt` is
Unix milliseconds:

```sql
SELECT calendar_date, datetime(timestamp_gmt / 1000, 'unixepoch') AS at_utc,
       weight_g / 1000.0 AS kg, source_type
  FROM garmin_weigh_ins ORDER BY timestamp_gmt DESC;
```

**An activity and its FIT file.** `start_time_gmt` is Garmin's
`startTimeGMT` text. Attach the blob store to get the file's size:

```sql
ATTACH 'file:garmin/ingest/blobs.sqlite?mode=ro' AS cas;
SELECT a.id, a.start_time_gmt, a.activity_type, a.name, c.byte_len
  FROM garmin_activities a
  LEFT JOIN garmin_activity_files f ON f.id = a.id || '#fit'
  LEFT JOIN cas.cas_objects c ON c.blake3 = f.blake3
 ORDER BY a.start_time_gmt DESC;
```

**What has been listed: `coverage`.** Rows with scope `activities`
are the stretches of start dates the activity listing has walked to its
end; `lo` and `hi` are `YYYY-MM-DD`. Inside a span, every activity
Garmin listed when it was walked is stored.

### Is the copy complete?

Garmin writes no `sync_runs` rows. The last finished run's summary is
the message of the store's newest `download` commit; it counts
`errors=`, `listings_failed=` and `phases_failed=`. A run that failed
outright (for example the sign-in expired) leaves no such commit.

```sql
SELECT date, message FROM dolt_log() WHERE message LIKE 'download %' LIMIT 1;
```

**1. What failed.** Every way a run falls short is a row in `problems`:

```sql
SELECT severity, scope_key, sample, first_seen_at_utc
  FROM problems ORDER BY severity, scope_key;
```

- `garmin_daily:<metric>#<date>`: that day of that metric could not be
  fetched. It goes when a later run fetches it, or `since` moves past
  it.
- `garmin_activity_details:<id>`, `garmin_activity_files:<id>#fit`,
  `garmin_wellness_files:<date>#wellness_zip`: a detail, FIT file or
  bundle that could not be fetched (an error), or a FIT that could not
  be read (a warning; the activity is kept without its file).
- `listing:<name>` (`user_settings`, `devices`, `weight`, `activities`,
  `personal_records`, `gear`, `badges`, `workouts`, `goals`): a listing
  that did not answer with a complete list. Nothing was deleted on its
  account, so deletions there are not yet seen. `listing:gear` as a
  warning means the account has no `profileId`, and gear cannot be
  listed at all.
- `phase:<name>` (`devices`, `daily`, `weight`, `activities`,
  `wellness`, `items`): a whole phase failed, or a metric was dropped
  for the run after ten failures in a row.
- `skipped:…`: an entry Garmin listed with no id, so it was not stored.

A FIT file that is held but cannot be decoded is a warning row with
stage `time_series` in the `problems` table of
`<group>/render_time_series/time_series.doltlite_db`, not here.

**2. What is still owed.** The download keeps what each record is held
at in `held_version`, so what the next run will fetch can be asked of
the store.

Days of a metric that have settled (more than `refresh_days`, 7 by
default, ago) but are not held at their settle date. `:since` is the
window's start: the config's `since`, else the value of
`garmin:default_since` in `sync_scope_state`. Days within the last week
are re-fetched every run anyway. The boundary day can differ by one
from the run's, which uses the local date:

```sql
WITH RECURSIVE d(day) AS (
  SELECT :since
  UNION ALL SELECT date(day, '+1 day') FROM d WHERE day < date('now', '-7 days'))
SELECT m.metric, count(*) AS days_owed
  FROM d CROSS JOIN (SELECT DISTINCT metric FROM garmin_daily) m
  LEFT JOIN garmin_daily_bookkeeping b ON b.id = m.metric || '#' || d.day
 WHERE b.fetched_at_utc IS NULL OR b.held_version IS NOT date(d.day, '+7 days')
 GROUP BY m.metric;
```

Activities whose detail is not held for the listing entry now stored,
and (with `activity_files` on) FIT files still owed:

```sql
SELECT count(*) AS details_owed FROM garmin_activities a
  LEFT JOIN garmin_activity_details_bookkeeping b ON b.id = a.id
 WHERE a.listing_hash IS NOT NULL
   AND (b.fetched_at_utc IS NULL OR b.held_version IS NOT a.listing_hash);

SELECT count(*) AS files_owed FROM garmin_activities a
  LEFT JOIN garmin_activity_files f ON f.id = a.id || '#fit'
  LEFT JOIN garmin_activity_files_bookkeeping b ON b.id = a.id || '#fit'
 WHERE a.listing_hash IS NOT NULL AND f.blake3 IS NULL
   AND (b.fetched_at_utc IS NULL OR b.held_version IS NOT a.listing_hash);
```

The activity listing is complete back to `since` when one `coverage`
span with scope `activities` reaches from the window's start to the
last run's date; more than one span means a stretch was never listed:

```sql
SELECT lo, hi FROM coverage WHERE scope = 'activities' ORDER BY lo;
```

Weigh-ins keep no such record: the whole window is listed every run,
and a failed chunk is a `listing:weight` row.

### `garmin_account`

One row per account record: `social_profile` and `user_settings`.

| column | what it holds |
|---|---|
| `id` | `social_profile` or `user_settings` |
| `payload` | the endpoint's answer, JSONB |
| `display_name` | the account's `displayName`, which Garmin's per-user paths take |

### `garmin_devices`

One row per device registered to the account. Pruned to the listing
when it is a complete list.

| column | what it holds |
|---|---|
| `id` | Garmin's `deviceId` (else `unitId`) |
| `payload` | the device's entry in the registration listing, JSONB |
| `product_display_name` | `productDisplayName`, the device's model name |

### `garmin_daily`

One row per metric per calendar day asked for.

| column | what it holds |
|---|---|
| `id` | `<metric>#<YYYY-MM-DD>` |
| `payload` | the metric's endpoint answer for that day, JSONB; JSON `null` when Garmin had nothing (204, 404, `{}` or `[]`) |
| `metric` | one of the twenty names in `metrics` (`daily_summary`, `sleep`, `hrv`, …) |
| `calendar_date` | the day, `YYYY-MM-DD`, as the account lived it |

### `garmin_weigh_ins`

One row per weigh-in. Rows dated inside the window that Garmin no
longer lists are deleted.

| column | what it holds |
|---|---|
| `id` | Garmin's `samplePk` |
| `payload` | the weigh-in, JSONB (body fat, BMI and the rest are in here) |
| `calendar_date` | `calendarDate`, `YYYY-MM-DD` |
| `timestamp_gmt` | `timestampGMT`, Unix milliseconds |
| `weight_g` | `weight`, in grams |
| `source_type` | `sourceType`, such as `MANUAL` or `INDEX_SCALE` |

### `garmin_activities`

One row per activity, as the activity listing describes it.

| column | what it holds |
|---|---|
| `id` | Garmin's `activityId` |
| `payload` | the activity's listing entry, JSONB |
| `start_time_gmt` | `startTimeGMT`, Garmin's text for the start in UTC |
| `activity_type` | `activityType.typeKey`, such as `running` |
| `name` | `activityName` |
| `listing_hash` | the blake3 of the listing entry; when it changes, the detail is fetched again |

### `garmin_activity_details`

One row per activity whose detail was asked for, keyed like
`garmin_activities`.

| column | what it holds |
|---|---|
| `id` | Garmin's `activityId` |
| `payload` | `/activity-service/activity/<id>`, JSONB; JSON `null` when Garmin had none; SQL NULL when every fetch so far failed |

### `garmin_activity_files`

One row per activity whose FIT file was asked for: the link to its
bytes in `blobs.sqlite`.

| column | what it holds |
|---|---|
| `id` | `<activity id>#fit` |
| `activity_id` | Garmin's `activityId` |
| `file_kind` | `fit` |
| `blake3` | the FIT file's key in `cas_objects`; NULL when Garmin had no file (an activity entered by hand) or the download held no readable FIT |

### `garmin_wellness_files`

One row per day whose wellness bundle was asked for (only with
`wellness_files = true`).

| column | what it holds |
|---|---|
| `id` | `<YYYY-MM-DD>#wellness_zip` |
| `calendar_date` | the day |
| `file_kind` | `wellness_zip` |
| `blake3` | the zip's key in `cas_objects`, as Garmin served it; NULL when Garmin had no bundle that day |

### `garmin_items`

One row per entry of the whole-account listings, re-read complete
every run and pruned to them.

| column | what it holds |
|---|---|
| `id` | `<kind>#<upstream id>` |
| `payload` | the entry, JSONB |
| `kind` | `personal_records`, `gear`, `badges`, `workouts` or `goals` (active goals only) |
| `upstream_id` | Garmin's id for it: `id`, `gearPk` or `uuid`, `badgeId` or `badgeUuid`, `workoutId`, `id` or `goalId` |

### `coverage`

The stretches of start dates the activity listing has walked to its
end (above).

| column | what it holds |
|---|---|
| `scope` | `activities` |
| `lo` | the first start date in the stretch, `YYYY-MM-DD` |
| `hi` | the last, the same way |

## GitHub (`github`)

GitHub's objects are kept as its REST API returned them, unchanged. Each table's `payload` is one object from one endpoint:

- `self_identity`: from `GET /user`;
- `pull_requests`: from `GET /repos/{repo}/pulls/{n}`;
- `issue_comments`: from `GET /repos/{repo}/issues/{n}/comments`;
- `pr_reviews`: from `GET /repos/{repo}/pulls/{n}/reviews`;
- `pr_review_comments`: from `GET /repos/{repo}/pulls/{n}/comments`.

No field is set aside in `volatile_payload`.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a repository `owner/name` | `repo_full_name` on every table but `self_identity` |
| a pull request's number | `pr_number`; the `pull_requests.id` is `<owner>/<name>#<number>` |
| a pull request's own numeric id | `json_extract(payload, '$.id')` on `pull_requests` |
| an issue comment's id (`#issuecomment-<id>` in a link) | `issue_comments.id`, as text |
| a review's id (`#pullrequestreview-<id>`) | `pr_reviews.id`, as text; `pr_review_comments.pull_request_review_id`, as an integer |
| a review comment's id (`#discussion_r<id>`) | `pr_review_comments.id`, as text; `in_reply_to_id` on the replies to it, as an integer |
| a user's login | `self_identity.login`; `user_login` on comments and reviews; `json_extract(payload, '$.user.login')` on `pull_requests` |

**Who "me" is.** `self_identity.login` is the account the source signs in as. There is one row per account it has ever signed in as; there is normally exactly one.

```sql
-- Pull requests I opened.
SELECT p.id, p.state, json_extract(p.payload, '$.title') AS title
  FROM pull_requests p, self_identity s
 WHERE json_extract(p.payload, '$.user.login') = s.login;
```

**One pull request's whole conversation, in time order:**

```sql
SELECT 'review' AS what, id, user_login, submitted_at AS at, state,
       json_extract(payload, '$.body') AS body
  FROM pr_reviews WHERE repo_full_name = :repo AND pr_number = :n
UNION ALL
SELECT 'comment', id, user_login, created_at, NULL,
       json_extract(payload, '$.body')
  FROM issue_comments WHERE repo_full_name = :repo AND pr_number = :n
UNION ALL
SELECT 'inline', id, user_login, created_at, NULL,
       json_extract(payload, '$.body')
  FROM pr_review_comments WHERE repo_full_name = :repo AND pr_number = :n
ORDER BY at;
```

**Inline threads.** A thread's first comment has `in_reply_to_id` NULL. Each reply's `in_reply_to_id` is the first comment's id:

```sql
SELECT r.id AS root, c.id, c.user_login, c.path, c.line, c.created_at
  FROM pr_review_comments r
  JOIN pr_review_comments c
    ON c.id = r.id OR c.in_reply_to_id = CAST(r.id AS INTEGER)
 WHERE r.in_reply_to_id IS NULL
   AND r.repo_full_name = :repo AND r.pr_number = :n
 ORDER BY r.id, c.created_at;
```

**A pull request GitHub no longer serves.** When GitHub answers 404 or 410 for a pull request, the stored copy is kept, and its row in `pull_requests_bookkeeping` and in `listed_change_requests` are removed. These queries find such pull requests:

```sql
SELECT p.id FROM pull_requests p
  LEFT JOIN pull_requests_bookkeeping b ON b.id = p.id
 WHERE b.id IS NULL;
```

**What has been looked at.** The download lists pull requests with GitHub's search, then fetches the ones it does not hold:

- `listed_change_requests` is every pull request a search has named, at the newest `updated_at` any search gave it.
- `coverage` records the stretch of `updated_at` each search has looked at.
- A pull request is held at a version when its `pull_requests_bookkeeping.held_version` equals the listed `updated_at`. The query is in the section "Is the copy complete?".

### Is the copy complete?

**1. What the download could not do.** These are the rows that mean something is missing:

```sql
SELECT scope_key, severity, reason, sample FROM problems
 WHERE scope_key LIKE 'pull_requests:%'   -- one pull request did not fetch whole
    OR scope_key LIKE 'listing:search %'  -- a search failed, was refused, or was cut off
    OR scope_key LIKE 'phase:%';          -- the run stopped early (phase:search, phase:fetch)
```

- `pull_requests:<owner>/<name>#<n>` means a fetch of that pull request, or of one of its comment lists, failed. It is an `error` when no copy is stored, and a `warning` when an older copy is stored and is now out of date. The next run tries again.
- `listing:search <filter>` has these meanings:
  - `error`: the search failed, and that stretch is still to search.
  - `warning` with reason `forbidden`: GitHub refused the token (401 or 403).
  - `warning` with reason `fetch_failed`: the search reached GitHub's 1,000-result cap and could not get past it.
- `phase:search` and `phase:fetch` mean the download gave up after rate limits or failures past its budget, or after 25 failed pull requests in a row. A row stays until a later run gets through.

**2. What is still owed.** A pull request is owed when a search listed it at an `updated_at` that its stored copy does not have:

```sql
SELECT l.id, l.updated_at, b.held_version, b.attempt_count, b.last_error
  FROM listed_change_requests l
  LEFT JOIN pull_requests_bookkeeping b ON b.id = l.id
 WHERE b.fetched_at_utc IS NULL
    OR b.held_version IS NOT l.updated_at;
```

An empty result means every pull request the searches have listed is held whole at the version listed. This is expected to be non-empty after a run capped by `max_prs`: the rest waits for later runs, and no `problems` row says so.

**3. Whether every search is caught up.** Each of the three filters should have exactly one `coverage` row, with `lo` empty:

```sql
SELECT scope, count(*) AS spans, min(lo) AS lo, max(hi) AS hi
  FROM coverage GROUP BY scope;
```

More than one span, or a non-empty `lo`, is a stretch still to search. A missing filter has never been searched. `hi` is when the last search of that filter began.

The newest `sync_runs` row's `status` says how the last run ended. It is `partial` when a search or the fetch loop ended early. A run with `api.pull_requests` set searches nothing, so neither query above covers the pull requests it names.

### `self_identity`

One row per GitHub account the source has signed in as.

| column | what it holds |
|---|---|
| `id` | the account's numeric user id, as text |
| `payload` | GitHub's `GET /user` answer |
| `login` | the account's login |
| `html_url` | its profile page on github.com |

### `pull_requests`

One row per pull request the download has fetched.

| column | what it holds |
|---|---|
| `id` | `<owner>/<name>#<number>` |
| `payload` | GitHub's `GET /repos/{repo}/pulls/{n}` answer |
| `repo_full_name` | the repository, `<owner>/<name>`, as it was named when the pull request was listed |
| `pr_number` | the pull request's number in that repository |
| `state` | `open` or `closed`; a merged pull request is `closed` with `merged_at` set |
| `html_url` | the pull request's page on github.com |
| `head_sha` | the commit its branch points at (`head.sha`) |
| `base_sha` | the commit of the branch it merges into (`base.sha`) |
| `head_ref` | its branch's name (`head.ref`) |
| `base_ref` | the name of the branch it merges into (`base.ref`) |
| `updated_at` | when GitHub last saw it change, as GitHub writes it (`2369-04-12T08:15:00Z`) |
| `merged_at` | when it was merged; NULL if it was not |

### `issue_comments`

One row per comment in a pull request's conversation tab: the comments that are not on the diff.

| column | what it holds |
|---|---|
| `id` | GitHub's numeric id for the comment, as text |
| `payload` | the comment as `GET /repos/{repo}/issues/{n}/comments` lists it |
| `repo_full_name` | the repository of the pull request it is on |
| `pr_number` | the number of the pull request it is on |
| `html_url` | the comment's link (`…#issuecomment-<id>`) |
| `user_login` | its author's login |
| `created_at` | when it was written |
| `updated_at` | when it was last edited; equal to `created_at` if it never was |

### `pr_reviews`

One row per review: an approval, a request for changes, or a review that only comments. The review's inline comments are in `pr_review_comments`.

| column | what it holds |
|---|---|
| `id` | GitHub's numeric id for the review, as text |
| `payload` | the review as `GET /repos/{repo}/pulls/{n}/reviews` lists it |
| `repo_full_name` | the repository of the pull request it is on |
| `pr_number` | the number of the pull request it is on |
| `state` | GitHub's review state: `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED` or `PENDING` |
| `commit_id` | the commit that was reviewed |
| `user_login` | the reviewer's login |
| `submitted_at` | when it was submitted; a review has no `updated_at` |
| `html_url` | the review's link (`…#pullrequestreview-<id>`) |

### `pr_review_comments`

One row per inline comment on a pull request's diff, a reply in a thread included.

| column | what it holds |
|---|---|
| `id` | GitHub's numeric id for the comment, as text |
| `payload` | the comment as `GET /repos/{repo}/pulls/{n}/comments` lists it |
| `repo_full_name` | the repository of the pull request it is on |
| `pr_number` | the number of the pull request it is on |
| `in_reply_to_id` | for a reply, the id of the thread's first comment, as an integer; NULL for the first comment |
| `pull_request_review_id` | the id of the review it was submitted with, as an integer |
| `html_url` | the comment's link (`…#discussion_r<id>`) |
| `user_login` | its author's login |
| `path` | the file it is on |
| `line` | the line it is on in the current diff; NULL when that line is no longer in the diff |
| `original_line` | the line it was written on |
| `commit_id` | the commit the current position is on |
| `original_commit_id` | the commit it was written on |
| `created_at` | when it was written |
| `updated_at` | when it was last edited; equal to `created_at` if it never was |

### `listed_change_requests`

One row per pull request a search has named. Nothing was fetched to make it: it is what the search said. A row is removed when GitHub answers 404 or 410 for the pull request.

| column | what it holds |
|---|---|
| `id` | the pull request, keyed as `pull_requests.id` is |
| `updated_at` | the newest `updated_at` any search gave it; NULL when no search gave one |

### `coverage`

The stretches of `updated_at` each search has looked at whole. Every pull request the search returned in such a stretch is in `listed_change_requests`.

| column | what it holds |
|---|---|
| `scope` | `search:<filter>`: `search:author:@me`, `search:commenter:@me` or `search:mentions:@me` |
| `lo` | the oldest `updated_at` of the stretch, as GitHub writes it; empty for the start of time |
| `hi` | the newest `updated_at` of the stretch: the time the sync that covered it began, or a later result's `updated_at` if GitHub's clock was ahead |

## GitLab (`gitlab`)

GitLab's objects are kept as its REST API (v4) returned them, with two exceptions. A numeric `v=` parameter is removed from every `avatar_url`. Your own user record's `local_time` is moved to `self_identity_bookkeeping.volatile_payload`. Each table's `payload` is one object from one endpoint:

- `self_identity`: from `GET /user`;
- `merge_requests`: from `GET /projects/{id}/merge_requests/{iid}`;
- `discussions`: one discussion from `GET /projects/{id}/merge_requests/{iid}/discussions`, with its `notes` array inside.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a project's path `group/project` | `project_full_path` on `merge_requests` and `discussions` |
| a project's numeric id | `json_extract(payload, '$.project_id')` on `merge_requests` |
| a merge request's number in its project (`!7`, its iid) | `mr_iid`; the `merge_requests.id` is `<path>!<iid>` |
| a merge request's global numeric id | `json_extract(payload, '$.id')` on `merge_requests` |
| a discussion's id (40 hex characters) | `discussions.discussion_id`; the row's `id` is `<path>!<iid>#<discussion id>` |
| a note's id (`#note_<id>` in a link) | inside a `discussions` payload, `$.notes[*].id` |
| a user's username | `self_identity.username`; `$.author.username` in a merge request's or a note's payload |

**Who "me" is.** `self_identity.username` is the account the source signs in as. There is one row per account it has ever signed in as; there is normally exactly one.

```sql
-- Merge requests I opened.
SELECT m.id, m.state, json_extract(m.payload, '$.title') AS title
  FROM merge_requests m, self_identity s
 WHERE json_extract(m.payload, '$.author.username') = s.username;
```

**One merge request's notes, in time order, without GitLab's system notes:**

```sql
SELECT d.discussion_id,
       n.value ->> '$.id'              AS note_id,
       n.value ->> '$.author.username' AS author,
       n.value ->> '$.created_at'      AS created_at,
       n.value ->> '$.position.new_path' AS path,
       n.value ->> '$.body'            AS body
  FROM discussions d, json_each(d.payload, '$.notes') n
 WHERE d.project_full_path = :project AND d.mr_iid = :iid
   AND coalesce(n.value ->> '$.system', 0) = 0
 ORDER BY created_at;
```

A note's link is the merge request's `web_url` followed by `#note_<id>`. In a discussion with `individual_note = 1` the note stands alone. Otherwise the notes after the first are replies to it.

**A merge request GitLab no longer serves.** When GitLab answers 404 or 410 for a merge request, the stored copy is kept, and its row in `merge_requests_bookkeeping` and in `listed_change_requests` are removed:

```sql
SELECT m.id FROM merge_requests m
  LEFT JOIN merge_requests_bookkeeping b ON b.id = m.id
 WHERE b.id IS NULL;
```

**What has been looked at.** The download lists merge requests with three lists, then fetches the ones it does not hold:

- `listed_change_requests` is every merge request a list has named, at the newest `updated_at` any list gave it.
- `coverage` records the stretch of `updated_at` each list has looked at.
- A merge request is held when its `merge_requests_bookkeeping.held_version` equals the listed `updated_at`. The query is in the section "Is the copy complete?".

### Is the copy complete?

**1. What the download could not do.** These are the rows that mean something is missing:

```sql
SELECT scope_key, severity, reason, sample FROM problems
 WHERE scope_key LIKE 'merge_requests:%'  -- one merge request did not fetch whole
    OR scope_key LIKE 'listing:search %'  -- a list failed or was refused
    OR scope_key LIKE 'phase:%';          -- the run stopped early (phase:search, phase:fetch)
```

- `merge_requests:<path>!<iid>` means a fetch of that merge request, or of its discussions, failed. It is an `error` when no copy is stored, and a `warning` when an older copy is stored and is now out of date. The next run tries again.
- `listing:search <list>` has these meanings:
  - `error`: the list failed, and that stretch is still to list.
  - `warning` with reason `forbidden`: GitLab refused the token (401 or 403).
- `phase:search` and `phase:fetch` mean the download gave up after rate limits or failures past its budget, or after 25 failed merge requests in a row. A row stays until a later run gets through.

**2. What is still owed.** A merge request is owed when a list named it at an `updated_at` that its stored copy does not have:

```sql
SELECT l.id, l.updated_at, b.held_version, b.attempt_count, b.last_error
  FROM listed_change_requests l
  LEFT JOIN merge_requests_bookkeeping b ON b.id = l.id
 WHERE b.fetched_at_utc IS NULL
    OR b.held_version IS NOT l.updated_at;
```

An empty result means every merge request the lists have named is held whole at the version listed. This is expected to be non-empty after a run capped by `max_mrs`: the rest waits for later runs, and no `problems` row says so.

**3. Whether every list is caught up.** Each of the three lists should have exactly one `coverage` row, with `lo` empty:

```sql
SELECT scope, count(*) AS spans, min(lo) AS lo, max(hi) AS hi
  FROM coverage GROUP BY scope;
```

More than one span, or a non-empty `lo`, is a stretch still to list. A missing list has never run. `hi` is when the last run of that list began.

The newest `sync_runs` row's `status` says how the last run ended. It is `partial` when a list or the fetch loop ended early. A run with `api.merge_requests` set lists nothing, so neither query above covers the merge requests it names.

### `self_identity`

One row per GitLab account the source has signed in as.

| column | what it holds |
|---|---|
| `id` | the account's numeric user id, as text |
| `payload` | GitLab's `GET /user` answer, without `local_time` |
| `username` | the account's username |
| `web_url` | its profile page |

### `merge_requests`

One row per merge request the download has fetched.

| column | what it holds |
|---|---|
| `id` | `<project path>!<iid>` |
| `payload` | GitLab's `GET /projects/{id}/merge_requests/{iid}` answer |
| `project_full_path` | the project's path, `group/project` (subgroups included), as it was when the merge request was listed |
| `mr_iid` | the merge request's number in its project |
| `state` | GitLab's state: `opened`, `closed`, `merged` or `locked` |
| `web_url` | the merge request's page |
| `head_sha` | the newest commit of its branch (`diff_refs.head_sha`) |
| `base_sha` | the commit its diff is taken against (`diff_refs.base_sha`) |
| `start_sha` | the target branch's commit when the diff was made (`diff_refs.start_sha`) |
| `source_branch` | its branch's name |
| `target_branch` | the name of the branch it merges into |
| `updated_at` | when GitLab last saw it change, to the millisecond (`2369-04-12T08:15:00.000Z`) |
| `merged_at` | when it was merged; NULL if it was not |

### `discussions`

One row per discussion on a merge request: a thread of notes, or one standalone note.

| column | what it holds |
|---|---|
| `id` | `<project path>!<iid>#<discussion id>` |
| `payload` | the discussion as GitLab lists it, its `notes` array included |
| `project_full_path` | the project of the merge request it is on |
| `mr_iid` | the number of the merge request it is on |
| `discussion_id` | GitLab's id for the discussion |
| `individual_note` | 1 for a standalone note that cannot be replied to as a thread, 0 for a thread |
| `max_note_updated_at` | the newest `updated_at` among its notes |

### `listed_change_requests`

One row per merge request a list has named. Nothing was fetched to make it: it is what the list said. A row is removed when GitLab answers 404 or 410 for the merge request.

| column | what it holds |
|---|---|
| `id` | the merge request, keyed as `merge_requests.id` is |
| `updated_at` | the newest `updated_at` any list gave it; NULL when no list gave one |

### `coverage`

The stretches of `updated_at` each list has looked at whole. Every merge request the list returned in such a stretch is in `listed_change_requests`.

| column | what it holds |
|---|---|
| `scope` | `search:created_by_me`, `search:assigned_to_me` or `search:reviewer` |
| `lo` | the oldest `updated_at` of the stretch, as GitLab writes it; empty for the start of time |
| `hi` | the newest `updated_at` of the stretch: the time the sync that covered it began, or a later result's `updated_at` if GitLab's clock was ahead |

## Google Drive (`google_drive`)

The store keeps each Google Doc, Sheet and Slides deck at every Drive
`version` a sync saw. `files.payload` is the file's entry from Drive's
`files.list` (`fields=files(*)`), as of the newest version captured.
`comments.payload` is one comment thread from `comments.list`
(`includeDeleted=true`), replies inline. `revisions.payload` is one
revision from `revisions.list`. `folders.payload` is a folder's
`{id, name, parents, driveId}`; `drives.payload` a shared drive's
`{id, name}` from `drives.list`. The file's content in Google's own
form (the Docs, Sheets or Slides API's JSON for the whole file) is not
a payload: it is a gzipped blob in `blobs.sqlite`, as are the images it
shows and any exports.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a file id (the `<id>` in `docs.google.com/document/d/<id>/…`) | `files.id`, `file_parents.id`, and `file_id` on `captures`, `comments`, `revisions` |
| a Drive `version` of a file | `captures.version`; the row's `id` is `<file id>#<version>` |
| a revision id | `revisions.id` is `<file id>#<revision id>`; `captures.head_revision_id` is the bare revision id |
| a comment id (the `disco=` in a comment's link) | `comments.id` is `<file id>#<comment id>` |
| a reply id | inside its thread's `comments.payload`, `$.replies[*].id` |
| a folder id | `folders.id`, and in `$.parents` of `files`, `file_parents` and `folders` payloads |
| a shared drive id | `drives.id`; it is also the id of that drive's root folder, so top folders name it as their parent |
| a blob's blake3 hash | `capture_parts.blake3`, `revision_exports.blake3`, and `cas_objects.blake3` in `blobs.sqlite` |

**"Me".** In a file's payload, the `owners`, `lastModifyingUser` and
`sharingUser` entries carry `"me": true` for the account, and in a
comment the `author` does; a comment author has no address otherwise.

The files the account owns:

```sql
SELECT f.id, json_extract(f.payload, '$.name') AS name
  FROM files f, json_each(f.payload, '$.owners') o
 WHERE json_extract(o.value, '$.me') = 1;
```

Each file's versions, newest first, and what each holds besides its
images:

```sql
SELECT c.file_id, json_extract(f.payload, '$.name') AS name, c.version, c.modified_time,
       (SELECT group_concat(p.part, ' ') FROM capture_parts p
         WHERE p.capture_id = c.id AND p.part NOT LIKE 'image:%') AS parts
  FROM captures c JOIN files f ON f.id = c.file_id
 ORDER BY c.file_id, CAST(c.version AS INTEGER) DESC;
```

The newest version's native JSON, which is the file's content. Fetch
the bytes from `blobs.sqlite` and gunzip them:

```sql
SELECT p.blake3
  FROM captures c JOIN capture_parts p ON p.capture_id = c.id
 WHERE c.file_id = :file_id AND p.part = 'native.json.gz'
 ORDER BY CAST(c.version AS INTEGER) DESC LIMIT 1;
```

```sh
sqlite3 <data_root>/<group>/ingest/blobs.sqlite \
  "SELECT writefile('native.json.gz', bytes) FROM cas_objects WHERE blake3 = '<blake3>'"
gunzip native.json.gz
```

Open comment threads, with who started them:

```sql
SELECT file_id, json_extract(payload, '$.author.displayName') AS author,
       json_extract(payload, '$.quotedFileContent.value') AS on_text,
       json_extract(payload, '$.content') AS said
  FROM comments
 WHERE json_extract(payload, '$.resolved') = 0 AND json_extract(payload, '$.deleted') = 0;
```

**Kinds.** `files.payload ->> '$.mimeType'` is
`application/vnd.google-apps.document`, `…spreadsheet` or
`…presentation`.

**A `files` row with a NULL `payload`** is a file whose first fetch
failed: the row exists to carry its sidecar's error.

**Nothing is deleted.** A file that leaves Drive's listing (trashed,
unshared, deleted) keeps every row; so do folders and drives that leave
it, and comments and revisions Google later drops. Nothing in the store
says which files Drive still has.

**Set aside from the payload** (in `volatile_payload`): a file's
`thumbnailLink` and every `photoLink` (of `owners`, `permissions`,
`lastModifyingUser`, `sharingUser`); a revision's
`lastModifyingUser.photoLink`; a comment's `author.photoLink` and its
replies'; and on `captures_bookkeeping`, the image links taken out of
the native JSON, `{"image_links": {"<JSON pointer>": "<url>"}}`, which
Google signs afresh on every read and which expire in about 30
minutes. `files_bookkeeping.held_version` is the Drive `version` the
stored copy is of.

### Is the copy complete?

1. **Problems.** Anything under these keys means part of the copy is
   missing or stale:

   - `files:<file id>`: a file whose fetch failed (an error when no
     version of it is held; a warning when an older version is), or
     whose version was kept with an export or an image missing (a
     warning, the reason in `sample`);
   - `revision_exports:<id>`: an old revision's export that failed, or
     that Drive no longer has (a warning);
   - `listing:files`, `listing:folders`, `listing:drives`: a listing
     that stopped part-way, or that Drive itself called incomplete;
   - `phase:google-docs`, `phase:google-sheets`, `phase:google-slides`,
     `phase:revision exports`: a kind or a pass that stopped, often a
     sign-in missing or refused for that service;
   - `config:file_ids:<id>`: a file named in `file_ids` that Drive does
     not have or will not show.

   ```sql
   SELECT scope_key, severity, reason, sample FROM problems
    WHERE scope_key LIKE 'files:%' OR scope_key LIKE 'revision_exports:%'
       OR scope_key LIKE 'listing:%' OR scope_key LIKE 'phase:%' OR scope_key LIKE 'config:%'
    ORDER BY severity, scope_key;
   ```

2. **What is owed.** A file is owed when the stored copy is not at the
   `version` Drive lists now. The store does not keep the listed
   version, so only part of that is visible:

   - files the last listing named that no capture has ever landed for:

     ```sql
     SELECT p.id FROM file_parents p
       LEFT JOIN files_bookkeeping b ON b.id = p.id
      WHERE b.fetched_at_utc IS NULL;
     ```

   - files whose last attempt failed, or kept less than everything:

     ```sql
     SELECT id, attempt_count, last_error FROM files_bookkeeping WHERE last_error IS NOT NULL;
     ```

   - with `revision_exports` set, revisions not yet held in a format
     (here `odf` for Docs; Drive names the link by the export's MIME
     type, sometimes with `x-` after `application/`):

     ```sql
     SELECT r.id FROM revisions r JOIN files f ON f.id = r.file_id
      WHERE json_extract(f.payload, '$.mimeType') = 'application/vnd.google-apps.document'
        AND coalesce(json_extract(r.payload, '$.exportLinks."application/vnd.oasis.opendocument.text"'),
                     json_extract(r.payload, '$.exportLinks."application/x-vnd.oasis.opendocument.text"')) IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM revision_exports_bookkeeping b
                         WHERE b.id = r.id || '#export.odt' AND b.fetched_at_utc IS NOT NULL);
     ```

   A file edited upstream since the last sync, which no sync has
   attempted yet, cannot be seen from the store.

3. **The last run.** The newest `sync_runs` row: `status` `ok` means it
   listed everything and tried every owed file; its `summary` counts
   `owed`, `captured`, `failed` and `left` (owed files a stop or a
   give-up left for the next sync).

   ```sql
   SELECT started_at_utc, status, json_extract(summary, '$.left') AS left,
          json_extract(summary, '$.failed') AS failed
     FROM sync_runs ORDER BY run_id DESC LIMIT 1;
   ```

### `files`

One row per file the listing named and a capture stored, keyed by Drive's file id.

| column | what it holds |
|---|---|
| `id` | the Drive file id |
| `payload` | the file's Drive listing entry (`files(*)`) as of its newest capture, its `permissionIds`, `permissions`, `owners`, `parents` and `spaces` sorted; NULL if no capture of it has ever landed |

### `captures`

One row per version of a file a sync stored. Rows are only ever added: a file's history is its `captures` rows.

| column | what it holds |
|---|---|
| `id` | `<file id>#<version>` |
| `file_id` | the Drive file id |
| `version` | Drive's `version` for the file when it was listed, as text; compare as `CAST(version AS INTEGER)` |
| `modified_time` | the listing's `modifiedTime` |
| `native_revision_id` | the `revisionId` the native JSON itself names (Docs and decks; NULL for Sheets) |
| `head_revision_id` | the newest revision Drive listed for the file at that capture; NULL when it listed none |

### `capture_parts`

One row per part of a capture, pointing at its bytes in `blobs.sqlite`.

| column | what it holds |
|---|---|
| `id` | `<capture id>#<part>` |
| `capture_id` | the `captures.id` it belongs to |
| `part` | `native.json.gz` (Google's own JSON for the file, gzipped, its image links taken out), `export.<ext>` (`odt`, `ods`, `odp`, `docx`, `xlsx`, `pptx`, `pdf`), or `image:<JSON pointer>` (an image, by where its link was in the native JSON) |
| `blake3` | the bytes' hash in `blobs.sqlite`; NULL for an export Google declined (over 10 MB, or download turned off) or an image over 1 GiB |

### `revisions`

One row per revision Drive listed for a file, written each time the file is captured and never deleted.

| column | what it holds |
|---|---|
| `id` | `<file id>#<revision id>` |
| `payload` | the revision from `revisions.list`: `modifiedTime`, `lastModifyingUser`, `exportLinks` and more |
| `file_id` | the Drive file id |

### `revision_exports`

One row per export of an old revision `revision_exports` asked for, fetched once.

| column | what it holds |
|---|---|
| `id` | `<revisions.id>#export.<ext>` |
| `revision_id` | the `revisions.id` it is an export of |
| `part` | `export.<ext>` |
| `blake3` | the bytes' hash in `blobs.sqlite`; NULL when Drive no longer has that revision's content, or the download passed 1 GiB |

### `comments`

One row per comment thread on a file, replies inline, written each time the file is captured and never deleted.

| column | what it holds |
|---|---|
| `id` | `<file id>#<comment id>` |
| `payload` | the thread from `comments.list`: `author`, `content`, `htmlContent`, `quotedFileContent`, `resolved`, `deleted`, `createdTime`, `replies` and more |
| `file_id` | the Drive file id |

### `folders`

One row per folder the account can see and is not in the trash, listed every sync, plus My Drive's root.

| column | what it holds |
|---|---|
| `id` | the folder id; My Drive's root is a row of its own |
| `payload` | `{id, name, parents, driveId}` as listed, `parents` sorted |

### `drives`

One row per shared drive the account belongs to, listed every sync.

| column | what it holds |
|---|---|
| `id` | the shared drive's id |
| `payload` | `{id, name}` from `drives.list` |

### `file_parents`

One row per file the listing named, written every sync, so a move shows at the next sync without fetching the file again.

| column | what it holds |
|---|---|
| `id` | the Drive file id |
| `payload` | `{id, parents}` from the newest listing that named the file, `parents` sorted; no `parents` for a file in no folder |

## Google Takeout (`google_takeout`)

The store holds what one Takeout export said, feed by feed. There is
no API response: each `payload` is the export's own record, kept
whole where the export is JSON and rebuilt as JSON where it is HTML or
CSV. Chat's `payload` is the message object from `messages.json`, the
`group_info.json` or the `user_info.json`, as Google wrote it. Maps
reviews and saved places are one GeoJSON `Feature` each; a Maps photo is
its `.json` sidecar. Voice, YouTube, Gemini, Saved lists, My Maps and
Play Books are read from HTML, CSV or KML into small JSON objects,
described per table below. Attachment bytes (Chat files, Voice pictures
and recordings, Gemini files, Maps photos, greetings) are in
`blobs.sqlite`'s `cas_objects`, by `blake3`.

Most ids are minted by the ingest, as a uuidv5 of what the record says,
because the export gives most records no id. A record that changes what
it is keyed by gets a new id.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a Google Chat message id (`<space>/<topic>/<message>`, from `messages.json`) | `chat_messages.id` |
| a Chat space or DM's id (`AAAA…`) | the last word of `chat_groups.id` (`Space AAAA…`, `DM …`), and the first part of each of its `chat_messages.id` |
| a Chat export folder name (`Google Chat/Groups/<name>`) | `chat_groups.id`, `chat_messages.group_id` |
| a Chat user folder (`Google Chat/Users/User <id>`) | `chat_users.id` |
| a Chat attachment's `export_name` | `chat_attachments.export_name`; the row's `id` is `<message id>#<export_name>` |
| a Voice phone number | `voice_messages.conversation_key` for a one-to-one conversation |
| a YouTube video id (`watch?v=<id>`) | `youtube_watch_history.video_id` |
| a YouTube channel id (`UC…`) | `youtube_watch_history.channel_id`, `youtube_subscriptions.id` |
| a Maps photo's file name in `Maps/Photos and videos/` | `maps_photos.id` |
| a Saved list, My Maps map or Play Books page | `maps_saved_lists.file`, `my_maps_placemarks.file`, `play_books_annotations.file`, its path under `Takeout/` |

**Who is "me".** Chat: the address in `chat_users`' `user_info.json`.
Voice: `payload -> '$.is_me'` on a text. The other feeds hold only the
account's own records.

A Chat conversation, oldest first, with the account's messages marked:

```sql
SELECT m.when_ts,
       lower(m.sender_email) = (SELECT lower(json_extract(payload, '$.user.email'))
                                  FROM chat_users LIMIT 1) AS is_me,
       json_extract(m.payload, '$.creator.name') AS author,
       json_extract(m.payload, '$.text') AS text
  FROM chat_messages m
 WHERE m.group_id = 'Space AAAATenFwd'
 ORDER BY datetime(m.when_ts), m.id;
```

A Voice conversation, texts and calls together:

```sql
SELECT when_ts, kind,
       json_extract(payload, '$.is_me') AS is_me,
       coalesce(json_extract(payload, '$.body'), json_extract(payload, '$.transcript')) AS text
  FROM voice_messages
 WHERE conversation_key = '+12025550102'
 ORDER BY datetime(when_ts);
```

A Chat message's attachments with their bytes' size:

```sh
datalib-doltlite -readonly google_takeout/ingest/entities.doltlite_db \
  "ATTACH 'file:google_takeout/ingest/blobs.sqlite?mode=ro' AS cas;
   SELECT a.export_name, c.content_type, c.byte_len
     FROM chat_attachments a JOIN cas.cas_objects c ON c.blake3 = a.blake3
    WHERE a.message_id = 'TNG-BRIDGE/T1/T1'"
```

The channels watched most:

```sql
SELECT channel_id, json_extract(payload, '$.channelTitle') AS channel, count(*) AS watches
  FROM youtube_watch_history
 GROUP BY channel_id
 ORDER BY watches DESC
 LIMIT 10;
```

`ingested_files` keeps one scope per feed (`google_takeout/<feed>`) of
the files that feed last read, and `google_takeout/archives`, the
`.zip`/`.tgz` parts each feed last finished reading, keyed
`<feed>/<part>`. This source writes no `sync_runs` rows and no
`sync_scope_state` rows.

### Is the copy complete?

This source keeps no owed-record bookkeeping: a Takeout is read whole,
so "complete" means "this sync read every product it was told to, and
every file of it". It writes no `sync_runs` rows, so there is no
run status to read. Check three things.

1. **Problems that mean something is missing.** Errors and warnings,
   leaving out the notes about files left out on purpose:

   ```sql
   SELECT scope_key, severity, reason, sample
     FROM problems
    WHERE severity IN ('error', 'warning')
    ORDER BY scope_key;
   ```

   What each `scope_key` prefix means:

   | prefix | what is missing |
   |---|---|
   | `phase:<feed>` | that whole feed failed this run (an error, or a file in a layout the reader does not know); nothing of it was stored, deleted or marked read |
   | `listing:files` | the export could not be walked in full (a link, an entry that would not list); nothing was deleted this run |
   | `listing:removed_records` | Voice files were removed or rewritten but one would not read; no Voice record was deleted |
   | `record:files:<path>` | a file of the export would not open |
   | `skipped:<feed>:<hash>` | an entry read and not stored, or a file that would not read or parse; `sample` says which |
   | `skipped:unread_files:<hash>` (warning) | a kind of file no feed of this source reads |
   | `chat_attachments:…`, `gemini_attachments:…` | an attachment the export does not hold (`not_found`) or that would not read |
   | `maps_photos:<file>` | a Maps photo whose media is missing or would not read |

2. **Attachments with no bytes**, the one thing a later sync can still
   fill in from the same export:

   ```sql
   SELECT 'chat' AS feed, count(*) FROM chat_attachments WHERE blake3 IS NULL
   UNION ALL SELECT 'gemini', count(*) FROM gemini_attachments WHERE blake3 IS NULL
   UNION ALL SELECT 'maps_photos', count(*) FROM maps_photos WHERE blake3 IS NULL;
   ```

   A Voice attachment that is missing has no row at all; it shows only
   as a `skipped:google_voice:…` warning.

3. **That the last sync ran to its end.** The store's newest commit
   message is the last download's summary, and names the feeds that
   failed:

   ```sql
   SELECT date, message FROM dolt_log() ORDER BY date DESC LIMIT 1;
   ```

   `feeds_failed=0` means every feed that was on read. A sync that failed
   as a whole (a folder holding two exports, a part that would not read)
   commits nothing and leaves no row here; the step shows as failed on
   the Manage screen. A sync over an unchanged export commits nothing
   either, so the newest commit can be older than the last sync.

### `chat_groups`

One Google Chat space or DM: its `group_info.json`.

| column | what it holds |
|---|---|
| `id` | the export's folder name for it (`Space <id>`, `DM <id>`) |
| `payload` | `group_info.json` as Google wrote it: `name` (a named space's), `members` (each `name`, `email`, `user_type`) |

### `chat_users`

The account's own Chat user record, from `Users/User <id>/user_info.json`.

| column | what it holds |
|---|---|
| `id` | the export's folder name (`User <id>`) |
| `payload` | `user_info.json`: `user` (`name`, `email`, `user_type`), `membership_info` per space, `starred_group_id`, `starred_message_ids` |

### `chat_messages`

One Chat message, from its space's `messages.json`.

| column | what it holds |
|---|---|
| `id` | the message's `message_id` (`<space>/<topic>/<message>`) |
| `payload` | the message object as Google wrote it: `creator`, `created_date`, `updated_date`, `text`, `annotations`, `attached_files`, `previous_message_versions`, `quoted_message_metadata`, `reactions`, `message_labels`, `topic_id` |
| `group_id` | the folder it came from, `chat_groups.id` |
| `sender_email` | `creator.email`; NULL for a sender with none |
| `when_ts` | `created_date` as an RFC 3339 time with the offset it was written in; NULL where the date or its zone did not read |

### `chat_attachments`

One file a Chat message names, its earlier versions included.

| column | what it holds |
|---|---|
| `id` | `<message id>#<export_name>` |
| `message_id` | the message, `chat_messages.id` |
| `export_name` | the file's name as `attached_files[].export_name` gives it |
| `blake3` | the key of its bytes in `blobs.sqlite`; NULL when the export does not hold the file or it would not read |

### `gemini_activity`

One entry of Gemini Apps activity (`My Activity/Gemini Apps/MyActivity.html`): a prompt and Gemini's response.

| column | what it holds |
|---|---|
| `id` | a uuid minted from the prompt and its date line |
| `payload` | `promptText`, `responseHtml` (the response as the page's HTML), `attachedFiles` (each `file`, the name in the export, and `name`, as uploaded), `generatedImages`, `whenStr` (the date as written) |
| `when_ts` | the date as an RFC 3339 time with its offset |

### `gemini_attachments`

One file a Gemini entry names: an attached file, a generated image, or a preview.

| column | what it holds |
|---|---|
| `id` | `<activity id>#<filename>` |
| `activity_id` | the entry, `gemini_activity.id` |
| `filename` | the file's name in `My Activity/Gemini Apps/` |
| `blake3` | the key of its bytes in `blobs.sqlite`; NULL when the export does not hold it or it would not read |

### `maps_photos`

One photo or video the account added to a Maps place, from `Maps/Photos and videos/`.

| column | what it holds |
|---|---|
| `id` | the media's file name (its sidecar's name less `.json`) |
| `payload` | the sidecar JSON: `title`, `description`, `creationTime`, `photoTakenTime`, `geoData`, `geoDataExif`, `imageViews` |
| `when_ts` | `creationTime.timestamp` as written |
| `blake3` | the key of the media's bytes in `blobs.sqlite`; NULL when the media is missing or would not read |

### `maps_reviews`

One review the account wrote, from `Maps (your places)/Reviews.json`.

| column | what it holds |
|---|---|
| `id` | a uuid minted from the place id in `google_maps_url` and the review's `date` |
| `payload` | the GeoJSON `Feature`: `geometry`, and `properties` with `date`, `google_maps_url`, `location`, `five_star_rating_published`, `review_text_published`, `questions` |
| `when_ts` | `properties.date` as written |

### `maps_saved_lists`

One place on a Maps list, from `Saved/<list>.csv`.

| column | what it holds |
|---|---|
| `id` | a uuid minted from the file, the place's `Title` and `URL`, and how many identical entries came before it |
| `payload` | `list` (the list's name, from its file name), `description` (the lines before the header), `position`, `row` (the CSV row by its header: `Title`, `Note`, `URL`, `Tags`, `Comment`) |
| `file` | the list's path under `Takeout/` |

### `maps_saved_places`

One starred or saved place, from `Maps (your places)/Saved Places.json`.

| column | what it holds |
|---|---|
| `id` | a uuid minted from the place's id (its `!1s…` id, else `cid=`, else the `q=` query of a dropped pin) and its `date` |
| `payload` | the GeoJSON `Feature`: `geometry`, and `properties` with `date`, `google_maps_url`, `location`, `Comment` |
| `when_ts` | `properties.date` as written |

### `my_maps_placemarks`

One pin, line or shape on a My Maps map, from `My Maps/<map>.kmz`.

| column | what it holds |
|---|---|
| `id` | a uuid minted from the file, the layer's and placemark's names, its geometry, and how many identical ones came before it |
| `payload` | `map` (the KML `Document`: name, description), `folder` (the layer), `placemark` (the KML `Placemark` as JSON: `@attr` for attributes, `#text` for text), `position` |
| `file` | the map's path under `Takeout/` |

### `play_books_annotations`

One highlight, note, bookmark or listening position, from `Google Play Books/<book>/<book>.html`.

| column | what it holds |
|---|---|
| `id` | a uuid minted from the file, the entry's section, kind, quoted text, note, position and date, and how many identical ones came before it |
| `payload` | `book` (title, author, narrator, links), `section` (`My notes`, `My bookmarks`, `Listening History`), `kind`, `fields` (the entry's parts by class name, `book-text`, `annotation-text`, `document-position-text`, `last-modified-date`, …), `position` |
| `file` | the book page's path under `Takeout/` |
| `when_ts` | `last-modified-date` as an RFC 3339 time, read in the zone the page names; NULL when it did not read |

### `voice_attachments`

One file a Voice record carries: a picture in a text, or a call's recording.

| column | what it holds |
|---|---|
| `id` | `<message id>#<ref_name>` |
| `message_id` | the record, `voice_messages.id`, or a greeting's `voice_greetings.id` |
| `ref_name` | the file's name in `Voice/Calls/` (or `Voice/Spam/`, `Voice/Greetings/`) |
| `blake3` | the key of its bytes in `blobs.sqlite` |

### `voice_bills`

One row of the table in `Voice/Bills.html`.

| column | what it holds |
|---|---|
| `id` | a uuid minted from the row's cells |
| `payload` | `kind` (`bill`), `headers` (the table's column names), `cells` (this row's values) |

### `voice_greetings`

One voicemail greeting, from `Voice/Greetings/`.

| column | what it holds |
|---|---|
| `id` | a uuid minted from the file's name |
| `payload` | `kind` (`greeting`), `filename` |
| `blake3` | the key of the audio's bytes in `blobs.sqlite` |

### `voice_messages`

One text, or one call or voicemail, from a page under `Voice/Calls/` (or `Voice/Spam/`).

| column | what it holds |
|---|---|
| `id` | a uuid minted from the folder, conversation, time, sender and text (a text), or kind, folder, other party and time (a call) |
| `payload` | `id`, `kind`, `folder`, `conversation_key`, `conversation_display`, `when`, `when_raw`; a text adds `sender` (`tel`, `name`), `is_me`, `body`, `attachments`; a call adds `party` (`tel`, `name`), `transcript`, `duration` (ISO 8601, `PT13S`), `audio`; a recording with no page adds `orphan: true` |
| `conversation_key` | the other party's number; `group:<hash>` for a group; `label:<name>` when the file names no number; `unknown` |
| `when_ts` | the time as RFC 3339 with milliseconds and its offset; NULL where it did not read |
| `kind` | `text`, `voicemail`, `missed`, `placed`, `received` or `recorded` |
| `folder` | `calls` or `spam` |

### `youtube_subscriptions`

One channel the account subscribes to, from `subscriptions.csv`.

| column | what it holds |
|---|---|
| `id` | the channel id (`UC…`) |
| `payload` | `channelId`, `channelUrl`, `channelTitle` |
| `channel_title` | the channel's title |

### `youtube_watch_history`

One video watched, from `watch-history.html`.

| column | what it holds |
|---|---|
| `id` | a uuid minted from the video id and the time |
| `payload` | `videoUrl`, `videoId`, `videoTitle`, `channelUrl`, `channelId`, `channelTitle`, `whenStr` (the date as written) |
| `when_ts` | the date as an RFC 3339 time with its offset; NULL where its zone did not read |
| `video_id` | the video's id |
| `channel_id` | the channel's id; NULL when the entry names no channel |

## IQAir AirVisual (`airvisual`)

The store is read from the AirVisual Pro's own files: the monthly
history files `YYYYMM_AirVisual_values.txt` (one `;`-separated line per
sample, in the folder and in each `archive<N>/`), `history.txt` (one
JSON line per clock change) and `latest_config_measurements.json` (the
monitor's name, serial and settings). There is no `payload` on the
sample rows: every measurement is a typed column, in the unit the
monitor logs it. A blank cell and `-1` are both NULL. There is no
`_bookkeeping` sidecar and no `sync_runs` row.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a monitor's serial number | `airvisual_devices.id`, `airvisual_samples.device_id`, `airvisual_clock_changes.device_id`, `ingested_files.scope` = `airvisual/export/<serial>` |
| a monitor's name | `airvisual_devices.name` (the config's `name`, else the monitor's own) |
| a history file | `airvisual_samples.source_file` and `ingested_files.rel_path`, relative to the monitor's folder |
| a line logged before the clock was set | `airvisual_unplaced_samples.id` = `<serial>#<file>#<line>` |

There is no "me": every monitor is the owner's.

**Times.** `ts_ms` (and `airvisual_devices.last_ts_ms`) is the
monitor's local wall clock written as if it were UTC. So this prints
the time the monitor's screen showed:

```sql
SELECT device_id, datetime(ts_ms / 1000, 'unixepoch') AS local_time, co2_ppm, pm25_ugm3
  FROM airvisual_samples WHERE device_id = :serial
 ORDER BY ts_ms DESC LIMIT 20;
```

The real time is `ts_ms` minus the monitor's UTC offset at the time.
For a monitor that has stayed in one time zone with no clock change
recorded, that is the zone's offset (`airvisual_devices.timezone`). The
render works it out per stretch between clock changes; the time series
hold the result.

**Daily means, by the monitor's local day:**

```sql
SELECT date(ts_ms / 1000, 'unixepoch') AS local_day,
       avg(pm25_ugm3) AS pm25, avg(co2_ppm) AS co2, max(co2_ppm) AS co2_max,
       count(*) AS samples
  FROM airvisual_samples WHERE device_id = :serial
 GROUP BY local_day ORDER BY local_day;
```

Samples are unevenly spaced by design (the monitor's sensor schedule:
10 seconds, 5 minutes and 15 minutes all occur), so weight by time, not
by row, when an average matters.

### Is the copy complete?

AirVisual writes no `sync_runs` rows, and a sync that finds nothing new
writes nothing, so the store's newest commit says when something last
changed, not when the last sync ran.

**1. What failed:**

```sql
SELECT severity, scope_key, sample FROM problems ORDER BY scope_key;
```

- `listing:device <name or path>`: a monitor's folder was missing or
  empty (most often the share is not mounted), could not be read, or
  gave no serial. Nothing new was read from that monitor.
- `listing:clock <serial>`: its clock cannot be trusted (a jump no time
  zone explains, an unknown time zone, an `archive<N>/` with no line in
  `history.txt`, a newest reading more than 15 minutes in the future).
  Its readings may be at the wrong real time.
- `listing:files <serial>`: some entries of the folder could not be
  read.
- `record:airvisual_files:<serial>/<path>`: a history file, or
  `latest_config_measurements.json`, that could not be read. It is
  tried again every sync.
- `file:airvisual/export/<serial>:<path>`: a history file that would
  not parse, or lines in it that could not be used, or columns in it
  that are not stored. It stays until the file changes.

**2. What is still owed.** The store keeps no owed records: each sync
reads every file whose content changed. What it holds is listed in
`ingested_files`; compare it with the files on the share to see one
that was never read:

```sql
SELECT scope, rel_path, size_bytes, last_finished_at_utc
  FROM ingested_files WHERE scope LIKE 'airvisual/export/%' ORDER BY scope, rel_path;
```

How recent each monitor is, on its own clock:

```sql
SELECT id, name, datetime(last_ts_ms / 1000, 'unixepoch') AS newest_local_time
  FROM airvisual_devices;
```

### `airvisual_devices`

One row per monitor ever read.

| column | what it holds |
|---|---|
| `id` | the serial number, which keys every sample |
| `name` | the config's `name`, else the monitor's own `node_name`, else the serial |
| `model` | `status.model` from `latest_config_measurements.json` |
| `mac_address` | `status.mac_address` |
| `app_version` | `status.app_version`, the firmware |
| `system_version` | `status.system_version` |
| `timezone` | `settings.timezone`, an IANA name such as `Europe/Zurich` |
| `last_ts_ms` | the newest sample's `ts_ms`, on the monitor's own clock (not real time) |

### `airvisual_samples`

One row per monitor and logged second.

| column | what it holds |
|---|---|
| `device_id` | the serial, `airvisual_devices.id` |
| `ts_ms` | the line's `Timestamp` times 1,000: the monitor's local clock written as UTC |
| `pm25_ugm3` | PM2.5, µg/m³ |
| `pm10_ugm3` | PM10, µg/m³ |
| `pm1_ugm3` | PM1, µg/m³ |
| `aqi_us` | the monitor's AQI on the US scale |
| `aqi_cn` | the monitor's AQI on the Chinese scale |
| `outdoor_aqi_us` | the AQI (US) of the public station the monitor follows, not this monitor's |
| `outdoor_aqi_cn` | the same station's AQI on the Chinese scale |
| `temperature_c` | temperature, °C |
| `humidity_pct` | relative humidity, % |
| `co2_ppm` | CO₂, ppm |
| `voc_ppb` | VOC, ppb, on the models that log it |
| `source_file` | the history file the row was last read from, relative to the monitor's folder |

### `airvisual_clock_changes`

One row per line of a monitor's `history.txt`: its clock jumped, and
the files from before the jump went into `archive<archive>/`. Replaced
whole each run.

| column | what it holds |
|---|---|
| `device_id` | the serial |
| `archive` | N of the `archive<N>/` folder this change started |
| `previous_wall_s` | the clock just before the jump, seconds, on the monitor's own count |
| `after_wall_s` | the clock just after it, the same way |
| `after_timezone` | the time zone the monitor said it was in after the jump |

### `airvisual_unplaced_samples`

One row per line logged before the monitor's clock was set (a
`Timestamp` before 2000), which cannot be placed in time.

| column | what it holds |
|---|---|
| `id` | `<serial>#<source_file>#<line_no>` |
| `payload` | the whole line as a JSON object of strings, `{header: value}`, JSONB |
| `device_id` | the serial |
| `source_file` | the file, relative to the monitor's folder |
| `line_no` | the line's number in the file, the header being line 1 |
| `device_ts_s` | the `Timestamp` as logged: seconds since that boot |

## Lightroom (`lightroom`)

The store holds a copy of an Adobe Lightroom Classic catalog (a
`.lrcat`, an ordinary SQLite file), **table for table, with no tables of
its own** in its schema record. Every catalog table is mirrored under
its own name with its own columns, as that Lightroom version laid them
out (113 to 135 tables in the catalogs measured); `.tables` and
`.schema <table>` in `datalib-doltlite` list them. There is no `payload`
column: each row is the row the catalog had. Values keep the catalog's
own types, so a column with no declared type can hold an integer in one
row and a blob in the next. Besides the mirrored tables the store has the
shared `problems`, `sync_runs` and `sync_scope_state`; this source writes
no `sync_runs` rows. A source with `backups.path` also has the backups
ledger below.

What is not copied: indexes, triggers and views; generated columns;
column defaults and CHECK or foreign key rules. `exclude_columns` drops
columns, and `skip_xmp` drops `Adobe_AdditionalMetadata.xmp`,
`AgMetadataSearchIndex.searchIndex` and the `AgMetadataSearchIndex`
columns ending in `SearchIndex`. `include_tables` and `exclude_tables`
choose tables.

**Each sync replaces the whole copy.** The mirror drops every table and
fills it again from the catalog; the storage engine stores only what
changed, so a sync of an unchanged catalog makes no commit. A table the
catalog no longer has is gone from the current copy, and a dropped
column is gone from `dolt_history_<table>` too; opening an earlier
commit read-only (`<file>@<commit>`) reads it back.

**Commits.** A sync with `backups.path` first mirrors each backup the
store does not hold, oldest first, one commit each, dated when the
backup was taken (its folder's name, read in this machine's time zone),
with the message `download <source id>: backup <file>`. It then mirrors
the newest state on top: the live catalog when `catalog.path` is set
(`download <source id>: catalog <path>`), otherwise the newest backup
again. A backup is known by the hash of its file, so renaming its folder
does not replay it.

**Keys.** A table with a single-column UNIQUE index on `id_global` is
keyed on it, so a renumbered `id_local` reads as an edit; other tables
keep their declared key (usually `id_local`). A table with no declared
key and exactly one UNIQUE index with no NULLs in it is keyed on that
index; with neither it is keyless and diffs by row position.
`stable_key_columns` (`["id_global"]` unless set) and `primary_keys`
change this.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a photo's Lightroom UUID | `Adobe_images.id_global`; the grid row's `upstream_id` |
| an image's row number | `Adobe_images.id_local`, which `AgLibraryKeywordImage.image`, `AgLibraryCollectionImage.image` and others reference |
| its file | `Adobe_images.rootFile` → `AgLibraryFile.id_local` (`baseName`, `extension`, `folder`) → `AgLibraryFolder.id_local` (`pathFromRoot`, `rootFolder`) → `AgLibraryRootFolder.id_local` (`absolutePath`) |
| a keyword | `AgLibraryKeyword.id_local`, joined to images by `AgLibraryKeywordImage` |
| a collection | `AgLibraryCollection.id_local`, joined by `AgLibraryCollectionImage` |

Each image with its file's full path, rating and capture time:

```sql
SELECT i.id_global, rf.absolutePath || fo.pathFromRoot || f.baseName
         || coalesce('.' || f.extension, '') AS path,
       i.rating, i.captureTime
  FROM Adobe_images i
  JOIN AgLibraryFile f ON f.id_local = i.rootFile
  LEFT JOIN AgLibraryFolder fo ON fo.id_local = f.folder
  LEFT JOIN AgLibraryRootFolder rf ON rf.id_local = fo.rootFolder;
```

`captureTime` is a clock time, usually without an offset; the rendered
grid row's `created_at` has the offset Datalib worked out.

Every value one photo's row has held, in commit order:

```sql
SELECT h.commit_hash, l.date, h.rating, h.pick
  FROM dolt_history_Adobe_images h
  JOIN dolt_log() l ON l.commit_hash = h.commit_hash
 WHERE h.id_global = :id_global;
```

`dolt_log()` lists commits newest first along the branch, which is the
order to trust; `date` and `commit_date` are backup times for replayed
backups.

### The backups ledger: lightroom_snapshots

One row per backup the store holds, written in the commit that mirrored
it. It exists only in a store that has read a backups folder, and is not
in the store's schema record.

| column | what it holds |
|---|---|
| `snapshot` | the backup's entry name in the folder (`2026-09-27 1650`, maybe with a note after it) |
| `taken_at` | when it was taken, from the name, as local `YYYY-MM-DDTHH:MM:SS` |
| `file` | the file mirrored, relative to the backups folder |
| `blake3` | the hash of that file, lowercase hex; how a renamed backup is recognized |

### Is the copy complete?

This source keeps no record of what is owed for the catalog: each sync
reads the whole of it. For a backups folder, `lightroom_snapshots` is the
record of which backups are held.

1. **Problems the download recorded.**

   ```sql
   SELECT scope_key, severity, sample, changed_at_utc
     FROM problems
    ORDER BY changed_at_utc DESC;
   ```

   - `phase:source`: the live catalog was refused (no table in it, the
     filters matched none, or a table named like one the store keeps);
     the copy is as the sync before left it.
   - `record:lightroom_snapshots:<entry name>`: a backup that would not
     mirror (a zip that will not open, a folder holding two catalogs, a
     name that does not start with a date), or the newest held backup is
     gone from the folder. Every sync tries it again.
   - `listing:backups`: part of the backups folder could not be walked;
     `record:backups:<path>`: a file in it could not be read.

   A sync that failed outright (a missing `catalog.path`, a backups
   folder that is not mounted or holds no backups) records nothing here
   and commits nothing; it shows as a failed step on the Manage row.

2. **Which backups are held.** Compare the folder with the ledger:

   ```sql
   SELECT snapshot, taken_at, file FROM lightroom_snapshots ORDER BY taken_at;
   ```

3. **When the copy last changed.** `sync_runs` stays empty for this
   source. The newest commit along the branch is the last sync that
   changed anything:

   ```sql
   SELECT commit_hash, date, message FROM dolt_log() LIMIT 1;
   ```

   A sync that found nothing new makes no commit. When the last sync ran
   and whether it succeeded is the step's row in
   `<data_root>/system/supervisor.sqlite`
   (`SELECT last_status, last_success_at_utc, last_error FROM steps WHERE step = '<source id>/ingest'`).

4. **What render left out** is in the render step's own store: a
   `capture_time_assumed_utc` warning on each photo read as UTC, and one
   `skip_xmp` problem when the XMP is not mirrored.

## Linear (`linear`)

Linear's objects are kept as its GraphQL API (`api.linear.app/graphql`)
returned them: `payload` is one node of the connection the table is
named for (`issues`, `comments`, `documents`, `projectUpdates`,
`attachments`, `teams`, `users`, `workflowStates`, `issueLabels`,
`projectStatuses`, `cycles`, `projects`, `projectMilestones`,
`initiatives`), asked for with `includeArchived: true`, so archived
records are here with `archivedAt` set. A reference to another object is
stored as `{ "id": "…" }` and nothing else. A list of ids Linear returns
in no particular order (`labelIds`, and connections such as
`teams.nodes`) is sorted by id. There is no render step; these tables
are the whole copy.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| an issue's uuid | `issues.id`; `$.issueId` on a comment; `$.issue.id` on a history entry and an attachment |
| an issue key (`ENG-123`) | `$.identifier` in `issues.payload`; earlier keys, from before a move, in `$.previousIdentifiers` |
| a team id, or its key (`ENG`) | `teams.id`, `$.key` in its payload; `$.team.id` on issues, cycles, workflow states, labels |
| a person | `users.id`; `$.assignee.id`, `$.creator.id` on an issue; `$.user.id` on a comment or project update; `$.actorId` on a history entry |
| a workflow state | `workflow_states.id`; `$.state.id` on an issue; `$.fromStateId`, `$.toStateId` on a history entry |
| a label | `issue_labels.id`; an entry of `$.labelIds` on an issue |
| a project | `projects.id` (`$.slugId` is the id in its URL); `$.project.id` on an issue, document, milestone, project update |
| a cycle | `cycles.id`; `$.cycle.id` on an issue |
| a comment's parent | `$.parentId` names the comment it replies to |
| a document's text, from a comment on it | `$.documentContentId` on the comment equals `$.documentContentId` on the document |

**"Me".** The store does not record which person the source signs in
as: Linear's `isMe` and `viewer` are not asked for, because they
depend on who is asking. Find the person in `users` by the email they
sign in with (`$.email`).

**Set aside from the payload** (in `volatile_payload`): `updatedAt` on
every table. Also: a team's `issueCount` and `activeCycle`; a person's
`lastSeen`, `statusEmoji`, `statusLabel`, `statusUntilAt` and
`createdIssueCount`; a label's `lastAppliedAt`; a cycle's `progress`,
`issueCountHistory`, `completedIssueCountHistory`, `scopeHistory`,
`completedScopeHistory` and `inProgressScopeHistory`; a project's
`sortOrder`, `prioritySortOrder`, `scope`, `lastUpdate`,
`resourceCount` and the same six series; a milestone's `sortOrder` and
`progress`; an initiative's `sortOrder`, `prioritySortOrder` and
`lastUpdate`; an issue's `sortOrder`, `prioritySortOrder`,
`subIssueSortOrder` and `customerTicketCount`; a document's
`sortOrder`; a project update's `isStale` and `commentCount`.

**The sidecar's stamps mean different things on two kinds of table.**
The tables read whole every run (`teams`, `users`, `workflow_states`,
`issue_labels`, `project_statuses`, `cycles`, `projects`,
`project_milestones`, `initiatives`) and `issue_history` stamp
`fetched_at_utc` only the first time a row is seen, and keep no
`held_version`. The walked tables (`issues`, `comments`, `documents`,
`project_updates`, `attachments`) stamp it each time the record is
written, and `held_version` is its `updatedAt`.

Open issues with their state and assignee, newest change first:

```sql
SELECT i.payload ->> '$.identifier' AS issue,
       i.payload ->> '$.title' AS title,
       s.payload ->> '$.name' AS state,
       u.payload ->> '$.name' AS assignee,
       b.held_version AS updated_at
  FROM issues i
  JOIN issues_bookkeeping b ON b.id = i.id
  LEFT JOIN workflow_states s ON s.id = i.payload ->> '$.state.id'
  LEFT JOIN users u ON u.id = i.payload ->> '$.assignee.id'
 WHERE i.payload ->> '$.archivedAt' IS NULL
   AND s.payload ->> '$.type' NOT IN ('completed', 'canceled')
 ORDER BY b.held_version DESC;
```

An issue's comments, oldest first, with who wrote each and which one
it replies to:

```sql
SELECT c.id, c.payload ->> '$.parentId' AS reply_to,
       c.payload ->> '$.createdAt' AS created_at,
       u.payload ->> '$.name' AS author,
       c.payload ->> '$.body' AS body
  FROM comments c
  JOIN issues i ON i.id = c.payload ->> '$.issueId'
  LEFT JOIN users u ON u.id = c.payload ->> '$.user.id'
 WHERE i.payload ->> '$.identifier' = 'ENG-123'
 ORDER BY c.payload ->> '$.createdAt';
```

An issue's state changes, from its history:

```sql
SELECT h.payload ->> '$.createdAt' AS at,
       a.payload ->> '$.name' AS by,
       f.payload ->> '$.name' AS from_state,
       t.payload ->> '$.name' AS to_state
  FROM issue_history h
  JOIN issues i ON i.id = h.payload ->> '$.issue.id'
  LEFT JOIN users a ON a.id = h.payload ->> '$.actorId'
  LEFT JOIN workflow_states f ON f.id = h.payload ->> '$.fromStateId'
  LEFT JOIN workflow_states t ON t.id = h.payload ->> '$.toStateId'
 WHERE i.payload ->> '$.identifier' = 'ENG-123'
   AND h.payload ->> '$.toStateId' IS NOT NULL
 ORDER BY at;
```

**What is not mirrored**: files uploaded into Linear
(`uploads.linear.app`); kinds of object other than the tables below
(initiative updates, customers, releases, notifications, views); with
`teams` or `projects` set, issues outside them, comments on documents
and initiatives, and documents that belong to neither a chosen team nor
a kept project; an issue's relations past 50, a project's teams past 50
and members past 100, an initiative's projects past 100, and every
issue's `inverseRelations`. Nothing is ever removed: a record Linear
deletes stays as last fetched.

### Is the copy complete?

How to tell whether the Linear copy is complete.

**1. Problems.** Every row means something is missing or late:

```sql
SELECT scope_key, severity, reason, sample, changed_at_utc
  FROM problems
 WHERE scope_key LIKE 'listing:%' OR scope_key LIKE 'phase:%'
    OR scope_key LIKE 'record:%' OR scope_key LIKE 'config:%'
 ORDER BY changed_at_utc DESC;
```

- `listing:<table>`: a list or a walk of that table stopped part way
  (a request failed). For a walked table the next sync carries on from
  where it stopped. Cleared by the next sync that ends without stopping
  early.
- `phase:credential`, `phase:rate_limit`, `phase:gave_up`: the sync
  stopped early (the sign-in was refused, Linear refused for its rate
  limit without saying how long to wait, or Linear kept failing). What
  it fetched is kept. Cleared by the next sync that ends without
  stopping early.
- `record:issues:<issue id>`: that issue is stored, but its history
  past the first 50 entries did not all come. This row is never
  cleared on its own; whether it still holds, look at the issue's
  `issue_history` rows.
- `config:teams:<name>`, `config:projects:<name>`: a name in the
  config matched nothing in the workspace, so it selected nothing.

**2. What is still unread.** Linear's download keeps no list of what
upstream has; what it records is which stretches of `updatedAt` each
walked table has been read over, in `coverage`. A table is read down
to its oldest record when its scope has one row whose `lo` is empty:

```sql
SELECT scope, count(*) AS stretches,
       min(lo) = '' AS reaches_oldest,
       max(hi) AS newest_seen
  FROM coverage
 GROUP BY scope;
```

A scope with `stretches` above 1, or `reaches_oldest` 0, has a stretch
of older changes not yet read; the next sync reads it after what is
new. Rows under a selection key the config no longer uses (`all`
beside a hex key, or two hex keys) are from an earlier choice of teams
and projects, and are not read any more. The small lists have no
coverage: they are read whole every sync. Nothing in the store says
what changed upstream after the newest sync; how recent the copy is as
a whole is the `started_at_utc` of the newest `sync_runs` row whose
`status` is `ok`:

```sql
SELECT run_id, started_at_utc, status, summary FROM sync_runs ORDER BY run_id DESC LIMIT 5;
```

A sync that could not read the list of teams or projects stops before
reading anything else, and its row says `error`.

### `teams`

One row per team in the workspace, read whole every run.

| column | what it holds |
|---|---|
| `id` | Linear's team id |
| `payload` | the team: `name`, `key`, `displayName`, `description`, `timezone`, cycle and triage settings, `parent`, `defaultIssueState`, `triageIssueState` |

### `users`

One row per person in the workspace, deactivated ones included
(`includeDisabled: true`), read whole every run.

| column | what it holds |
|---|---|
| `id` | Linear's user id |
| `payload` | the person: `name`, `displayName`, `email`, `avatarUrl`, `title`, `timezone`, `active`, `admin`, `owner`, `guest`, `app`, `url`, `disableReason` |

### `workflow_states`

One row per workflow state (a column of a team's board), read whole
every run.

| column | what it holds |
|---|---|
| `id` | Linear's state id |
| `payload` | the state: `name`, `type` (`triage`, `backlog`, `unstarted`, `started`, `completed`, `canceled`), `color`, `description`, `position`, `team` |

### `issue_labels`

One row per label, workspace-wide and per team, read whole every run.

| column | what it holds |
|---|---|
| `id` | Linear's label id |
| `payload` | the label: `name`, `description`, `color`, `isGroup`, `team` (empty for a workspace label), `parent` (its group), `creator` |

### `project_statuses`

One row per project status the workspace defines, read whole every run.

| column | what it holds |
|---|---|
| `id` | Linear's project status id |
| `payload` | the status: `name`, `type`, `color`, `description`, `position`, `indefinite` |

### `cycles`

One row per cycle, read whole every run; with `teams` or `projects`
set, only the chosen teams' cycles.

| column | what it holds |
|---|---|
| `id` | Linear's cycle id |
| `payload` | the cycle: `number`, `name`, `description`, `startsAt`, `endsAt`, `completedAt`, `autoArchivedAt`, `team` |

### `projects`

One row per project, read whole every run; with `teams` or `projects`
set, the named projects and every project a chosen team works on.

| column | what it holds |
|---|---|
| `id` | Linear's project id |
| `payload` | the project: `name`, `description`, `content` (its markdown), `slugId`, `url`, `priority`, `health`, dates (`startDate`, `targetDate`, `startedAt`, `completedAt`, `canceledAt`), `trashed`, `labelIds`, `status`, `creator`, `lead`, `convertedFromIssue`, `teams.nodes` (up to 50), `members.nodes` (up to 100) |

### `project_milestones`

One row per project milestone, read whole every run; only milestones of
kept projects.

| column | what it holds |
|---|---|
| `id` | Linear's milestone id |
| `payload` | the milestone: `name`, `description`, `targetDate`, `status`, `project` |

### `initiatives`

One row per initiative, read whole every run, whatever the selection.

| column | what it holds |
|---|---|
| `id` | Linear's initiative id |
| `payload` | the initiative: `name`, `description`, `content`, `slugId`, `url`, `status`, `priority`, `targetDate`, `startedAt`, `completedAt`, `health`, `trashed`, `labelIds`, `owner`, `creator`, `parentInitiative`, `projects.nodes` (up to 100) |

### `issues`

One row per issue, read newest first by `updatedAt`.

| column | what it holds |
|---|---|
| `id` | Linear's issue id (a uuid) |
| `payload` | the issue: `identifier` (`ENG-123`), `number`, `title`, `description` (markdown), `priority`, `estimate`, the lifecycle stamps (`createdAt`, `startedAt`, `completedAt`, `canceledAt`, `dueDate`, the SLA stamps…), `trashed`, `labelIds`, `previousIdentifiers`, `reactionData`, `url`, `branchName`, references (`team`, `cycle`, `project`, `projectMilestone`, `creator`, `assignee`, `delegate`, `state`, `parent`, `sourceComment`, `botActor`…), and `relations.nodes` (up to 50, each `id`, `type`, `relatedIssue`). Its history is not here but in `issue_history` |

### `issue_history`

One row per entry of an issue's history: one change to the issue.

| column | what it holds |
|---|---|
| `id` | Linear's history entry id |
| `payload` | the entry: `createdAt`, `actorId`, `issue`, and the `from…`/`to…` pair of whatever changed (`fromStateId`/`toStateId`, `fromAssigneeId`/`toAssigneeId`, `fromTitle`/`toTitle`, `fromPriority`, `fromProjectId`, `fromCycleId`, `fromDueDate`…), `addedLabelIds`, `removedLabelIds`, `relationChanges`, `archived`, `trashed`, `autoClosed`, `autoArchived`, `botActor` |

### `comments`

One row per comment, read newest first by `updatedAt`.

| column | what it holds |
|---|---|
| `id` | Linear's comment id |
| `payload` | the comment: `body` (markdown), `createdAt`, `editedAt`, `resolvedAt`, `quotedText`, `reactionData`, `url`, what it is on (`issueId`, `projectUpdateId`, `initiativeUpdateId`, `projectId`, `initiativeId`, `documentContentId`), `parentId`, `resolvingCommentId`, `user`, `externalUser`, `resolvingUser`, `onBehalfOf`, `botActor` |

### `documents`

One row per document, read newest first by `updatedAt`.

| column | what it holds |
|---|---|
| `id` | Linear's document id |
| `payload` | the document: `title`, `content` (markdown), `documentContentId`, `slugId`, `url`, `icon`, `color`, `hiddenAt`, `trashed`, `creator`, `owner`, `updatedBy`, and what it belongs to (`project`, `initiative`, `issue`, `release`) |

### `project_updates`

One row per project update (a status post on a project), read newest
first by `updatedAt`.

| column | what it holds |
|---|---|
| `id` | Linear's project update id |
| `payload` | the update: `body` (markdown), `editedAt`, `slugId`, `shortSummary`, `health`, `isDiffHidden`, `diffMarkdown`, `reactionData`, `url`, `project`, `user` |

### `attachments`

One row per attachment: a link or integration record on an issue (a
pull request, a Slack thread, a URL), read newest first by
`updatedAt`. Not the file bytes of an upload.

| column | what it holds |
|---|---|
| `id` | Linear's attachment id |
| `payload` | the attachment: `title`, `subtitle`, `url`, `metadata`, `source`, `sourceType`, `groupBySource`, `creator`, `externalUserCreator`, `originalIssue`, `issue` (its `id` only) |

### `coverage`

The stretches of `updatedAt` each walked table has been read over, for
one selection. Everything Linear listed in a stretch is stored.

| column | what it holds |
|---|---|
| `scope` | `<table>:<selection>`: the table is `issues`, `comments`, `documents`, `project_updates` or `attachments`; the selection is `all` for the whole workspace, otherwise 16 hex characters naming the chosen teams and projects |
| `lo` | the oldest `updatedAt` in the stretch, as Linear writes it (`2026-10-21T05:38:45.862Z`); empty when the walk reached the oldest record |
| `hi` | the newest `updatedAt` the walk saw; not when it last looked |

## LinkedIn (`linkedin`)

The LinkedIn store has **no tables of its own** in its fixed shape:
besides the tables every raw store has (`problems`, `sync_runs`,
`sync_scope_state`), its tables are made while the export is read, one
for each CSV file the export holds, plus `articles`. So a store holds
only the tables of the files its exports had, and `.tables` lists them:

```sh
datalib-doltlite -readonly linkedin/ingest/entities.doltlite_db ".tables"
```

```sql
SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name;
```

A table's name is the file's path inside the export, lowercased, with
the extension dropped, every run of characters that is not a letter or
digit made one `_`, and a trailing `_<digits>` dropped (LinkedIn adds
the member's number to some names): `Connections.csv` → `connections`,
`Comments_12345678.csv` → `comments`, `Jobs/Job Applications.csv` →
`jobs_job_applications`, `Receipts_v2.csv` → `receipts_v2`. A name that
would start with a digit gets `t_` in front.

Every such table has the same two columns, and no `_bookkeeping`
sidecar:

| column | what it holds |
|---|---|
| id | the row's key: the natural-key column(s) named below, joined, when the row has them; otherwise a uuid made from the table name and the row's contents |
| payload | the CSV row as JSONB: one key per header, spelled as the header is (`"CONVERSATION ID"`, `"First Name"`), every value a string with spaces trimmed. A header repeated is `"<name> (2)"`, a blank one `column_<n>` |

`articles` is the exception to the CSV shape: one row per `.html` file
under a folder named `Articles`, `id` its path inside the export and
`payload` `{ "file": <that path>, "html": <the file's contents> }`.

**Which id is which**

| upstream | where |
|---|---|
| a connection's profile URL (`URL` in `Connections.csv`) | `connections.id` |
| a share's link (`ShareLink`) | `shares.id` |
| a skill's name | `skills.id` |
| an address (`Email Address`) | `email_addresses.id` |
| a conversation (`CONVERSATION ID`) | the payload's `"CONVERSATION ID"` in `messages` (and the other message tables); not an id of any row |
| a message, a comment, a profile row | none: `id` is a hash of the row |

The tables keyed on a column, and the column: `company_follows`
(`Organization`), `connections` (`URL`), `email_addresses` (`Email
Address`), `instantreposts` (`Link`), `invitations` (`inviterProfileUrl`,
`inviteeProfileUrl` and `Sent At`, joined by the character U+001F),
`jobs_job_applications` and `jobs_saved_jobs` (`Job Url`), `reactions`
(`Link`), `receipts_v2` (`Invoice Number`), `rich_media` (`Media Link`),
`saved_items` (`savedItem`), `shares` (`ShareLink`), `skills` (`Name`).
Every other table, and a row whose key column is empty, is keyed by the
hash.

**Telling "me"**: the export names its owner only in `email_addresses`
(the row whose `Primary` is `Yes`) and `profile`. No message row says
which sender is the owner.

The queries below name tables that exist only once an export with that
file has been read, so they are written as shell commands:

One conversation's messages, oldest first:

```sh
datalib-doltlite -readonly linkedin/ingest/entities.doltlite_db \
  "SELECT payload ->> '\$.DATE' AS date, payload ->> '\$.FROM' AS sender,
          payload ->> '\$.CONTENT' AS text
     FROM messages
    WHERE payload ->> '\$.\"CONVERSATION ID\"' = '2-example-conversation'
    ORDER BY date"
```

The owner's primary address:

```sh
datalib-doltlite -readonly linkedin/ingest/entities.doltlite_db \
  "SELECT id FROM email_addresses WHERE payload ->> '\$.Primary' = 'Yes'"
```

Connections, with the date LinkedIn gives as text (for example "16 Jun 2026"), which does not sort as a date:

```sh
datalib-doltlite -readonly linkedin/ingest/entities.doltlite_db \
  "SELECT id AS profile, payload ->> '\$.\"First Name\"' AS first,
          payload ->> '\$.\"Last Name\"' AS last, payload ->> '\$.\"Connected On\"' AS connected
     FROM connections"
```

The tables a complete export makes, and what one row of each is:

| table | one row is |
|---|---|
| `ad_targeting` | ad-targeting attributes LinkedIn inferred about you |
| `ads_clicked` | an ad you clicked |
| `articles` | an article you published, as HTML |
| `comments` | a comment you left on a post (`Date`, `Link` to the post, `Message`) |
| `company_follows` | a company you follow |
| `connections` | a first-degree connection |
| `education` | a school on your profile |
| `email_addresses` | an address on your account, with `Primary` and confirmed flags |
| `endorsement_given_info`, `endorsement_received_info` | an endorsement you gave, or got |
| `guide_messages`, `learning_coach_messages`, `learningcoachmessages`, `learning_role_play_messages` | a message in a LinkedIn AI-assistant conversation, in the `messages` shape |
| `inferences_about_you` | an attribute LinkedIn inferred about you |
| `instantreposts` | a post you reposted |
| `invitations` | a connection invitation sent or received |
| `job_applicant_saved_screening_question_responses`, `jobs_job_applicant_saved_answers` | a saved answer to a job application's question |
| `jobs_job_applications` | a job you applied to |
| `jobs_job_seeker_preferences` | your job-seeking preferences |
| `jobs_saved_jobs` | a job you saved |
| `lan_ads_engagement` | a LinkedIn Audience Network ad event |
| `languages`, `patents`, `publications`, `positions`, `volunteering` | an entry of that section of your profile |
| `learning` | a LinkedIn Learning course activity |
| `logins` | a sign-in to your account |
| `member_follows` | a member you follow |
| `messages` | a direct message: `CONVERSATION ID`, `CONVERSATION TITLE`, `FROM`, `SENDER PROFILE URL`, `TO`, `RECIPIENT PROFILE URLS`, `DATE`, `SUBJECT`, `CONTENT`, `FOLDER`, `ATTACHMENTS` |
| `phonenumbers` | a phone number on your account |
| `profile` | your profile (one row) |
| `profile_summary` | your profile's counters |
| `reactions` | a reaction you gave a post |
| `receipts_v2` | a purchase receipt |
| `recommendations_given`, `recommendations_received` | a recommendation |
| `registration` | your account's registration |
| `rich_media` | an image or link attached to your content |
| `saved_items` | an item you saved |
| `savedjobalerts` | a saved job alert |
| `searchqueries` | a search you ran |
| `shares` | a post you shared (`Date`, `ShareLink`, `ShareCommentary`, `SharedUrl`, `MediaUrl`, visibility) |
| `skills` | a skill on your profile |
| `verifications_verifications` | a verification on your account |
| `whatsapp_phone_numbers` | a WhatsApp number linked to your account |

A CSV not in this list (LinkedIn adds files) still gets a table, named
the same way and keyed by the hash. Of all these, render reads only the
message tables, `shares`, `comments`, `connections`, `skills`,
`email_addresses` and `profile`.

### Is the copy complete?

LinkedIn keeps no record of anything owed: each sync reads the whole
export folder, so the copy is complete when every file in it read.

1. **Files that did not read.** A CSV or article that would not read, or
   a part of the folder that could not be listed, is a `listing:` row in
   `problems`; its table still holds the last good read. Any row here
   means the copy is behind the export on disk:

   ```sql
   SELECT scope_key, severity, sample, changed_at_utc
     FROM problems
    WHERE scope_kind = 'entity' AND scope_key LIKE 'listing:%'
    ORDER BY scope_key;
   ```

   `listing:csv <file>` is a CSV, `listing:articles <file>` an article,
   `listing:files` a folder or link the walk could not read (and while
   it is there no article is deleted). A row clears at the next sync
   that reads the file.

2. **When the export was last read.** The download does not write
   `sync_runs`, so that table stays empty. Each sync that changed the
   store makes a commit whose message is `download <source id>:
   files=<n> rows=<n> parse_errors=<n>`; `parse_errors` greater than 0
   means some file did not read:

   ```sh
   datalib-doltlite -readonly linkedin/ingest/entities.doltlite_db \
     "SELECT date, message FROM dolt_log() WHERE message LIKE 'download %' LIMIT 5"
   ```

   A sync that found nothing changed makes no commit, so the newest
   commit is when the copy last changed, not when it was last read.

3. **What the export itself left out.** Whether a table is missing
   because the export never had that file cannot be told from the store:
   a file the export lacks leaves no trace. Compare `.tables` with the
   list in "Reading the raw store", and with the files in `export.path`.

Render's own notes (the "Stored and not searchable" count of tables no
feed reads, a date that would not parse, a skill row with no name) are
in the render step's store, not here.

## Markdown (`markdown`)

The download stores each note's text as it read it, not a service's
JSON, so its tables have no `payload` column. Nothing is read from disk
at render time.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a note's path under the folder (`Bridge/Warp core.md`) | `markdown_files.id`; `ingested_files.rel_path` under scope `markdown/files`; the grid row's `upstream_id` |
| the folder that was scanned | `markdown_scan_meta.abs_root` |
| the source's id | `markdown_scan_meta.id` |

The newest notes:

```sql
SELECT id, size, modified_at FROM markdown_files
 ORDER BY modified_at DESC LIMIT 20;
```

A note's absolute path on disk:

```sql
SELECT s.abs_root || '/' || f.id AS path
  FROM markdown_files f, markdown_scan_meta s
 WHERE f.id = :path;
```

The notes in one folder:

```sql
SELECT id FROM markdown_files WHERE id LIKE 'Bridge/%' ORDER BY id;
```

### Is the copy complete?

Markdown keeps no record of what is owed and writes no `sync_runs` rows.
Every sync compares the folder with what the store holds, so after a
sync with no problems the store matches the folder. Read `problems`:

```sql
SELECT scope_key, severity, outcome, sample
  FROM problems
 WHERE scope_key = 'listing:files'
    OR scope_key LIKE 'record:files:%'
    OR scope_key LIKE 'file:markdown/files:%';
```

- `listing:files`: a folder would not list. That scan deleted nothing,
  so notes deleted from the folder may still be in the copy.
- `record:files:<path>`: a file that would not open (severity `error`;
  its note is missing or out of date, and every sync tries it again), or
  a file over `max_bytes` (severity `warning`, reason
  `over_size_limit`; it keeps whatever text an earlier sync stored).
- `file:markdown/files:<path>`: a file that is not UTF-8, stored with
  replaced bytes (outcome `nulled`). It stays until the file changes.

Every stored note has a stamp in `ingested_files`. A note with no stamp
would mean a write that did not finish:

```sql
SELECT f.id FROM markdown_files f
  LEFT JOIN ingested_files i ON i.scope = 'markdown/files' AND i.rel_path = f.id
 WHERE i.rel_path IS NULL;
```

### `markdown_files`

One row per note: a markdown file under the folder, keyed by its path.
Editing the file rewrites the row; moving it adds a row at the new path
and deletes the old one.

| column | what it holds |
|---|---|
| `id` | the file's path under the folder, slash-separated |
| `blake3` | the hash of the bytes read, lowercase hex |
| `size` | the size of the bytes read |
| `modified_at` | the file's modification time, to the second, ending `Z` (a file system names no time zone) |
| `text` | the whole file, as UTF-8; a byte that was not UTF-8 is U+FFFD |

### `markdown_scan_meta`

One row, keyed by the source's id: where the scan ran. Render builds each
note's `file://` link from it.

| column | what it holds |
|---|---|
| `id` | the source's id |
| `abs_root` | the folder as the scan resolved it, an absolute path |

## Media (`media`)

`media` keeps no upstream payload and no copy of any file: there is no `payload` column and no `blobs.sqlite`. Each scan walks the folder `fswalk.path` names, reads each audio, image, video or playlist file it has not seen before, and records what it is. The container is decided from the file's first bytes, not its name.

Files are keyed by content: a file's bytes are hashed with blake3, and the item behind it is one `media_items` row however many paths hold it. At the end of each scan, the path rows of files no longer in the folder are deleted, then the items no path names; a scan whose walk could not read part of the folder deletes nothing. The tables hold the folder as of the last complete scan; earlier states are earlier commits.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a file's path under the scan root | `media_files.id` |
| the blake3 of a file's bytes, lowercase hex | `media_files.blake3`, `media_items.blake3`, `media_audio.blake3`, `media_visual.blake3` |
| a hash of the file's audio or image data without its tags | `media_items.payload_blake3`, under `media_items.payload_scheme` |
| a playlist's path under the scan root | `media_playlists.id`, `media_playlist_entries.playlist_id` |
| one line of a playlist | `media_playlist_entries.id`, `<playlist path>#<position>` |
| the scan root's absolute path | `media_scan_meta.abs_root` |
| the source's id (its group) | `media_scan_meta.id` |

**What is in the library:**

```sql
SELECT media_class, container, count(*) AS items, sum(size) AS bytes
  FROM media_items
 GROUP BY media_class, container
 ORDER BY items DESC;
```

**Music by artist and album**, one row per file:

```sql
SELECT f.id, a.artist, a.album, a.track_no, a.title, i.duration_ms
  FROM media_files f
  JOIN media_items i ON i.blake3 = f.blake3
  LEFT JOIN media_audio a ON a.blake3 = f.blake3
 WHERE i.media_class = 'audio'
 ORDER BY a.artist, a.album, a.disc_no, a.track_no;
```

**Photos and videos with a location**, in capture order:

```sql
SELECT f.id, v.captured_at, v.camera_model, v.gps_lat, v.gps_lon
  FROM media_visual v JOIN media_files f ON f.blake3 = v.blake3
 WHERE v.gps_lat IS NOT NULL
 ORDER BY v.captured_at;
```

**One recording under several byte versions** (retagged, or with a re-rendered preview):

```sql
SELECT i.payload_scheme, i.payload_blake3, count(DISTINCT i.blake3) AS versions,
       group_concat(f.id, ' | ') AS paths
  FROM media_items i JOIN media_files f ON f.blake3 = i.blake3
 WHERE i.payload_blake3 IS NOT NULL
 GROUP BY i.payload_scheme, i.payload_blake3
HAVING count(DISTINCT i.blake3) > 1;
```

**How much of each playlist is still on disk:**

```sql
SELECT p.id, p.title, p.entry_count,
       sum(f.blake3 IS NOT NULL) AS present,
       p.entry_count - sum(f.blake3 IS NOT NULL) AS missing
  FROM media_playlists p
  JOIN media_playlist_entries e ON e.playlist_id = p.id
  LEFT JOIN media_files f ON f.id = e.resolved_path
 GROUP BY p.id
 ORDER BY missing DESC;
```

### Is the copy complete?

`media` keeps no record of anything owed: every scan walks the whole folder. What a scan could not do is in `problems`, and each row is replaced by the next scan's verdict:

```sql
SELECT scope_key, severity, sample
  FROM problems
 WHERE scope_key LIKE 'record:%' OR scope_key LIKE 'listing:%'
    OR scope_key LIKE 'skipped:%'
 ORDER BY scope_key;
```

- `listing:files`: the walk could not read part of the folder. That scan deleted nothing, so files deleted since may still be in the copy.
- `record:files:<path>`: a file that could not be opened to hash it (severity `error`), or one over `max_bytes` and not read (severity `warning`).
- `record:media_files:<path>`: a file hashed but whose item could not be read. It has no `media_files` row unless an earlier scan wrote one, and is tried again every scan.
- `record:media_playlists:<path>`: a playlist that would not read (`error`) or is over 16 MiB (`warning`); it keeps the row an earlier scan wrote.
- `skipped:cloud_placeholders:<hash>`, severity `warning`: one row for all the online-only files not read; the sample gives the count and names one.

`sync_runs` stays empty for this source. A scan commits once, at its end, and only when something changed; the newest commit (`SELECT date, message FROM dolt_log() LIMIT 1`) has the message `download <source id>: …` followed by the scan's counts, `dataless_skipped=…`, `removed=…` and `errors=…` among them.

### `media_items`

One row per distinct file content (by blake3) that some path in the folder holds and that could be read.

| column | what it holds |
|---|---|
| `blake3` | the blake3 hash of the file's bytes, lowercase hex; the item's id |
| `size` | the file's size in bytes |
| `media_class` | `audio`, `image` or `video`, from the container where it decides, else from the extension |
| `container` | the format the first bytes show: `mp3`, `flac`, `wav`, `avi`, `bmff` (MP4, M4A, MOV, HEIC, AVIF), `jpeg`, `png`, `tiff` (DNG and other raw files built on TIFF), `gif`, `webp`, `aiff`, `ogg`, `matroska`, or `unknown` |
| `codec` | a short name for the main stream's codec, else the container's name |
| `duration_ms` | the length in milliseconds, for audio and video, when known |
| `payload_blake3` | a blake3 over the file's audio or image data with tags, EXIF, XMP, ICC profiles and embedded previews left out, lowercase hex; NULL where the format has no recipe, the file could not be parsed, or it is over `payload_max_bytes` |
| `payload_scheme` | which recipe made `payload_blake3` (`mp3.frames.v1`, `jpeg.scan.v1`, `bmff.samples.v1`, …); NULL with it |

### `media_audio`

One row per item whose tags were read: every audio file, and a video or image only when it carries real tags.

| column | what it holds |
|---|---|
| `blake3` | the item, `media_items.blake3` |
| `title` | the title tag |
| `artist` | the artist tag |
| `album` | the album tag |
| `album_artist` | the album artist tag |
| `composer` | the composer tag |
| `genre` | the genre tag |
| `date` | the recording, year or release date as tagged, free text (`1997`, `1997-08-25`, `08/25/1997`) |
| `track_no` | the track number |
| `track_total` | the number of tracks |
| `disc_no` | the disc number |
| `disc_total` | the number of discs |
| `bitrate_kbps` | the audio bitrate, kbit/s |
| `sample_rate_hz` | the sample rate, Hz |
| `channels` | the number of channels |
| `bit_depth` | bits per sample, where the format has one |

### `media_visual`

One row per item for which any capture metadata, dimensions or video codec was found: images and videos, and an audio file only if it carries capture fields.

| column | what it holds |
|---|---|
| `blake3` | the item, `media_items.blake3` |
| `width` | the width in pixels of the full image (for a DNG, its crop size, not the preview's) |
| `height` | the height in pixels, the same way |
| `orientation` | the EXIF orientation, 1 to 8, not applied to `width` and `height` |
| `captured_at` | when it was taken, `YYYY-MM-DDTHH:MM:SS`, with an offset only when the file gave one |
| `camera_make` | the EXIF camera maker |
| `camera_model` | the EXIF camera model |
| `lens_model` | the EXIF lens model |
| `iso` | the EXIF ISO speed |
| `exposure_time` | the EXIF exposure time as written (`1/250`) |
| `f_number` | the EXIF f-number |
| `focal_length_mm` | the EXIF focal length, mm |
| `gps_lat` | latitude in decimal degrees, negative for south |
| `gps_lon` | longitude in decimal degrees, negative for west |
| `gps_altitude_m` | altitude in metres |
| `title` | the EXIF `ImageDescription` |
| `caption` | the EXIF `UserComment` |
| `frame_rate` | a video's frames per second |
| `video_codec` | a video's video codec |
| `audio_codec` | a video's audio codec |

### `media_files`

One row per media file in the folder whose item is in `media_items`, as of the last complete scan (a file the last scan found and did not read keeps the row an earlier scan wrote).

| column | what it holds |
|---|---|
| `id` | its path relative to the scan root, `/`-separated |
| `blake3` | the hash of its bytes when the scan read it, joining to `media_items.blake3` |

### `media_playlists`

One row per `.m3u` or `.m3u8` playlist in the folder, keyed by path: two identical playlists in two folders are two rows.

| column | what it holds |
|---|---|
| `id` | its path relative to the scan root |
| `blake3` | the blake3 hash of the playlist file's bytes |
| `format` | `m3u` or `m3u8`, from the extension |
| `title` | the `#PLAYLIST:` line, when there is one |
| `entry_count` | how many entries it has |

### `media_playlist_entries`

One row per entry line of a playlist, in order, duplicates and broken targets included.

| column | what it holds |
|---|---|
| `id` | `<playlist path>#<position>` |
| `playlist_id` | the playlist, `media_playlists.id` |
| `position` | the entry's place in the playlist, counting from 0 |
| `target_raw` | the line as written |
| `target_kind` | `relative`, `absolute` (a leading `/`, a drive letter or a UNC path) or `url` (a scheme such as `http:` or `file:`) |
| `resolved_path` | for a `relative` entry that stays inside the scanned folder, the path under the scan root it names, with `\` read as `/`; NULL otherwise. Join it to `media_files.id` to see whether the file is there |
| `ext_title` | the title from the `#EXTINF` line before it |
| `ext_duration_s` | the duration in whole seconds from that `#EXTINF` line; NULL when it was `-1`, fractional or absent |

### `media_scan_meta`

One row: where this source scans.

| column | what it holds |
|---|---|
| `id` | the source's id |
| `abs_root` | the scan root, as the config names it with `~` expanded; symbolic links in it are not resolved |

`media_items_bookkeeping.fetched_at_utc` is when an item was first seen (its `id` is the item's `blake3`); `media_files_bookkeeping` and `media_playlists_bookkeeping` do the same for paths. Later scans do not change them.

## Notion (`notion`)

Notion is read through its REST API at `Notion-Version: 2026-03-11`.
`pages.payload` is the page object from `POST /v1/search` or
`GET /v1/pages/{id}`, properties included. `comments.payload` is one
comment from `GET /v1/comments`. `users.payload` is a user from
`GET /v1/users/{id}`, or the user object a people property carried.
A page's body is not a payload: it is the text of
`GET /v1/pages/{id}/markdown` (Notion's own "enhanced" markdown, with
tags such as `<callout>`, `<columns>`, `<details>` and `<table>`), in
`page_markdown.markdown`. Every Notion file link in a stored body has
had its signature removed (`https://…/file.pdf` without its
`X-Amz-…` query), since Notion signs links afresh on every fetch; that
unsigned link is also the attachment's `ref_id`. `pages.payload` is not
rewritten, so a cover or icon Notion hosts there is a signed link that
has long expired.

**Which id is which.** Every Notion id is a dashed uuid.

| you have | it is in the mirror as |
|---|---|
| a page id (the 32 hex digits at the end of a page's link) | `pages.id`, `page_markdown.id`, `page_comments.id`, `tree.id`, `listed_pages.id`; `page_id` on `comments`, `comment_anchors`, `notion_attachments` |
| a database id | `tree.id` where `kind = 'database'`; `pages.parent_id` of its rows |
| a data source id | not a column: a row's `payload ->> '$.parent.data_source_id'` |
| a block id | `tree.id` where `kind = 'block'` (a toggle or column a page sits in); `comments.parent_id` where `parent_type = 'block_id'`; `comment_anchors.id` |
| a discussion id | `comments.discussion_id` |
| a comment id | `comments.id` |
| a user id | `users.id`; `$.created_by.id` and `$.last_edited_by.id` in a page's or a comment's payload |
| a file in a page's body | `notion_attachments.ref_id` (its unsigned link); the row's `id` is `<page id>#<ref_id>` |

**"Me".** The store does not record which user the credential belongs
to.

Pages with their titles and where they sit:

```sql
SELECT p.id, t.title, p.parent_type, p.parent_id, p.last_edited_time, p.url, p.in_trash
  FROM pages p LEFT JOIN tree t ON t.id = p.id
 WHERE p.payload IS NOT NULL
 ORDER BY p.last_edited_time DESC;
```

The rows of one database, by its title:

```sql
SELECT p.id, t.title
  FROM pages p
  JOIN tree d ON d.id = p.parent_id AND d.kind = 'database'
  LEFT JOIN tree t ON t.id = p.id
 WHERE d.title = 'Crew manifest';
```

A row's properties, with their types:

```sql
SELECT pr.key AS property, json_extract(pr.value, '$.type') AS type, json(pr.value) AS value
  FROM pages p, json_each(p.payload, '$.properties') pr
 WHERE p.id = :page_id;
```

A page's discussions, in order:

```sql
SELECT c.discussion_id, c.created_time,
       json_extract(c.payload, '$.display_name.resolved_name') AS author,
       (SELECT group_concat(json_extract(r.value, '$.plain_text'), '')
          FROM json_each(c.payload, '$.rich_text') r) AS said,
       a.plain_text AS on_text
  FROM comments c
  LEFT JOIN comment_anchors a ON a.id = c.parent_id AND c.parent_type = 'block_id'
 WHERE c.page_id = :page_id
 ORDER BY c.discussion_id, c.created_time;
```

**A page's body can have holes.** Where Notion left part of a body out,
the body holds a marker such as `<unknown url="…#<block id>"/>`, and
`page_markdown.unresolved_block_ids` lists the ids still missing.
Datalib fills what it can in place; read `unresolved_block_ids` as well
as the text, since an id can be listed with no marker in the body.

**The trash.** `pages.in_trash` is whether Notion's trash holds the page
now, as the last complete trash listing found. The payload's `in_trash`
is the page as Notion last returned it. They differ for a page deleted
for good: `in_trash` is 0 and the payload says `true`.

**A `pages` row with a NULL `payload`** is a page whose object never
arrived: the row exists to carry its sidecar's error. A `users`,
`tree` or `comment_anchors` row with only its id is one Notion
answered 403 or 404 for; it is kept so it is not asked for again.

**Versions.** `pages_bookkeeping.held_version` is the page's
`last_edited_time`. `page_markdown_bookkeeping.held_version` and
`page_comments_bookkeeping.held_version` are the page's
`last_edited_time` the body and the comments were read for. The other
sidecars hold no version: their record is read once.

Nothing is set aside in `volatile_payload` for this source.

### Is the copy complete?

1. **Problems.** Under these keys something is missing or stale:

   - `pages:<id>`: a page object that did not fetch (roots mode), or a
     page in the trash that did not answer when asked (a warning);
   - `page_markdown:<id>`: a body that did not fetch (an error), or one
     stored with something lost: too many holes, a block Notion will not
     show (a warning);
   - `page_comments:<id>`: a page's comments that did not list;
   - `notion_attachments:<page id>#<ref_id>`: a file that did not
     download, or one over 1 GiB (a warning);
   - `comment_anchors:<id>`, `tree:<id>`, `users:<id>`: a commented
     block, a parent, or a user that did not fetch;
   - `listing:search`, `listing:roots`, `listing:trash`: a listing cut
     short; `listing:comments` (a warning): the credential may not read
     comments;
   - `phase:rate_limit`, `phase:credential`: the sync stopped early and
     the rest is owed; `phase:<table>`: one loop gave up after 25
     failures in a row;
   - `config:roots:<value>`: a configured root Notion has not got or
     will not show.

   ```sql
   SELECT scope_key, severity, reason, sample FROM problems ORDER BY severity, scope_key;
   ```

2. **Is the listing whole?** One `coverage` span starting at `''`:

   ```sql
   SELECT scope, lo, hi FROM coverage ORDER BY scope, lo;
   ```

3. **What is still owed.** Every loop keeps what it holds in its
   sidecar, so what is owed is a query:

   ```sql
   -- bodies not held at the page's edit time
   SELECT count(*) AS bodies_owed FROM pages p
     LEFT JOIN page_markdown_bookkeeping b ON b.id = p.id
    WHERE p.payload IS NOT NULL
      AND (b.fetched_at_utc IS NULL OR b.held_version IS NOT p.last_edited_time);

   -- comment listings not held at the page's edit time (when `comments` is on)
   SELECT count(*) AS comment_listings_owed FROM pages p
     LEFT JOIN page_comments_bookkeeping b ON b.id = p.id
    WHERE p.payload IS NOT NULL
      AND (b.fetched_at_utc IS NULL OR b.held_version IS NOT p.last_edited_time);

   -- files with no bytes yet (when `attachments` is on)
   SELECT count(*) AS files_owed FROM notion_attachments a
     LEFT JOIN notion_attachments_bookkeeping b ON b.id = a.id
    WHERE b.fetched_at_utc IS NULL;

   -- parents a page sits under that are not placed yet
   SELECT count(DISTINCT t.parent_id) AS parents_owed FROM tree t
     LEFT JOIN tree_bookkeeping b ON b.id = t.parent_id
    WHERE t.parent_type IN ('page_id', 'block_id', 'data_source_id', 'database_id')
      AND t.parent_id IS NOT NULL AND b.fetched_at_utc IS NULL;
   ```

   In roots mode, a page already mirrored whose listed stamp has moved
   is owed its object:

   ```sql
   SELECT l.id FROM listed_pages l
     JOIN tree t ON t.id = l.id AND t.kind = 'page'
     LEFT JOIN pages_bookkeeping b ON b.id = l.id
    WHERE b.fetched_at_utc IS NULL OR b.held_version IS NOT l.last_edited_time;
   ```

   A page under a root not yet mirrored at all cannot be told apart in
   SQL from a page outside the roots, since `listed_pages` holds both;
   whether a page is under a root takes walking `tree` up to a root.

4. **The last run.** The newest `sync_runs` row's `status`: `ok` when it
   reached everything; `partial` when it stopped early.

   ```sql
   SELECT started_at_utc, status, summary FROM sync_runs ORDER BY run_id DESC LIMIT 1;
   ```

   None of this shows a page that became visible to the credential with
   an old edit time (see Limitations), or a comment on a page whose edit
   time did not move.

### `pages`

One row per page the store mirrors, database rows included.

| column | what it holds |
|---|---|
| `id` | the page id |
| `parent_type` | Notion's parent type: `workspace`, `page_id`, `block_id`, `database_id` or `data_source_id` |
| `parent_id` | the parent's id: a page, a block, or for a database row its database (not its data source); `workspace` for a page at the top |
| `in_trash` | 1 while Notion's trash holds the page, as the last complete trash listing found |
| `created_time` | Notion's `created_time` |
| `last_edited_time` | Notion's `last_edited_time`, to the minute |
| `url` | the page's link on notion.so |
| `payload` | the page object, properties included (JSONB); NULL for a page whose object never arrived |

### `page_markdown`

One row per page whose body has been read.

| column | what it holds |
|---|---|
| `id` | the page id |
| `markdown` | the body as Notion's markdown, file links unsigned; empty for a page with no body |
| `truncated` | 1 if Notion's answer said it was truncated |
| `unresolved_block_ids` | a JSON array of the block ids still missing from the body; NULL when none |

### `page_comments`

One row per page whose comments have been listed. The row is only the
id; the version it was listed at is in its sidecar.

| column | what it holds |
|---|---|
| `id` | the page id |

### `comments`

One row per comment. A page's listing returns every discussion on it,
those on blocks inside it included; a comment the listing no longer
returns (resolved, or deleted) is deleted here.

| column | what it holds |
|---|---|
| `id` | the comment id |
| `discussion_id` | the thread it belongs to |
| `parent_type` | `page_id` or `block_id` |
| `parent_id` | the page or block it hangs off |
| `page_id` | the page it was listed for |
| `created_time` | Notion's `created_time` |
| `last_edited_time` | Notion's `last_edited_time` |
| `payload` | the comment object: `rich_text`, `created_by`, `display_name` and more (JSONB) |

### `comment_anchors`

One row per block a comment hangs off, read once.

| column | what it holds |
|---|---|
| `id` | the block id |
| `page_id` | a page that block's comments were listed for |
| `block_type` | the block's Notion type; NULL for a block Notion no longer has |
| `plain_text` | the block's text as it was when read; NULL for a block Notion no longer has |

### `users`

One row per user a page, a people property or a comment names, read once.

| column | what it holds |
|---|---|
| `id` | the user id |
| `name` | their name: from the user object, a people property, or a comment's `display_name`; NULL when nothing named them |
| `payload` | the user object (JSONB); NULL when Notion would not show it and no people property carried it |

### `tree`

One row per page, database and block a page sits under, and per page
itself. It is what the path above a page and the table of contents are
built from.

| column | what it holds |
|---|---|
| `id` | the page, database or block id |
| `kind` | `page` (mirrored here), `unmirrored_page` (seen in roots mode, not mirrored), `database` or `block` |
| `title` | the page's or database's title; NULL for a block |
| `parent_type` | as in `pages.parent_type` |
| `parent_id` | as in `pages.parent_id` |
| `created_time` | Notion's `created_time` |

### `listed_pages`

Roots mode only: every page the walk down from the roots or the search
listed, wherever it sits, at the edit time it was listed at. A page
under a root is owed its object when it is not held at this stamp.

| column | what it holds |
|---|---|
| `id` | the page id |
| `last_edited_time` | the `last_edited_time` it was listed at |

### `notion_attachments`

One row per file a page's body links, pointing at its bytes in `blobs.sqlite`.

| column | what it holds |
|---|---|
| `id` | `<page id>#<ref_id>` |
| `page_id` | the page whose body links it |
| `ref_id` | the file's unsigned link, as it appears in the stored body |
| `blake3` | the bytes' hash in `blobs.sqlite`; NULL until they are fetched |

### `coverage`

The stretch of edit times the search has read whole. One span from
`lo = ''` up to the newest edit seen means the whole workspace has been
listed; more than one span, or none starting at `''`, means a listing
was cut short and the next sync reads the gap.

| column | what it holds |
|---|---|
| `scope` | `search` with no roots configured, `roots` with roots |
| `lo` | the oldest edit time the span reaches, as Notion writes it (`2026-09-01T00:00:00.000Z`); `''` for the beginning of the workspace |
| `hi` | the newest edit time the span reaches |

## PDFs (`pdf`)

`pdf` keeps no upstream payload and no copy of any PDF: there is no `payload` column and no `blobs.sqlite`. Each scan walks the folder `fswalk.path` names, reads each `.pdf` file it has not seen before, and records what the file is: a classification of its pages, the metadata in its Info dictionary and XMP packet, and two hashes. Converting it to text is the render step's job, which reads the file from disk.

A document is named by the blake3 hash of its bytes, so identical copies share one `pdf_documents` row, and a moved file keeps its document. Each scan rebuilds `pdf_paths` from what it found, then deletes the `pdf_documents` rows no path names, so both tables hold the tree as of the last complete scan; earlier states are earlier commits. A scan whose walk could not read part of the tree rebuilds nothing: what it saw is written over what was there, and nothing is deleted.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a file's path under the scan root | `pdf_paths.id` |
| the blake3 of a file's bytes, lowercase hex | `pdf_paths.blake3`, `pdf_documents.blake3` |
| a hash of the document's content without its metadata | `pdf_documents.content_blake3` |
| the trailer `/ID` (its first half) | `pdf_documents.pdf_id_permanent` |
| the XMP `xmpMM:DocumentID` | `pdf_documents.xmp_document_id` |
| the scan root's absolute path | `pdf_scan_meta.abs_root` |
| the source's id (its group) | `pdf_scan_meta.id` |

**Every file, with what it is:**

```sql
SELECT p.id AS path, d.title, d.pdf_type, d.page_count, d.ocr_page_count,
       d.has_encoding_issues = 0 AND d.page_count > d.ocr_page_count AS renders
  FROM pdf_paths p JOIN pdf_documents d ON d.blake3 = p.blake3
 ORDER BY p.id;
```

**The documents that are not searchable**, because no page has text or a font does not decode:

```sql
SELECT d.blake3, MIN(p.id) AS path, d.pdf_type, d.page_count,
       d.ocr_page_count, d.has_encoding_issues
  FROM pdf_documents d JOIN pdf_paths p ON p.blake3 = d.blake3
 WHERE d.has_encoding_issues = 1 OR d.page_count <= d.ocr_page_count
 GROUP BY d.blake3
 ORDER BY path;
```

**One PDF in several places:**

```sql
SELECT blake3, count(*) AS copies, group_concat(id, ' | ') AS paths
  FROM pdf_paths
 GROUP BY blake3
HAVING count(*) > 1;
```

**Versions of one document whose metadata alone differs** (retitled, or a regenerated `/ID`). A tool that rewrites the file's objects (Acrobat "Save As", `qpdf --linearize`) changes `content_blake3` too, so this finds some versions, not all:

```sql
SELECT content_blake3, count(*) AS versions, group_concat(title, ' | ') AS titles
  FROM pdf_documents
 WHERE content_blake3 IS NOT NULL
 GROUP BY content_blake3
HAVING count(*) > 1;
```

### Is the copy complete?

`pdf` keeps no record of anything owed: every scan walks the whole tree. What a scan could not do is in `problems`, and each row is replaced by the next scan's verdict:

```sql
SELECT scope_key, severity, sample
  FROM problems
 WHERE scope_key LIKE 'record:%' OR scope_key LIKE 'listing:%'
    OR scope_key LIKE 'skipped:%'
 ORDER BY scope_key;
```

- `listing:files`: the walk could not read part of the tree. That scan deleted nothing, so files deleted since may still be in the copy; the sample names the first entry that failed and how many did.
- `record:pdf_paths:<path>`: a PDF that could not be read or classified, or was refused by the expansion limits. It is not in the copy (or keeps the row an earlier scan wrote, if it could not be opened).
- `record:files:<path>`, severity `warning`: a file over `max_bytes`, not read.
- `skipped:documents_without_text:<hash>`, severity `warning`: one row for all the documents that are stored but not searchable; the sample gives the count and names one. The query under "The documents that are not searchable" lists them.

The render step keeps its own `problems` in the render store (a document whose file changed since the sync, a document missing parts of itself); those are not in this file.

`sync_runs` stays empty for this source. A scan commits once, at its end, and only when something changed; the newest commit (`SELECT date, message FROM dolt_log() LIMIT 1`) has the message `download <source id>: pdfs=… docs=… errors=…`.

### `pdf_documents`

One row per distinct PDF (by its bytes) that some path in the tree holds and that classified.

| column | what it holds |
|---|---|
| `blake3` | the blake3 hash of the file's bytes, lowercase hex; the document's id |
| `size` | the file's size in bytes |
| `page_count` | how many pages it has |
| `pdf_type` | how the classifier read it: `text_based`, `scanned`, `image_based` or `mixed` (some pages with text, some without) |
| `confidence` | the classifier's confidence in `pdf_type`, from 0 to 1 |
| `needs_ocr` | 1 if at least one page has no extractable text; the document may still render its other pages |
| `ocr_page_count` | how many pages have no extractable text |
| `has_encoding_issues` | 1 if any page uses a font that cannot be decoded to text; such a document is not rendered at all |
| `title` | the Info dictionary's `/Title`, else a title the converter guessed from the page; NULL if neither |
| `author` | the Info dictionary's `/Author`, else XMP `dc:creator`; often a producer's username, often NULL |
| `doc_created_at` | the Info dictionary's `CreationDate`, as `YYYY-MM-DDTHH:MM:SS` with its offset when the PDF gave one; missing parts filled with the earliest value |
| `doc_modified_at` | the Info dictionary's `ModDate`, the same way |
| `content_blake3` | a hash over the document's objects with the Info dictionary, XMP packet and trailer `/ID` left out, so it survives a retitle; NULL when the file is encrypted, could not be parsed whole, or had an object stream too large to read |
| `pdf_id_permanent` | the first half of the trailer `/ID`, lowercase hex; many producers regenerate it on every save |
| `xmp_document_id` | the XMP `xmpMM:DocumentID`, meant to stay the same across saves; rarely present |
| `xmp_instance_id` | the XMP `xmpMM:InstanceID`, new on every save |
| `xmp_original_document_id` | the XMP `xmpMM:OriginalDocumentID`, the document this one was derived from |

### `pdf_paths`

One row per `.pdf` file in the tree whose document is in `pdf_documents`, as of the last scan.

| column | what it holds |
|---|---|
| `id` | its path relative to the scan root, `/`-separated |
| `blake3` | the hash of its bytes when the scan read it, joining to `pdf_documents.blake3` |

### `pdf_scan_meta`

One row: where this source scans. The render step reads the root from here.

| column | what it holds |
|---|---|
| `id` | the source's id |
| `abs_root` | the scan root, as the config names it with `~` expanded; symbolic links in it are not resolved |

`pdf_documents_bookkeeping.fetched_at_utc` is when the document was first seen (its `id` is the document's `blake3`), and `pdf_paths_bookkeeping.fetched_at_utc` when the path was first seen holding a document; later scans do not change either.

## Perseus Digital Library (`perseus`)

**Perseus has no raw store and no tables of its own.** Its download
writes plain files, not an `entities.doltlite_db`, which is why it has
no entry in `raw_shapes/current.json`. The source's
`<data_root>/<group>/ingest/` folder holds the TEI XML files fetched
from `https://raw.githubusercontent.com/PerseusDL/canonical-greekLit/refs/heads/master/data/<path>`,
each under its base name. The render step reads that folder, or the
folder its `common.input_path` names, if set.

This heading deliberately has no `(`perseus`)` code span: the check on
`raw_stores.md` fails a source heading whose type has no raw store.

### Is the copy complete?

Perseus keeps no store, so there are no `problems`, `sync_runs` or
owed-record rows to read. The only signals are:

1. **The ingest step's last run.** A run that did not download every
   file fails, naming each file that failed (`N of M perseus files did
   not download`). The app's Manage row and the step's logs show it.
2. **The files on disk.** Every name in `files` (or in the default
   list) should be in `<data_root>/<group>/ingest/` under its base name.
   A file's modification time is when it was last downloaded.

The render step fails, naming the folder, if the folder holds no
`tlg0003.tlg001.*.xml` file at all.

## Signal (`signal`)

The store holds the newest Signal Android backup in the folder, decrypted:
each `payload` is one frame of the backup (Signal's `backup.proto`),
written as JSON by Datalib. Field names are the proto's in snake_case
(`date_sent`, `directional_details`). A field that can be one of several
kinds is an object with one key naming the kind:
`{"item": {"StandardMessage": {...}}}`, `{"directional_details":
{"Outgoing": {...}}}`, `{"destination": {"Contact": {...}}}`. Byte
fields (an ACI, a key) are arrays of numbers. The frames keep the
backup's own numbers for recipients and chats (`$.id`, `$.chat_id`,
`$.author_id`); those change when the account moves to a new phone, so
**join on the `id` columns, never on those numbers.**

Each backup read is one commit, so `dolt_log()` lists them, newest
first, and `dolt_diff_chat_items('<older>', '<newer>')` lists what one
backup lost or changed against another. Datalib writes no `sync_runs`
rows for this source, and a sync that finds no new backup commits
nothing.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| the account itself | `recipients.id = 'self'`, `account.id = 'self'` |
| a person's ACI (their Signal account id) | `recipients.id` = `aci:` + its 32 hex digits, no dashes |
| a person with no ACI in the backup | `recipients.id` = `pni:<hex>`, else `e164:+<number>` |
| a phone number | `recipients.identifier` = `+<number>` |
| a group | `recipients.id` = `group:` + a hash of its master key |
| a chat | `chats.id`, the same as its `recipient_id`; `chat_items.chat_id` |
| a message | `chat_items.id` = `<chat id>#<author id>#<date_sent>`; the grid row's `upstream_id` |
| an attachment slot | `chat_item_attachments.id` = `<message id>#<slot>`, slot counting from 0 |
| a backup folder | `ingested_backups.snapshot_dir` |

A recipient or chat with nothing stable to go by, or a second one that
would take an id already taken, is filed as `recipient:<n>` or
`chat:<n>`, the backup's own number. That id holds for that backup only.
A contact first seen by number alone gets a new id (`aci:…`) in the
first backup that carries their ACI.

**Telling "me" from others.** A message you sent has `author_id =
'self'` and an `Outgoing` direction; one someone else sent has
`Incoming`; a chat update or call has `Directionless`.

```sql
SELECT id, datetime(date_sent / 1000, 'unixepoch') AS sent_utc,
       author_id = 'self' AS from_me,
       json_extract(payload, '$.item.StandardMessage.text.body') AS text
  FROM chat_items
 WHERE chat_id = 'e164:+15550100'
 ORDER BY date_sent;
```

**Every chat, with who it is with and how many messages it has:**

```sql
SELECT c.id, r.display_name, r.identifier, count(i.id) AS messages
  FROM chats c
  JOIN recipients r ON r.id = c.recipient_id
  LEFT JOIN chat_items i ON i.chat_id = c.id
 GROUP BY c.id
 ORDER BY messages DESC;
```

**A message's attachments, with their bytes** (attach `blobs.sqlite`
as in § "Opening a store"):

```sql
SELECT a.id, a.ref_id, c.content_type, c.byte_len
  FROM chat_item_attachments a
  LEFT JOIN cas.cas_objects c ON c.blake3 = a.blake3
 WHERE a.chat_item_id = '<message id>';
```

**Unread messages** (someone else's, not yet read on the phone when the
backup was taken):

```sql
SELECT i.id, i.chat_id FROM chat_items i
  JOIN read_marks m ON m.id = i.id
 WHERE m.read = 0;
```

### Is the copy complete?

The copy can only be as complete as the newest backup in the folder.
Datalib keeps no record of what is owed: there is no `held_version`,
`coverage` or listing table for this source, and it writes no
`sync_runs` rows. Check three things instead.

1. **Problems.** Ignore `skipped:not_shown:…`, which only counts what no
   page shows.

   ```sql
   SELECT scope_key, severity, sample FROM problems
    WHERE scope_key NOT LIKE 'skipped:not_shown:%'
    ORDER BY scope_key;
   ```

   - `record:ingested_backups:<folder name>`: that backup was not read
     (half copied, corrupt, or wrong key), or it is the newest one read
     and is gone from the folder, so the copy is stuck on it.
   - `phase:frames`: part of the newest backup read would not decode.
     Some messages are missing, and nothing that backup dropped was
     deleted.
   - `chat_item_attachments:<id>`: that attachment has no bytes. A
     `warning` means the backup never had them (the phone never
     downloaded the file); an `error` means the file was there but would
     not read or decrypt.

2. **Which backup the copy holds.** Compare it with the newest
   `signal-backup-*` folder on disk:

   ```sql
   SELECT snapshot_dir, ingested_at_utc FROM ingested_backups
    ORDER BY ingested_at_utc DESC LIMIT 1;
   ```

   The newest commit names it too:
   `SELECT message, date FROM dolt_log() LIMIT 1`.

3. **Attachments without bytes**, counted:

   ```sql
   SELECT count(*) FROM chat_item_attachments WHERE blake3 IS NULL;
   ```

### `account`

One row: the account's own settings and profile, from the backup's
account frame.

| column | what it holds |
|---|---|
| `id` | always `self` |
| `payload` | the backup's account frame (JSONB): profile name, username, settings |

### `recipients`

One row per person, group, or other address the backup lists: contacts,
groups, the account itself, Signal's release-notes chat, story lists and
call links.

| column | what it holds |
|---|---|
| `id` | who it is: `self`, `aci:<hex>`, `pni:<hex>`, `e164:+<n>`, `group:<hash>`, `list:<hex>`, `call_link:<hash>`, `release_notes`, or `recipient:<n>` when nothing stable is known |
| `payload` | the backup's recipient frame (JSONB); its `id` is the backup's own number |
| `identifier` | `me` for the account; for a contact, `+<number>`, else the ACI, else the PNI, as bare hex; NULL for a group and the rest |
| `display_name` | `Me` for the account; a contact's Signal profile name, else the name from the phone's contacts; a group's title; NULL when none |

### `chats`

One row per chat: a one-to-one chat, a group chat, or Note to Self.

| column | what it holds |
|---|---|
| `id` | the chat's id: the same as `recipient_id`, or `chat:<n>` when the chat has nothing stable to go by |
| `payload` | the backup's chat frame (JSONB): archived, pinned, muted, the disappearing-message timer; its `id` and `recipient_id` are the backup's own numbers |
| `recipient_id` | the `recipients.id` this chat is with |

### `chat_items`

One row per item in a chat: a message, and also each call, chat update,
sticker, deleted message and the rest. A message's reactions are inside
its payload, not rows of their own. Only items with text or an
attachment are rendered.

| column | what it holds |
|---|---|
| `id` | `<chat_id>#<author_id>#<date_sent>` |
| `payload` | the backup's chat item frame (JSONB), without the incoming read flag (that is in `read_marks`); earlier versions of an edited message are under `revisions` |
| `chat_id` | the `chats.id` it is in |
| `author_id` | the `recipients.id` who wrote it; `self` for the account |
| `date_sent` | when it was sent, in milliseconds since 1970 (UTC) |

### `chat_item_attachments`

One row per attachment slot of an ordinary message, pointing at its
bytes in `blobs.sqlite`.

| column | what it holds |
|---|---|
| `id` | `<chat_item_id>#<slot>`, the slot counting from 0 in the message's attachment list |
| `chat_item_id` | the `chat_items.id` it belongs to |
| `ref_id` | Signal's local media name for the file; for an attachment the backup carried no key for, its own `id` instead |
| `blake3` | the hash of its bytes in `blobs.sqlite`'s `cas_objects`; NULL when no backup read so far held them |

### `read_marks`

One row per message someone else sent: whether the account had read it
when the backup was taken.

| column | what it holds |
|---|---|
| `id` | the `chat_items.id` it is about |
| `read_through` | always NULL for Signal |
| `read` | 1 if read, 0 if not |
| `muted` | always NULL for Signal |

### `ingested_backups`

One row per backup snapshot the store has read: how the next sync knows
which ones to skip, and which one the current copy holds.

| column | what it holds |
|---|---|
| `fingerprint` | the blake3 of each of the snapshot's `metadata`, `main` and `files`, joined with `:`; a folder renamed or copied has the same fingerprint |
| `blake3` | the blake3 of those three files read one after another |
| `snapshot_dir` | the path of the folder it was last read from |
| `total_byte_size` | the size of those three files together (attachments not counted) |
| `ingested_at_utc` | when it was last written into the store, in UTC; the newest row names the backup the current copy holds |
| `tz_offset` | the offset for that stamp |

## SMS Backup & Restore (`sms_backup_restore`)

The store holds what SyncTech's SMS Backup & Restore app wrote into its
XML backups: `sms-*.xml` files (an `<smses>` root holding `<sms>` and
`<mms>` elements) and `calls-*.xml` files (a `<calls>` root holding
`<call>` elements). Every `*.xml` under `backup.path` is read, at any
depth. There is no service behind it, so `payload` is not an API
response: it is a JSON object Datalib builds from one element's XML
attributes, with the fields named below. The same message is usually
in several backups; it is stored once, and its payload is the one from
the file read last.

The ids are Datalib's own. The export gives no stable id for a text or
a call, so each row's `id` is a UUID made from the number, the time and
a hash of the body (a text), the carrier's message id where there is
one (an MMS), or the type and duration (a call). An identical record in
two backups gets the same id.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a phone number | `sms_messages.conversation_key`, `sms_calls.conversation_key`, as `+` and digits (`+12025550101`); `address` or `number` in the payload as the backup wrote it |
| a group MMS's members | `address` in the payload, the numbers joined by `~`; `conversation_key` is their digits run together |
| an MMS's carrier message id (`m_id`) or transaction id (`tr_id`) | `m_id`, `tr_id` in the payload |
| an MMS part's file name (`image000000.jpg`) | the end of `sms_attachments.ref_name`, which is `<message id>/<part name>` |
| a grid row's `upstream_id` | `sms_messages.id` or `sms_calls.id`; a conversation's is `sms:<conversation_key>` |

**Telling "me" from others.** `box_kind = 'sent'` is a message the
phone's owner sent (Android's type 2, or `msg_box` 2 for an MMS); every
other type, draft, outbox, failed and queued included, is stored as
`'inbox'`. The payload's `is_me` says the same. For a call,
`call_type = 'outgoing'` is one you placed. In an MMS's `addrs`, the
entry with `type` 137 is the sender (151 is to, 130 cc, 129 bcc), and
Android writes `insert-address-token` for the phone's own side.

**A conversation, texts and calls together, in order:**

```sql
SELECT when_ts, kind, box_kind AS direction,
       coalesce(payload ->> '$.body', payload ->> '$.text') AS text
  FROM sms_messages WHERE conversation_key = '+12025550101'
UNION ALL
SELECT when_ts, 'call', call_type, (payload ->> '$.duration') || ' s'
  FROM sms_calls WHERE conversation_key = '+12025550101'
ORDER BY when_ts;
```

`when_ts` is always UTC with milliseconds (`2026-05-08T21:53:18.761+00:00`),
so it sorts as text. It is NULL for a record whose backup gave no time.

**Who sent each MMS:**

```sql
SELECT m.id, m.when_ts, a.value ->> '$.address' AS sender
  FROM sms_messages m, json_each(m.payload, '$.addrs') a
 WHERE m.kind = 'mms' AND a.value ->> '$.type' = 137;
```

**MMS media, with its bytes' type and size:**

```sh
datalib-doltlite -readonly sms/ingest/entities.doltlite_db \
  "ATTACH 'file:sms/ingest/blobs.sqlite?mode=ro' AS cas;
   SELECT a.message_id, a.ref_name, c.content_type, c.byte_len
     FROM sms_attachments a JOIN cas.cas_objects c ON c.blake3 = a.blake3"
```

**Incoming messages the backup marks unread:**

```sql
SELECT m.id, m.conversation_key, m.when_ts
  FROM sms_messages m JOIN read_marks r ON r.id = m.id
 WHERE r.read = 0 AND m.box_kind = 'inbox';
```

`sync_runs` is empty for this source: its download writes no run rows.
`ingested_files` has one row per backup file read, under the scope
`sms_backup_restore/xml`.

### Is the copy complete?

This source keeps no record of what is still owed: there is no service
to owe anything, and `sync_runs` stays empty. A copy is complete when
every backup file in the folder was read and nothing in `problems` says
otherwise.

1. Anything the download could not do:

   ```sql
   SELECT severity, outcome, scope_key, sample FROM problems
    WHERE stage = 'fetch' ORDER BY severity, scope_key;
   ```

   What the `scope_key` prefixes mean:

   - `file:sms_backup_restore/xml:<path>` with `outcome = 'dropped'`: that backup file did not parse, or is XML but not a backup, so none of its records are in the copy. With `outcome = 'nulled'` (a warning): the file is read, but some MMS parts in it held no media.
   - `listing:file <path>`: the file could not be opened; it is read again on the next run.
   - `listing:files`: the folder walk hit an entry it could not read.
   - `record:files:<path>`: a file the scan found and could not open.
   - `listing:removed_records`: files were removed or rewritten but some file could not be read, so no deletion was made; the copy may still hold messages that no backup holds.
   - `sms_attachments:<id>`: one MMS part whose bytes did not decode; the message is there without it.

2. Which backup files the copy is built from, to compare with the folder:

   ```sql
   SELECT rel_path, size_bytes, last_finished_at_utc
     FROM ingested_files WHERE scope = 'sms_backup_restore/xml' ORDER BY rel_path;
   ```

   A file in the folder that is not listed here has not been read yet
   (or was added after the last sync). The newest backup's file name and
   the newest `when_ts` in `sms_messages` say how recent the copy is:

   ```sql
   SELECT max(when_ts) FROM sms_messages;
   ```

### `sms_messages`

One text (`<sms>`) or multimedia message (`<mms>`) from a backup.

| column | what it holds |
|---|---|
| `id` | the message's id, a UUID Datalib makes from the number, the time and a hash of the body (text) or the carrier's message id (MMS) |
| `payload` | the message as JSON (JSONB): `id`, `kind`, `conversation_key`, `conversation_display` (the contact name, or the number), `when`, `date` (milliseconds since 1970), `box`, `is_me`, `address`, `date_sent`, `readable_date`, `contact_name`, `attachments` (the `ref_name`s of its media); a text has `body`, an MMS has `text`, `m_id`, `tr_id` and `addrs` (each `{address, type}`) |
| `conversation_key` | the other side's number as `+` and digits; a group MMS's members' digits run together; `unknown` where the backup gave no address |
| `when_ts` | when it was sent or received, UTC with milliseconds; NULL where the backup gave no time |
| `kind` | `sms` or `mms` |
| `box_kind` | `sent` for a message the phone's owner sent, `inbox` for anything else |

### `sms_calls`

One call from a `calls-*.xml` backup.

| column | what it holds |
|---|---|
| `id` | the call's id, a UUID Datalib makes from the number, the time, the type and the duration |
| `payload` | the call as JSON (JSONB): `id`, `kind` (`call`), `conversation_key`, `conversation_display`, `when`, `date` (milliseconds since 1970), `call_type`, `number`, `duration` (seconds), `readable_date`, `contact_name` |
| `conversation_key` | the other side's number as `+` and digits, the same key its texts carry |
| `when_ts` | when the call was, UTC with milliseconds; NULL where the backup gave no time |
| `call_type` | `incoming`, `outgoing`, `missed`, `voicemail`, `rejected`, `blocked`, or `call` for a type the build does not know |

### `sms_attachments`

One media part of an MMS (a picture, a recording, a video), linking the
message to its bytes in `blobs.sqlite`.

| column | what it holds |
|---|---|
| `id` | `<message id>#<ref_name>` |
| `message_id` | the `sms_messages.id` it belongs to |
| `ref_name` | `<message id>/<part name>`, the part name as the backup gave it (`image000000.jpg`), or `part<n>.<ext>` where it gave none |
| `blake3` | the hash its bytes are filed under in `blobs.sqlite`; NULL where the part's base64 did not decode |

### `read_marks`

Whether the backup says a message was read, one row per message it says
anything about.

| column | what it holds |
|---|---|
| `id` | the `sms_messages.id` it is on |
| `read_through` | always NULL here; this source marks single messages |
| `read` | 1 read, 0 unread, as the backup file read last says |
| `muted` | always NULL here; the backup says nothing of muting |

## Todoist (`todoist`)

Todoist's objects are kept as its API v1 returned them: `payload` is
the object from `POST /sync` (the user, projects, sections, items,
notes, project_notes, labels and collaborators lists), or, in
`completed_items`, from `GET /tasks/completed/by_completion_date`. The
tables keep `/sync`'s names: `items` are tasks and `notes` are comments
on tasks. Every table but `sync_state` and `coverage` is just `id` and
`payload`; there are no promoted columns, so every filter goes through
`json_extract`. Todoist's ids are opaque strings.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| the account's user id | `user.id`; `user_id`, `added_by_uid`, `responsible_uid` in a task's payload; `posted_uid` in a comment's |
| a project id | `projects.id`; `project_id` in a task's, section's or project comment's payload |
| a section id | `sections.id`; `section_id` in a task's payload |
| a task id | `items.id`, `completed_items.id` (the same id for the same task); `parent_id` in a subtask's payload; `item_id` in a comment's |
| a comment id | `notes.id` (on a task), `project_notes.id` (on a project) |
| a label | its name, in a task's `labels` array; `labels.id` is not used by tasks |
| a collaborator's user id | `collaborators.id` |

A task's link is `https://app.todoist.com/app/task/<id>`. **"Me"** is
the `user` row; `collaborators` may list the account too.

**A task's current state.** Take the `items` row when there is one,
else the `completed_items` row. It is completed when the `items` row
says `checked`, or when only `completed_items` has it; leave out a row
with `is_deleted`.

```sql
SELECT t.id,
       t.payload ->> '$.content'    AS content,
       t.payload ->> '$.project_id' AS project_id,
       CASE WHEN t.src = 'completed' OR t.payload ->> '$.checked' THEN 1 ELSE 0 END AS done,
       t.payload ->> '$.completed_at' AS completed_at
  FROM (SELECT id, payload, 'open' AS src FROM items
        UNION ALL
        SELECT id, payload, 'completed' FROM completed_items c
         WHERE NOT EXISTS (SELECT 1 FROM items i WHERE i.id = c.id)) t
 WHERE NOT coalesce(t.payload ->> '$.is_deleted', 0);
```

**Open tasks assigned to me:**

```sql
SELECT i.id, i.payload ->> '$.content', i.payload -> '$.due' AS due
  FROM items i, user u
 WHERE i.payload ->> '$.responsible_uid' = u.id
   AND NOT coalesce(i.payload ->> '$.checked', 0)
   AND NOT coalesce(i.payload ->> '$.is_deleted', 0);
```

**Tasks with a label** (by its name):

```sql
SELECT i.id, i.payload ->> '$.content'
  FROM items i, json_each(i.payload, '$.labels') l
 WHERE l.value = 'errands';
```

**Comments on one task, oldest first:**

```sql
SELECT n.id, n.payload ->> '$.posted_uid', n.payload ->> '$.posted_at', n.payload ->> '$.content'
  FROM notes n
 WHERE n.payload ->> '$.item_id' = :task_id AND NOT coalesce(n.payload ->> '$.is_deleted', 0)
 ORDER BY n.payload ->> '$.posted_at';
```

**Nothing is ever deleted from these tables**: a record Todoist deleted
stays with `is_deleted: true`. Filter on it.

**Set aside from the payload** (in `volatile_payload`): a task's
`day_order` (in `items` and `completed_items`); the account's `karma`,
`karma_trend`, `completed_count`, `completed_today`, `status` and
`features`. **Never stored**: the account's `token`, `websocket_url`
(which carries the token) and `urgent_reminder_device`. A task's
`labels` are sorted by name before storing.

The `user` row's `user_bookkeeping.fetched_at_utc` is when the account
was first stored, not when it was last read: `/sync` sends the account
every time, and it is stamped only once. Only `completed_items_bookkeeping`
has a `held_version`: the task's `updated_at`, else its `completed_at`.

### Is the copy complete?

1. **The newest run, and the problems.** A run whose `/sync` failed
   ends `error` and stored nothing. A run whose read of completed tasks
   failed part-way still ends `ok`; only `problems` shows it.

   ```sql
   SELECT status, started_at_utc, json(summary) FROM sync_runs ORDER BY run_id DESC LIMIT 1;

   SELECT scope_key, severity, reason, sample FROM problems
    WHERE scope_key IN ('listing:completed_items', 'phase:completed_items',
                        'phase:credential', 'phase:gave_up')
       OR severity = 'error';
   ```

   - `listing:completed_items`: a page of completed tasks failed, so
     older completed tasks were not read. The next run carries on.
   - `phase:completed_items`: completed tasks were not read at all
     (no `joined_at` and no `since`).
   - `phase:credential`, `phase:gave_up`: Todoist stopped answering
     after `/sync` had landed; the rest of the read waits for the next
     run.

2. **What is still owed.** Open tasks, projects, sections and comments
   keep no owed-record list: `/sync` either landed (there is a
   `sync_state` row and the newest run did not end `error`) or it
   did not. Completed tasks are owed for any stretch of time
   `coverage` does not hold, between the floor (the `since` in
   `sync_runs.config`, else the account's `joined_at`) and the start
   of the newest run:

   ```sql
   SELECT (SELECT count(*) FROM sync_state) AS synced,
          (SELECT count(*) FROM coverage WHERE scope = 'completed_items') AS spans,
          (SELECT min(lo) FROM coverage WHERE scope = 'completed_items') AS read_back_to,
          (SELECT max(hi) FROM coverage WHERE scope = 'completed_items') AS read_up_to,
          (SELECT substr(payload ->> '$.joined_at', 1, 19) || 'Z' FROM user) AS joined_at,
          (SELECT config ->> '$.since' FROM sync_runs ORDER BY run_id DESC LIMIT 1) AS since;
   ```

   The completed tasks are whole when `spans` is 1 and `read_back_to`
   is at or before the floor. More than one span means a hole between
   them; `read_back_to` later than the floor means the oldest part has
   not been read yet. With `completed = false` (in `sync_runs.config`)
   there is no coverage on purpose. A task completed after `read_up_to`
   is read by the next run.

### `user`

One row: the account the source signs in as.

| column | what it holds |
|---|---|
| `id` | Todoist's user id |
| `payload` | the `user` object from `/sync` (`email`, `full_name`, `joined_at`, `inbox_project_id`, `tz_info`, …), without its token fields |

### `projects`

One row per project `/sync` has sent, deleted and archived ones
included.

| column | what it holds |
|---|---|
| `id` | the project id |
| `payload` | the project (`name`, `description`, `parent_id`, `child_order`, `shared`, `inbox_project`, `is_archived`, `is_deleted`, `created_at`, `updated_at`, …) |

### `sections`

One row per section of a project.

| column | what it holds |
|---|---|
| `id` | the section id |
| `payload` | the section (`name`, `project_id`, `section_order`, `is_archived`, `is_deleted`, `added_at`, `updated_at`, …) |

### `items`

One row per task `/sync` has sent: every open task, and any task
completed or deleted after the first sync, as `/sync` last reported it.

| column | what it holds |
|---|---|
| `id` | the task id |
| `payload` | the task (`content`, `description`, `project_id`, `section_id`, `parent_id`, `child_order`, `priority` (4 is the app's p1), `due`, `deadline`, `duration`, `labels`, `added_by_uid`, `responsible_uid`, `checked`, `is_deleted`, `added_at`, `updated_at`, `completed_at`, …) |

### `completed_items`

One row per completed task, as the completed-tasks endpoint lists it.

| column | what it holds |
|---|---|
| `id` | the task id, the same as in `items` |
| `payload` | the completed task: the task fields above plus `completed_by_uid`, `completed_count`, `postponed_count`, `note_count` |

### `notes`

One row per comment on a task.

| column | what it holds |
|---|---|
| `id` | the comment id |
| `payload` | the comment (`item_id`, `posted_uid`, `posted_at`, `content`, `file_attachment`, `reactions`, `uids_to_notify`, `is_deleted`) |

### `project_notes`

One row per comment on a project.

| column | what it holds |
|---|---|
| `id` | the comment id |
| `payload` | the comment, as in `notes` but with `project_id` in place of `item_id` |

### `labels`

One row per personal label.

| column | what it holds |
|---|---|
| `id` | the label id |
| `payload` | the label (`name`, `color`, `item_order`, `is_favorite`, `is_deleted`, …) |

### `collaborators`

One row per person who shares a project with the account.

| column | what it holds |
|---|---|
| `id` | their user id |
| `payload` | `email`, `full_name`, `timezone`, `image_id` |

### `sync_state`

One row: the position the next `/sync` starts from.

| column | what it holds |
|---|---|
| `id` | always `sync` |
| `sync_token` | the token the last `/sync` answer carried, written in the transaction that stored that answer |
| `full_sync_date_utc` | when Todoist generated the last full sync it sent, as Todoist wrote it; kept from the last full sync while incremental ones carry none |

### `coverage`

The stretches of completion time the read of completed tasks has read
whole: every task Todoist listed as completed in a stretch is stored.
Touching stretches are merged, so a store that has read everything
since its start has one row.

| column | what it holds |
|---|---|
| `scope` | `completed_items` |
| `lo` | the start of the stretch, `YYYY-MM-DDTHH:MM:SSZ` in UTC |
| `hi` | its end, the same way: the start time of the run that read it |

## WhatsApp (`whatsapp`)

A WhatsApp store is WhatsApp's message database, `msgstore.db`,
decrypted from the backup's `Databases/msgstore.db.crypt15` and copied
table for table. **It has no tables of its own for messages**: the
message tables are WhatsApp's, under WhatsApp's names, with WhatsApp's
columns, and their shape changes when WhatsApp's does. They are not
listed here. `.tables` shows them; a 2026 backup had 277. Every sync
drops and refills them from the newest backup, so the store's history
is the history of your backups: a row the phone deleted is in an
earlier commit (`dolt_at_message('<hash>')`), not in `main`.

What the copy leaves out of `msgstore.db`: secondary indexes (the
backup carries none anyway), triggers, views, the storage tables behind
a virtual table, and by default the tables `props`, `backup_changes`
and `frequent`. Each table is keyed on what `msgstore.db` declares,
which is almost always `_id`, a row number. Row numbers do not change
between backups of one phone.

There is no `payload` column. Two tables are Datalib's:
`wa_db_contacts`, copied from the backup's second database `wa.db`,
and `wa_media_files`, the registry of the files under `Media/`, whose
bytes are in `blobs.sqlite`. Neither has a `_bookkeeping` sidecar, and
the store keeps no `sync_runs` rows: those tables exist but this
source never writes them.

**Which id is which**

| upstream id | where it is |
|---|---|
| a chat's or person's WhatsApp id (`15550100001@s.whatsapp.net`, `…@g.us` for a group, `…@lid` for a linked id) | `jid.raw_string`; `wa_db_contacts.jid` |
| a chat | `chat._id`, pointing at `jid._id` through `chat.jid_row_id` |
| a message | `message._id`; its lasting key is `(jid.raw_string, message.key_id, message.from_me)` |
| an attachment | `message_media.message_row_id` → `message._id`; its file is `message_media.file_path`, the same text as `wa_media_files.relative_path` |
| a reaction | `message_add_on._id`, its message `message_add_on.parent_message_row_id`; the emoji is `message_add_on_reaction.reaction`, by `message_add_on_row_id` |
| a linked id's phone number | `jid_map.lid_row_id` → `jid_map.jid_row_id`, both `jid._id` |

**Me** is `message.from_me = 1`. Nothing in the backup names your own
number. In a group, `message.sender_jid_row_id` is the sender's
`jid._id`; in a one-to-one chat it is empty, and the sender of an
incoming message is the chat's `jid`.

**The text** of a plain message is `message.text_data`. A media
message's caption may be in `message_media.media_caption`, and a link
preview is in `message_text`. `message.timestamp` is milliseconds since
1970, UTC.

A chat's messages, oldest first:

```sql
SELECT m._id, datetime(m.timestamp / 1000, 'unixepoch') AS sent_utc,
       m.from_me, coalesce(s.raw_string, j.raw_string) AS sender, m.text_data
  FROM message m
  JOIN chat c ON c._id = m.chat_row_id
  JOIN jid j ON j._id = c.jid_row_id
  LEFT JOIN jid s ON s._id = m.sender_jid_row_id
 WHERE j.raw_string = '15550100001@s.whatsapp.net'
 ORDER BY m.sort_id;
```

On a row with `from_me = 1`, `sender` is not you (it falls back to the
chat's id), so read `from_me` first.

The name your phone shows for a jid (`rows` is JSON text, not JSONB):

```sql
SELECT jid, json_extract(rows, '$[0].display_name') AS address_book,
       json_extract(rows, '$[0].wa_name') AS own_name
  FROM wa_db_contacts WHERE jid = '15550100001@s.whatsapp.net';
```

An attachment's bytes, joining into `blobs.sqlite`:

```sh
datalib-doltlite -readonly whatsapp/ingest/entities.doltlite_db \
  "ATTACH 'file:whatsapp/ingest/blobs.sqlite?mode=ro' AS cas;
   SELECT mm.message_row_id, mm.file_path, f.blake3, c.byte_len
     FROM message_media mm
     LEFT JOIN wa_media_files f ON f.relative_path = mm.file_path
     LEFT JOIN cas.cas_objects c ON c.blake3 = f.blake3 LIMIT 5"
```

A row with `blake3` empty is an attachment whose file the backup's
`Media/` did not hold, or a second file with the same bytes as one
already registered (see `wa_media_files`).

### Is the copy complete?

The store keeps no record of what is owed: each sync copies the whole
backup, so the copy is complete for that backup or the sync failed.
Two questions remain.

1. **Did the last sync leave anything out?** Read `problems`:

   ```sql
   SELECT scope_key, severity, sample FROM problems ORDER BY scope_key;
   ```

   - `phase:source`: the decrypted database had no table to copy, or a
     table named like one of Datalib's. Nothing was copied, and every
     message table is as an earlier sync left it.
   - `phase:wa.db contacts`: `wa.db` would not decrypt or read; names
     are as an earlier sync left them.
   - `listing:media`: part of `Media/` could not be listed; the media
     registry was kept whole rather than refilled.
   - `record:media:<path under Media/>` and
     `record:wa_media_files:Media/<path>`: one media file whose bytes
     could not be read; its message shows a placeholder.

   A wrong key or a backup that will not decrypt fails the sync before
   anything is written, so it leaves no row here; the app shows the
   step as failed.

   The render step records the rest in its own store under
   `<data_root>/<group>/render_markdown/`: a message whose attachment
   has no file (`fetch_failed`, field `attachment`), and a message,
   chat or reaction it dropped (`chat:<_id>`, `message:<_id>`,
   `message_add_on:<_id>`, reason `no_identity`).

2. **How recent is the backup the copy was made from?** `sync_runs` is
   never written for this source, and a sync of an unchanged backup
   makes no commit. The newest message is the best measure of the
   backup's age:

   ```sql
   SELECT datetime(max(timestamp) / 1000, 'unixepoch') AS newest_message_utc FROM message;
   ```

   and `SELECT date, message FROM dolt_log() LIMIT 1` is when the store
   last changed.

### `wa_db_contacts`

One row per WhatsApp id that `wa.db`'s `wa_contacts` table names: the
phone's address-book entries for that id. Refilled whole from each
backup that has `Backups/wa.db.crypt15`; a backup without it, or one
whose `wa.db` will not read, leaves the rows as they were.

| column | what it holds |
|---|---|
| `jid` | the WhatsApp id, as `jid.raw_string` spells it |
| `rows` | every `wa_contacts` row for that id, in `_id` order, as a JSON array of objects with every column (text, not JSONB): `display_name` (the address book's name), `given_name`, `family_name`, `wa_name` (the name the person set), `company`, `title`, and the rest; a BLOB column is in hex |

### `wa_media_files`

One row per distinct file content found under the backup's `Media/`
folder, refilled whole on each sync that has `Media/`. Folders whose
name starts with a dot (WhatsApp's thumbnails and trash) are skipped.
Two files with identical bytes share one row, under the path of the
first one registered.

| column | what it holds |
|---|---|
| `blake3` | the blake3 hash of the bytes, lowercase hex: the key of the bytes in `blobs.sqlite`'s `cas_objects` |
| `relative_path` | the file's path under the backup folder, starting `Media/`, exactly as `message_media.file_path` spells it |
| `size_bytes` | its size in bytes |
| `mime_type` | the type guessed from the file's extension (`image/jpeg`, `audio/ogg`, …); NULL for an extension not on the short list |

## YoLink (`yolink`)

The readings come from YoLink's per-device history download
(`https://us.yosmart.com/download/<family_device_id>/<signature>`), a
CSV of one line per sample. Each CSV line becomes one row per metric in
`yolink_readings`, and `payload` is that whole line as a JSON object of
strings, `{header: value}` (`"Time"`, `"Temperature(℃)"`,
`"Humidity(%RH)"`, …), so a temperature-and-humidity line is stored
twice, once per metric. There is no `sync_runs` row; see the
`coverage` table for what has been fetched.

**Which id is which:**

| you have | it is in the mirror as |
|---|---|
| a device's `name` in the config | `yolink_devices.id`, `yolink_readings.device_name`, `coverage.scope` = `device:<name>` |
| a device's `family_device_id` | `yolink_devices.family_device_id` (half of the device's read credential; do not show it) |
| a device's `device_udid` | not stored; it is only in the config |
| one sample of one metric | `yolink_readings.id` = `<device name>#<ts_ms>#<metric>` |

There is no "me": every device is the account owner's.

**The newest reading of each device**, to see which have gone quiet:

```sql
SELECT device_name, datetime(max(ts_ms) / 1000, 'unixepoch') AS last_reading_utc
  FROM yolink_readings GROUP BY device_name ORDER BY last_reading_utc;
```

**Daily average temperature of one device:**

```sql
SELECT date(ts_ms / 1000, 'unixepoch') AS day_utc, avg(value) AS mean_c, count(*) AS n
  FROM yolink_readings
 WHERE device_name = :device AND metric = 'temperature_c'
 GROUP BY day_utc ORDER BY day_utc;
```

**Water used per day**, from the per-reading consumption, in US gallons
and litres:

```sql
SELECT date(ts_ms / 1000, 'unixepoch') AS day_utc, sum(value) AS gallons,
       sum(value) * 3.785411784 AS litres
  FROM yolink_readings
 WHERE device_name = :device AND metric = 'water_consumption_gal'
 GROUP BY day_utc ORDER BY day_utc;
```

### Is the copy complete?

YoLink writes no `sync_runs` rows. The last finished run's summary
(`devices=… windows=… windows_failed=… readings=… errors=…`) is the
message of the newest `download` commit:

```sql
SELECT date, message FROM dolt_log() WHERE message LIKE 'download %' LIMIT 1;
```

**1. What failed:**

```sql
SELECT severity, scope_key, sample FROM problems ORDER BY scope_key;
```

- `listing:<device>`: some stretches of that device's history could not
  be fetched or read (the row names how many and the first); or the
  device was given up on after 30 failures in a row; or its `start` is
  not a date; or every stretch was refused and it has no reading yet,
  which most likely means a wrong id. The missing stretches are asked
  for again on the next sync, but only while they are newer than about
  66 days.
- `silent:<device>` (a warning): no reading in the last day. The device
  may be offline or out of battery; nothing is missing from what YoLink
  holds.

**2. What is still owed.** The store records what has been fetched in
`coverage`. A device is complete when it has one span reaching from
its start (or 66 days before the last sync, whichever is later) to the
last sync. More than one span means a stretch between them was never
fetched; if it is more than 66 days old, YoLink no longer has it.

```sql
SELECT d.id AS device,
       count(c.lo) AS spans,
       datetime(min(CAST(c.lo AS INTEGER)) / 1000, 'unixepoch') AS from_utc,
       datetime(max(CAST(c.hi AS INTEGER)) / 1000, 'unixepoch') AS fetched_to_utc,
       datetime(d.start_ms / 1000, 'unixepoch') AS configured_start_utc
  FROM yolink_devices d
  LEFT JOIN coverage c ON c.scope = 'device:' || d.id
 GROUP BY d.id;
```

The holes themselves:

```sql
SELECT scope,
       datetime(CAST(hi AS INTEGER) / 1000, 'unixepoch') AS hole_from_utc,
       datetime(CAST(next_lo AS INTEGER) / 1000, 'unixepoch') AS hole_to_utc
  FROM (SELECT scope, hi, lead(lo) OVER (PARTITION BY scope ORDER BY lo) AS next_lo
          FROM coverage WHERE scope LIKE 'device:%')
 WHERE next_lo IS NOT NULL;
```

### `yolink_devices`

One row per device the config has ever listed. A device taken out of
the config keeps its row.

| column | what it holds |
|---|---|
| `id` | the device's `name` in the config, which keys its readings |
| `family_device_id` | the first part of the device's download link, 32 hex characters: half of its read credential |
| `kind` | `temperature_humidity` or `watermeter` |
| `start_ms` | the configured `start`, as Unix milliseconds of that day's midnight in UTC |

### `yolink_readings`

One row per device, time and metric.

| column | what it holds |
|---|---|
| `id` | `<device name>#<ts_ms>#<metric>` |
| `payload` | the CSV line it came from, as a JSON object of strings, JSONB |
| `device_name` | the device's `name`, `yolink_devices.id` |
| `ts_ms` | when the reading was taken, Unix milliseconds |
| `metric` | `temperature_c` (°C), `humidity_pct` (% relative humidity), `water_meter_gal` (the meter's lifetime total, US gallons) or `water_consumption_gal` (US gallons used since the previous reading) |
| `value` | the number, in the unit the metric's name says |

### `coverage`

The stretches of each device's history that have been fetched, a
stretch with no readings included. A stretch is recorded in the same
transaction as its readings.

| column | what it holds |
|---|---|
| `scope` | `device:<device name>` |
| `lo` | the start of the stretch, Unix milliseconds, zero-padded to 16 digits |
| `hi` | its end, the same way; the newest `hi` is when the last sync that reached this device ran |
