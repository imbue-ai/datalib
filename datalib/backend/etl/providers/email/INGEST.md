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
([`docs/dev/plans/sync_state.md`](/docs/dev/plans/sync_state.md) §2;
`src/ingest/listed.rs`). The Gmail API mode keeps the same three tables.

- `listed_messages` has a row per email upstream named, keyed by
  upstream's id, with a **stamp**: the state token of the response that
  last named it as changed.
- `fetched_messages` has a row per email fetched: the `emails` row it
  produced and the stamp it was fetched for. It is written in the
  transaction that writes the email.
- An email is **owed** when it is listed and not held at its listed
  stamp. Nothing marks an email as done and nothing remembers which
  emails a run meant to fetch, so a run that is killed, stopped or
  refused part-way leaves exactly the unfetched emails owed.
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
3. **Fetches what is owed**, fifty to an `Email/get`, one transaction
   per batch: the email rows, their thread rows and the stamp they
   satisfy. An email the server answers `notFound` for, or that a label
   filter keeps out, loses its listing and whatever was held for it.

Then the `.eml` phase downloads the body of every email that has none.

Destroyed emails hard-delete the row, its mailbox and keyword joins, its
`email_blobs` edge, what was listed and held for it, and their
bookkeeping, and its thread row is rewritten or goes. The bytes stay in
the CAS — another email may share the same `.eml` blob, and doltlite's
history retains the prior state either way.

A mailbox a full `Mailbox/get` does not list comes off every email and
its row goes, the same as one `Mailbox/changes` reports destroyed.

A store written before these tables existed is carried over by a rung
of the migration ladder (`listed::migrate_from_cursors`): what it holds
is listed and held under no stamp, so its tokens stay good and nothing
is fetched again.

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
  `listed_messages:<email id>` row with the attempts counted on its
  `listed_messages_bookkeeping` row; the fetch that works clears both.
  A refused credential, a retry loop that gave up, or three failed
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
- The `.eml` bodies are written as they arrive, every 32 MB or 256
  downloads, and each write is a point where the run may seal. A run
  that is killed keeps what it had sealed.

**Gmail API.**

- A message that would not fetch stays owed, and gets a
  `listed_messages:<Gmail id>` row with the attempts counted on its
  `listed_messages_bookkeeping` row. Every later run asks for it again,
  and the fetch that works clears both. It does not hold the
  `historyId` cursor.
- A message that fetched but would not store gets the same row. A Gmail
  message's bytes never change, so fetching it again with the same build
  would only spend 20 quota units for the same answer. Its
  `fetched_messages` row says which build could not store it
  (`unstorable_by`), and it is owed again only to a different build
  (version or git hash), or once Gmail lists it anew. A message deleted
  upstream drops its row.
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

**mbox.** A file that will not open, or whose read fails part-way, is a
`listing:mbox <file>` row. It is not stamped, so the next run reads it
again. Messages that will not parse are one `file:email/mbox:<file>`
row on their file, which stands until the file is read again. While
either kind of problem is present, the run deletes no message.
Rewritten files are then left unstamped, so the next run reads every
file again and prunes once all of them read cleanly. On a run that
reads every message, an `only_extract_labels` entry that no message
carries becomes a `config:` row.

## Rate limits

Fastmail doesn't 429 us in practice — JMAP's batch shape (one
methodCalls envelope = one HTTP request, regardless of how many
created/updated ids it carries) keeps the request count tame. A 429 or
502–504 is retried with backoff, honouring `Retry-After`, by the shared
HTTP layer (`datalib_etl::http::default_retryability`).

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
| `listed_messages` | JMAP and Gmail API modes: a row per message upstream named, with the token that last named it as changed; its `_bookkeeping` sidecar counts failed fetches |
| `fetched_messages` | JMAP and Gmail API modes: upstream's message id → the `emails` row it produced and the token it was fetched for |
| `listed_whole` | JMAP and Gmail API modes: the mailboxes or labels (or `*`) an enumeration has listed to its end |
| `email_mailboxes`, `email_keywords` | N:M join tables with a synthesized `id` PK, refreshed delete-then-insert per email upsert; no sidecars |
| `email_blobs` | CAS edge carrying the `.eml` `blake3`, NULL until the bytes land |
| `ingested_files` | the shared per-file resume cursor (`file_checkpoint`, scope `email/mbox`) |
