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
destroyed emails since the last run get touched. Force a full
re-enumeration with `--full-resync`.

To restrict to specific mailboxes, name them by their full label path
(`Work/Projects`), the way `only_extract_labels` does in a config:

```sh
jmap-ingest --hostname api.fastmail.com --out ~/backups/fastmail \
    --only-mailbox-labels "Inbox,Work/Projects"
```

The first run's `Mailbox/get` lands in the `mailboxes` table:

```sh
bazelisk build //third-party/doltlite:doltlite
bazel-bin/third-party/doltlite/doltlite ~/backups/fastmail/entities.doltlite_db \
    "SELECT id, name, role FROM mailboxes ORDER BY name"
```

Stock `sqlite3` cannot open the file.

## API surface used

| JMAP method        | Purpose                                                 |
|--------------------|---------------------------------------------------------|
| `.well-known/jmap` | Session discovery → `apiUrl`, `downloadUrl`, accounts   |
| `Mailbox/get`      | Full mailbox list (first run + fallback)                |
| `Mailbox/changes`  | Incremental: created / updated / destroyed mailbox ids  |
| `Email/get`        | Envelope of every touched email (no body: see below)    |
| `Email/changes`    | Incremental: created / updated / destroyed email ids    |
| `Email/query`      | Full enumeration when no state token exists             |
| `Thread/get`       | Thread membership for every touched threadId            |
| `downloadUrl`      | Each email's `.eml` bytes                               |

## Incrementality

State-token-first; falls back to enumeration on `cannotCalculateChanges`
or first run. Cursors persisted per `(account_id, type_name)` in the
shared `sync_scope_state` table under `jmap:<account_id>:state:<type>`
keys. `--full-resync` clears the cursor for this run only; the next
run re-establishes incremental sync from the post-resync state.
Widening `only_extract_labels` enumerates the newly admitted mailboxes
once, since `Email/changes` cannot surface mail that was already there;
a label path that matches no mailbox is a `problems` row.

Destroyed emails (per `Email/changes`) hard-delete the row, its
mailbox and keyword joins, its `email_blobs` edge, and their
bookkeeping. The bytes stay in the CAS — another email may share the
same `.eml` blob, and doltlite's history retains the prior state
either way.

## Rate limits

Fastmail doesn't 429 us in practice — JMAP's batch shape (one
methodCalls envelope = one HTTP request, regardless of how many
created/updated ids it carries) keeps the request count tame. A 429 or
502–504 is retried with backoff, honouring `Retry-After`, by the shared
HTTP layer (`datalib_etl::http::default_retryability`).

## Tests

**Nothing exercises the real JMAP wire format**: there is no JMAP
fixture, playback test or live test (`playback_roundtrip.rs` is an empty
placeholder). `tests/email_tests/jmap_render.rs` builds a parsed store in
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
| `accounts`, `mailboxes`, `threads`, `emails` | payload-shaped entity tables, each with a paired `<table>_bookkeeping` sidecar |
| `gmail_messages` | Gmail API mode only: Gmail's message id → the row it produced |
| `email_mailboxes`, `email_keywords` | N:M join tables with a synthesized `id` PK, refreshed delete-then-insert per email upsert; no sidecars |
| `email_blobs` | CAS edge carrying the `.eml` `blake3`, NULL until the bytes land |
| `ingested_files` | the shared per-file resume cursor (`file_checkpoint`, scope `email/mbox`) |
