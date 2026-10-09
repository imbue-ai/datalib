# JMAP Extract

`jmap-ingest` mirrors a JMAP mail account (RFC 8620 core + RFC 8621
mail) into a single doltlite raw store. Generic across JMAP servers —
tested against Fastmail (`api.fastmail.com`), works against any RFC
8620–conformant server in principle (Stalwart, etc.). The `email`
source's other two modes, the Gmail API and an mbox file, write the same
raw schema; [`docs/dev/email_download_modes.md`](/docs/dev/email_download_modes.md)
covers all three.

Each phase upserts upstream payloads as JSONB into per-type tables;
the full RFC 5322 `.eml` source of every email lands in the per-source
blob CAS (see "The raw store's shape" below). See `src/ingest/schema_raw.rs`
for the schema and
[`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md)
for the rationale behind the table shape.

## Auth

`jmap-ingest` does not handle credentials directly — it shells out
to [`latchkey curl`](https://github.com/imbue-ai/latchkey), which
injects `Authorization: Bearer <token>` on every outbound request
based on the request's URL host. For Fastmail, the steps are in
[`docs/user/getting_your_data.md`](/docs/user/getting_your_data.md)
§"Fastmail".

For another JMAP server (Stalwart, etc.), register a latchkey service
for its host and store its token (the service name is only a label;
the URL host drives routing):

```sh
latchkey services register mail-example --base-api-url="https://mail.example.com/"
latchkey auth set mail-example -H "Authorization: Bearer $(pbpaste)"
```

Blob bytes come from the session's `downloadUrl`, which may be on a
different host than the API (Fastmail's is `www.fastmailusercontent.com`);
that host needs a credential too. Then run with
`--hostname mail.example.com`; session discovery does the rest.

## Run it

```sh
bazelisk run //datalib/backend/etl/providers/email:jmap_ingest -- \
    --out ~/backups/fastmail \
    --hostname api.fastmail.com
```

The store is `<out>/entities.doltlite_db`. Subsequent runs are
incremental — the state token from `Email/changes` is persisted
per-account in `sync_scope_state`, so only created / updated /
destroyed emails since the last run get touched. `--full-resync` lists
the account again and fetches every email again.

To restrict to specific mailboxes, name them by their full label path
(`Work/Projects`), the way `only_extract_labels` does in a config:

```sh
jmap-ingest --hostname api.fastmail.com --out ~/backups/fastmail \
    --only-mailbox-labels "Inbox,Work/Projects"
```

The first run's `Mailbox/get` lands in the `mailboxes` table:

```sh
bazelisk build //third-party/doltlite:doltlite
bazel-bin/third-party/doltlite/doltlite -readonly ~/backups/fastmail/entities.doltlite_db \
    "SELECT id, name, role FROM mailboxes ORDER BY name"
```

Stock `sqlite3` cannot open the file;
[`docs/dev/doltlite.md`](/docs/dev/doltlite.md#getting-the-data-out-export-to-plain-sqlite)
has the one-pipe export.

## API surface used

| JMAP method        | Purpose                                                 |
|--------------------|---------------------------------------------------------|
| `.well-known/jmap` | Session discovery → `apiUrl`, `downloadUrl`, accounts   |
| `Mailbox/get`      | Full mailbox list (first run + fallback)                |
| `Mailbox/changes`  | Incremental: created / updated / destroyed mailbox ids  |
| `Email/changes`    | The delta: created / updated / destroyed email ids      |
| `Email/query`      | Enumeration of a mailbox, or the account, not yet listed whole |
| `Email/get`        | Envelope of every email owed (no body: see below); with no ids, the account's state before an enumeration |
| `downloadUrl`      | Each email's `.eml` bytes                               |

`Thread/get` is not called. A `threads` row is the ids of the emails
held for that thread, oldest first, written in the transaction that
writes or deletes one of them.

## Incrementality: listed, held, owed

The store keeps what upstream **listed** apart from what it **holds**,
and works out what it **owes** from the two each time it is asked
([`data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md#what-is-left-to-fetch-listed-minus-held)).
The holding and the owing are the shared `datalib_etl_web::owed`; what is
email's is the listing and how a batch is fetched and written
(`src/ingest/listed.rs`). The Gmail API mode keeps the same tables.

- `listed_messages` has a row per email upstream named, keyed by
  upstream's id, with a **stamp**: the state token of the response that
  last named it as changed. It is stored because a delta's answer cannot
  be asked for again once the token has moved.
- What is held for an email is `held_version` on its
  `listed_messages_bookkeeping` row: the stamp its row was fetched for,
  written by the loop in the transaction that writes the email.
- An email is **owed** when it is listed and not held at its listed
  stamp (`owed::owed`). Nothing marks an email as done and nothing
  remembers which emails a run meant to fetch, so a run that is killed,
  stopped or refused part-way leaves exactly the unfetched emails owed.
- `listed_whole` has a row per scope (a mailbox id, or `*` for the
  account) that an enumeration has listed to its end.

A run does three things with emails, in this order:

1. **Replays the delta.** Each `Email/changes` response is one
   transaction: what it names as created or updated is listed under its
   `newState`, what it names as destroyed is deleted, and the state
   token (`jmap:<account_id>:state:Email` in `sync_scope_state`)
   advances. The token never waits on a fetch. With no token to replay
   from — a first run, `--full-resync`, or `cannotCalculateChanges` —
   the run asks for the account's state now, saves it, forgets which
   scopes were listed whole, and lists every email again so that all of
   them are owed. No other error does this: an `Email/changes` that
   fails is a `problems` row, and the run goes on with what is stored.
2. **Enumerates what is not listed whole.** The scopes wanted are the
   mailboxes `only_extract_labels` admits, or `*`. `Email/query` walks
   the ones with no `listed_whole` row, listing each page's ids; an
   email already listed is left as it is. The walk starts again every
   run until one reaches its end. Its last transaction records the
   scopes as listed whole and, when the walk covered the whole account,
   deletes every listed email it did not name. Widening
   `only_extract_labels` is therefore an admitted mailbox with no row,
   and it is enumerated until an enumeration of it finishes. A label
   path that matches no mailbox is a `problems` row.
3. **Fetches what is owed** with `owed::drain`, fifty to an
   `Email/get`, one transaction per batch: the email rows, their thread
   rows and the stamp they satisfy. An email the server answers
   `notFound` for, or that a label filter keeps out, is gone: its
   listing, its row and whatever was held for it go.

Then the `.eml` phase downloads the body of every email that has none:
the same `owed::drain`, keyed by the `email_blobs` edge, eight
downloads at once. A flush is 256 bodies or 32 MB of them, whichever
comes first: the bodies go into the CAS, then the edge rows that name
them are written.

Destroyed emails hard-delete the row, its mailbox and keyword joins, its
`email_blobs` edge, its listing and the sidecar row that held it, and
its thread row is rewritten or goes. The bytes stay in the CAS — another
email may share the same `.eml` blob, and doltlite's history retains the
prior state either way.

A mailbox a full `Mailbox/get` does not list comes off every email and
its row goes, the same as one `Mailbox/changes` reports destroyed.

A store written before these tables existed is carried over by two
rungs of the migration ladder (`listed::migrate_from_cursors`, then
`listed::migrate_held_into_the_sidecar`): what it holds is listed and
held under no stamp, so its tokens stay good and nothing is fetched
again. A store from the days of a `fetched_messages` table climbs the
second rung alone, which moves the stamp each email was fetched for
into the sidecar.

## When part of a sync fails

Whatever part of a sync fails becomes a row in `problems`, and the
sync goes on with the rest. A row clears only when a later run tries
the same thing again and it works. The step fails only when nothing
useful is left to do: the store will not take a write, the credential
is refused before anything was mirrored, or the first listing fails
with nothing listed to go on with.

**JMAP.**

- A `Mailbox/get` that fails is a `listing:Mailbox/get` row. The
  mailboxes an earlier run stored still file the mail.
- An `Email/changes` that fails is a `listing:Email/changes` row. The
  state token stays where it was, and the next run replays from it.
- An `Email/query` walk that stops on an error is a
  `listing:Email/query` row. What it listed stays listed, and is
  fetched. Nothing is deleted, and its scopes are not recorded as listed
  whole, so the next run walks again.
- An `Email/get` that fails leaves its emails owed, and each gets a
  `listed_messages:<email id>` row with the attempt counted on its
  `listed_messages_bookkeeping` row; the fetch that works clears the
  row. A refused credential, a retry loop that gave up, or three failed
  batches in a row end the phase with one `phase:Email/get` row.
- An `.eml` that does not download is an `email_blobs:<edge id>` row,
  and every run tries again any `.eml` it does not hold. One over
  `blob_size_limit_bytes` is a warning (`over_size_limit`), not a
  failure.
- Some download failures stop the `.eml` phase early: a refused
  credential (401/403), a retry loop that gave up, or twenty failures in
  a row. Any of these would fail every remaining `.eml` the same way.
  The run stops asking, records one `phase:eml_download` row that says
  why, how many it downloaded and how many are left, and still succeeds,
  so everything it downloaded is kept. The next run downloads the rest,
  and the row clears once a run gets through the phase. The step does
  not fail here, because the emails themselves are already stored by
  the time the `.eml` phase runs.
- The `.eml` bodies wait in memory for their flush (256 of them, or
  32 MB), then go to the CAS and their edge rows are written; each
  write is a point where the run may seal. A run that is killed keeps
  what it had sealed.

**Gmail API.**

- A message that would not fetch stays owed, and gets a
  `listed_messages:<Gmail id>` row with the attempt counted on its
  `listed_messages_bookkeeping` row. Every later run asks for it again,
  and the fetch that works clears the row. It does not hold the
  `historyId` cursor. A message that fetched but would not store is the
  same: owed, with its row, until a fetch stores it.
- A message whose `.eml` was over the limit is a warning on its `.eml`.
  It is owed again once it fits under the limit, because it is held with
  no bytes. If Gmail answers 404 for it then, the message was deleted,
  and it goes.
- A `messages.list` walk that fails is a
  `listing:messages.list <label>` row. The other labels are still
  walked. The label that failed is not recorded as listed whole, so the
  next run walks it again.
- A refused credential, a spent daily quota, or a retry loop that gave
  up ends the fetch with one `phase:messages.get` row, and the run
  still succeeds, so what it fetched is kept; the next run fetches what
  is still owed. With nothing mirrored at all, the run fails instead.

**mbox.** Every run reads every `.mbox` under the input, and keeps no
record of what it read before. The folder is the unit of completeness:
two files can hold one message and rows are keyed by the message, so
only a clean read of every file says what left the input. A run that
gets one stores what it read and deletes every email no file holds,
every name-keyed label no message carries (when no label filter is
set), and everything filed under an account other than the configured
one: its emails, threads and name-keyed labels, then its row. That is
how a changed `account_id` (configured, or made from the input's name)
moves the mail. A changed `only_extract_labels` needs nothing more:
the next run reads everything anyway, and the prune counts every
message it met, filtered or not, so narrowing the filter deletes
nothing.

A file that will not open, whose read fails part-way, or that holds no
message at all (0 bytes, or not one `From ` line) is a
`listing:mbox <file>` row. An mbox has no envelope that could say "no
messages", so an empty file is never read as an emptied mailbox;
deleting the file is what drops the messages only it held. Messages
that will not parse are one `skipped:mbox:` row per file, naming it,
until a run reads that file without them. While either kind of
problem is present, or the walk of the folder missed an entry
(`listing:files`), the run deletes nothing, and if it would have, a
`listing:removed_records` row says how many emails wait. On a run that
reads every message, an `only_extract_labels` entry that no message
carries becomes a `config:` row.

Every run rewrites each email's `_bookkeeping` stamp, so a sync of an
unchanged folder still commits; the `.eml` bytes and their edges are
written only for an `.eml` the store does not hold yet.

## Rate limits

Fastmail doesn't 429 us in practice — JMAP's batch shape (one
methodCalls envelope = one HTTP request, regardless of how many
created/updated ids it carries) keeps the request count tame. A 429 or
502–504 is retried with backoff, honouring `Retry-After`, by the shared
HTTP layer (`datalib_etl_web::http::default_retryability`).

## Tests

**Nothing exercises the real JMAP wire format**: there is no recorded
JMAP fixture or live test (`playback_roundtrip.rs` is an empty
placeholder). The `jmap_*.rs` tests replay hand-written answers in the
shapes RFC 8621 gives, built by `jmap_tape.rs` from a small model of an
account. `jmap_interrupt.rs` and `gmail_interrupt.rs` are the
interruption test: a download cut off at each request in turn and run
again must leave the store an uninterrupted run leaves, from an empty
store and from the store an earlier run left. `tests/email_tests/jmap_render.rs` builds a parsed store in
memory with real `.eml` bytes and renders it; `jmap_mbox.rs` runs the
mbox mode end to end over `tests/fixtures/mbox/star_trek.mbox`.

## The raw store's shape

One schema regardless of where the data came from — mbox and JMAP both
populate it, and the mbox path synthesizes a JMAP-shaped envelope so the two
are identical downstream.

### The `.eml` is the canonical body

The RFC 5322 `.eml` is the **complete backup** of a message: body, headers and
every MIME part, attachments included. It rides in the shared per-source CAS
keyed by `blob_id`, and everything else is metadata around it.

Concretely, there is no `email_attachments` table. The parts inside an `.eml`
are reachable by mail-parsing the bytes at render time, so we don't download
them into separate CAS entries during ingest. Both mbox and JMAP land *only
the `.eml`*.

### `emails` carries the envelope as `payload`

`EmailRow` is payload-shaped like every other entity table: the `id`/`payload`
pair plus promoted columns (time, subject, from/to/cc, message-id, threading
headers, the `.eml`'s blob ref). The payload is the JMAP `Email/get` envelope
— envelope only, since the body comes back from the `.eml`. The promoted
columns exist for indexing and cheap projection; the `mailboxIds` / `keywords`
join inputs are read back out of the payload.

### The `.eml` hash lives on `email_blobs`, not on `emails`

That column has a second writer — the blob-download pass backfills it after
the envelope row already exists — so it lives on its own CAS edge table, like
every other provider's attachment edge. That keeps `emails` single-writer, so
re-upserting a changed envelope (flag or move churn) never clobbers a stored
hash.

### Tables

| table | shape |
|---|---|
| `accounts`, `mailboxes`, `threads`, `emails` | payload-shaped entity tables, each with a paired `<table>_bookkeeping` sidecar; a mailbox's counts live in the sidecar's `volatile_payload` |
| `listed_messages` | JMAP and Gmail API modes: a row per message upstream named, with the token that last named it as changed; its `_bookkeeping` sidecar counts the fetches and holds, in `held_version`, the token the email was fetched for |
| `listed_whole` | JMAP and Gmail API modes: the mailboxes or labels (or `*`) an enumeration has listed to its end |
| `email_mailboxes`, `email_keywords` | N:M join tables with a synthesized `id` PK, refreshed delete-then-insert per email upsert; no sidecars |
| `email_blobs` | CAS edge carrying the `.eml` `blake3`, NULL until the bytes land |
| `ingested_files` | unused: the shared per-file cursor table, declared but written by no mode |
