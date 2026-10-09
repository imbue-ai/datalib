# The `email` source's download modes

`type: email` has three download modes, all writing one raw schema. The
code is `datalib/backend/etl/providers/email/` (download; paths below are
relative to it), `email_render/` and `email_config/`.

| mode | selected by | for |
|------|-------------|-----|
| JMAP | `[steps.params.jmap]` | Fastmail, Stalwart, any RFC 8620+8621 server |
| Gmail API | `[steps.params.gmail]` | a Gmail / Google Workspace account |
| mbox | `[steps.params.mbox] path = …` | a Google Takeout export |

Render reads only the raw store, so it is mode-agnostic and needs no
changes when a mode is added. See
[`data_architecture_ingestion.md`](data_architecture_ingestion.md) for
the surrounding ingestion architecture.

Which Fastmail credential reaches what — and the read-only API token
the JMAP mode can use — is in [`fastmail.md`](fastmail.md).

Two of the three have a wizard form: **Gmail** and **Fastmail** are
separate entries in `datalib/ui/src/config/catalog.ts`, each writing the
table that selects its mode. They are not one form with a mode dropdown
— they authenticate against different latchkey services and want
different words on screen — and `variantKey` is what tells two entries
of one step type apart. The mbox mode, and a JMAP server other than
Fastmail, have no form: the catch-all "Email (mbox or other server)"
entry sends you to the config editor.

Both forms fill their label pickers from `datalib-step probe email`
(`src/probe.rs`), which reads the
account's real labels — one `users.labels.list` or one `Mailbox/get` —
and returns them spelled exactly the way `only_extract_labels` matches.
That spelling is the whole point of the probe — Gmail hands us the same
label under three different names depending on how we ask, and the
table in `src/ingest/labels.rs` is where they are reconciled.

## 1. Why modes of one source, not separate source types

- `src/ingest/schema_raw.rs` is one schema for every mode: `accounts`,
  `mailboxes`, `threads`, `emails`, `email_blobs`, the two join tables
  `email_mailboxes` and `email_keywords`, and the two tables the two
  API modes keep their progress in: `listed_messages` (what upstream
  named, with what is held for it on the table's sidecar) and
  `listed_whole`.
- `src/mailbox_labels.rs` resolves `Parent/Child` label paths the same
  way for a JMAP `parentId` tree and a flat Gmail label list, so
  `only_extract_labels` / `only_render_labels` mean the same thing in
  every mode.
- The `↗` outlink for Gmail is built from `Message-ID` (`gmail_outlink`
  in the `email_render` crate), which every mode supplies.

Mode selection is **explicit**: `EmailConfig::live_mode()` returns the
selected live transport and errors when both `jmap` and `gmail` are
set — silently preferring one would mirror a mailbox the user didn't
ask for. The file-backed mbox mode is deliberately *not* a `live_mode`
variant — choosing it means probing the filesystem for an `.mbox`,
which a schema-only config crate must not do; `src/processor.rs` falls
back to `mbox` when no live block is set.

## 2. The thing that makes multiple modes worth having

Three transports writing one schema is only worth the trouble if the same
mailbox ingested two ways **dedupes rather than doubles**. That is a
property of three specific pieces of shared code, not of two
implementations happening to agree:

**`src/ingest/envelope.rs`** — envelope synthesis. Every non-JMAP mode
holds the same two things (the RFC 5322 bytes, plus per-message facts the
transport supplied) and has to produce a JMAP-shaped `Email/get`
envelope. One implementation, so `EmailRow::from_jmap_envelope` — and
therefore every promoted column and the `mailboxIds` / `keywords` join
inputs — is written by exactly one code path.

**Message identity.** `email_id` (`envelope::email_id`) is Gmail's own
message id wherever the transport has one. The API spells it in hex; a
Takeout mbox spells it in decimal, as the sender on each message's
`From ` line (`From 1853466712473707184@xxx Mon Jan 05 …`), with no
header carrying it. Both become the same 16-digit zero-padded hex, so a
Takeout export followed by a live sync lands on the same rows. An mbox
from anywhere else falls back to the `Message-ID` header, then to the
content hash (blake3 of the `.eml`). JMAP keeps its own `Email.id`.

Gmail's id is preferred over `Message-ID` for two reasons. Its top bits
are the time Gmail received the message, so the rows one sync writes sit
together in the store, and a sync rewrites a few pages rather than one
per message ([`etl/README.md` § "What a write costs"](../../datalib/backend/etl/README.md#what-a-write-costs-the-transaction-is-the-unit-and-the-key-decides-the-size)).
And a `Message-ID` is not unique: two Gmail messages can carry the same
one, and keyed by it they collapse into a single row.

**`src/ingest/labels.rs`** — the label vocabulary. Gmail spells one
label differently depending on how you ask:

| concept | Takeout `X-Gmail-Labels` | Gmail API `labels.list` |
|---------|--------------------------|--------------------------|
| inbox   | `Inbox`                  | `INBOX`                  |
| promos  | `Category Promotions`    | `CATEGORY_PROMOTIONS`    |

`canonical_name` collapses a system label onto **Takeout's** spelling
and passes a user label through untouched; that is the `name` every
mode stores, so the user's Inbox reads as one label whichever way it
arrived.

**A label's row is keyed by the best id the mode has.**

| mode | `mailboxes.id` |
|------|----------------|
| JMAP | the server's `Mailbox.id` |
| Gmail API | `gmail:<account>:<label id>` (`gmail_mailbox_id`), Google's own id, so a rename renames the row rather than making a new one |
| mbox | `mbox-<hash of account and name>` (`mailbox_id`), since a Takeout export carries only the name; or, when the store already has a Gmail API row of that name, that row's id |

The two Gmail modes meet in one store when a Takeout import is followed
by a live sync, or the other way round. Each Gmail API run moves every
email under a name-keyed row onto the real id of the label of that
name, and drops the name-keyed row; an mbox run files a message under
the real id when the store has one. A name-keyed row for a label Gmail
no longer lists stays as it was — nothing says what became of it.

Google lets you create a *user* label named literally `INBOX`; only
the API's `type: system` flag tells it from the system inbox, which is
why `LabelIndex` canonicalizes only the labels Google marked system.

**A label that goes away comes off its mail.** `refile_mailboxes`
(`src/ingest/mod.rs`) moves every email under a gone mailbox off it —
its payload's `mailboxIds` and its `email_mailboxes` rows together, so
the two diffs agree — and deletes the row. It runs for a JMAP
`Mailbox/changes` destroy, for a row a full `Mailbox/get` did not list,
for a Gmail label `labels.list` no longer lists, and for a name-keyed
row no message carries after an mbox run read every file (with no
label filter). A listing that names nothing moves nothing, in both
JMAP and Gmail: every account has its system mailboxes, so an empty
list is a server that answered oddly. A Gmail reply with no `labels`
list at all fails the run.

**A mailbox's counts are volatile.** JMAP's `totalEmails`,
`unreadEmails`, `totalThreads` and `unreadThreads` go to the sidecar's
`volatile_payload` (`MAILBOX_VOLATILE_PATHS`), so a message arriving
does not change the Inbox's row. The email raw store's ladder rung 1
took them out of stores written before.

**Thread ids.** Gmail's API `threadId` is hex; Takeout's `X-GM-THRID`
header is the same 64-bit number in decimal. `normalize_thread_id`
converts to decimal so one conversation stays one thread.

## 3. The Gmail API mode

### Auth is free

latchkey ships a built-in `google-gmail` service and routes by URL host,
so `datalib_etl_web::http::latchkey_curl` — the same path every other HTTP
provider uses — injects and refreshes the token. Setup is one command:

```sh
latchkey auth browser google-gmail
```

(If it reports no OAuth client, `latchkey auth browser-prepare
google-gmail` creates one via the Cloud Console and takes a few minutes.)

The config is an empty table:

```toml
[steps.params.gmail]
```

Which Google account this source mirrors is **not** a Gmail knob — it is
a latchkey one, and it lives in the source-level block every
latchkey-backed provider shares:

```toml
[steps.params.latchkey_settings]
account = "you@gmail.com"
```

Name it once `google-gmail` holds more than one credential, which is
the normal case for work + personal: with two stored and none named,
latchkey refuses the request rather than mirroring the wrong mailbox
([`latchkey.md`](latchkey.md#accounts-who-names-them)).

The setting reaches the wire as `HttpRequest::latchkey`. It is
deliberately **not** part of `fixture_key` (`datalib_etl_web::http`): which
identity fetched a response doesn't change the response's shape, and
folding it in would make one user's playback fixtures unusable by
another. It is a source-level block rather than a Gmail knob because
the JMAP mode needs it too; a config that still sets `gmail.account`
fails at load with the new spelling in the error.

### Sync

The Gmail mode keeps the same tables as the JMAP mode, and the same
rule: what Gmail **listed** (`listed_messages`, a row per message id
with the `historyId` that last named it as changed) is stored apart
from what is **held** (the `historyId` each message was fetched for, in
`held_version` on the listing's sidecar), and what is **owed** is asked
of the store each time. The email row a Gmail id produced is keyed by
that id, so no mapping is stored. The provider's
[`INGEST.md`](/datalib/backend/etl/providers/email/INGEST.md)
§"Incrementality" has the rule in full.

The cursor is the mailbox `historyId`, stored per account in the shared
`sync_scope_state` table under a `gmail:<account>:historyId` key — the
same namespacing discipline as the JMAP path's `jmap:` keys. A run:

1. **Replays `history.list`** from the cursor. Its pages are collected,
   then one transaction lists every `messagesAdded` and relabeled id
   under the new `historyId`, deletes every `messagesDeleted` id (the
   email, what was listed and held for it, and its place in its thread;
   doltlite history retains the prior state), and stores the new
   `historyId`. The cursor moves with the listing and never waits on a
   fetch. A relabel names a message again, so it is fetched again
   although it is held.
   A `history.list` 404 means the cursor aged out of Google's retention
   window (documented as "typically at least one week"). That is not an
   error: with no cursor to replay from — that, a first run, or
   `full_resync` — the run stores the `historyId` the profile reported
   before any walk, and forgets which labels were listed whole. What is
   already held is not fetched again, at 20 units a message; so a label
   change made while no cursor was replaying is not seen.
2. **Walks `messages.list`** over each configured label (or the whole
   account) that has no `listed_whole` row, listing each page's ids. A
   message already listed is left as it is. A walk starts again every
   run until it reaches its end; then its label is recorded as listed
   whole, and a walk over the whole account also deletes every listed
   message it did not name — the deletions `history.list` never
   reported. `history.list` only names what *changed*, and mail that
   already sat under a newly configured label did not, so a widened
   `only_extract_labels` is a label with no row, and is walked.
3. **Fetches what is owed** with `messages.get?format=RAW`, newest id
   first: a listed message not held at its listed `historyId`, and one
   held with no `.eml` stored that now fits under
   `blob_size_limit_bytes`. The loop is the shared `owed::drain`: one
   `messages.get` per request, and a flush of 200 messages, or 32 MB of
   their raw bytes if that comes first, written as the `.eml` bytes
   into the CAS and then the email rows, thread rows, `.eml` edges and
   held versions in one transaction. A message Gmail answers 404 for, or that carries none of the configured
   labels, is gone: its listing, its row and whatever was held go. A
   message that will not fetch or will not store stays owed, with a
   `listed_messages:<id>` row in `problems` and its attempts counted.

Labels are reconciled every run, walk or not: `labels.list` is always
the whole set. Ingest stamps `_source: { via, gmailMessageId,
gmailThreadId }` into the envelope payload as provenance.

A thread row is the ids of the emails held for that thread, written in
the transaction that writes or deletes one of them — so relabeling one
message of a ten-message thread does not shrink the thread to one, and
no email is ever without its thread row.

### Throughput and the budget

Quota-limited rather than byte-limited:

- 6000 quota units per user per minute; `messages.get` costs 20 ⇒ **~300
  messages/minute**, regardless of message size.
- The daily project ceiling (80M units) is not the binding constraint —
  the per-minute cap holds one account to ~8.6M units/day. Google bills
  for usage past the daily threshold from 2026-05-01.

`QuotaThrottle` is a leaky bucket priced in **units**, not requests, so a
mixed workload meters accurately. It is the first line, not the last:
Google is the authority on the limit, and the code is built to bump
into it and keep going, the way the Slack provider does.

- **A rate-limit response is retried with backoff** through the shared
  HTTP chokepoint (`latchkey_curl_classified` with `gmail_retryability`).
  Google spells the per-user limit two ways — 429 `rateLimitExceeded`
  and **403 `userRateLimitExceeded`** — and a 403 read as an auth error
  is the trap: the run would walk on and "succeed" with every
  remaining message marked failed. 500 `backendError` is
  retried too, per Google's own guidance; a 403 about scopes, or
  `dailyLimitExceeded`, is not — nothing shorter than a person, or a
  day, fixes those.
- **Rate-limit responses lower the throttle's ceiling** by a fifth for
  the rest of the run (once per batch of responses seen since the last
  request), to a floor of a quarter of the configured value, and empty
  the bucket so the retry is not another burst. A long
  backfill therefore settles under whatever Google actually enforces
  instead of hitting it once a minute.
- **When the retry loop gives up** — the run's `download` bounds
  (`DownloadParams`: by default thirty minutes without a successful
  request, or fifty failures in a row) — the fetch stops rather than
  walking on to fail every remaining id one attempt at a time. So does
  a refused credential (401, or a 403 that is not a rate limit) or
  `dailyLimitExceeded`. What was fetched is written, the run records one
  `phase:messages.get` row and succeeds, and the next run fetches what
  is still owed. Only with nothing mirrored at all does the run fail.

A 100k-message mailbox is still ~6 hours of backfill; the run is
expected to take that long. `message_budget` stops a run early with a
partial result and is kept for experiments and the live test, which uses
it to prove a partial run walks forward; the wizard does not offer it.

**Batching was considered and skipped.** Gmail's batch endpoint saves
round trips, not quota units, and quota is what binds — so it would add
multipart encoding for no throughput.

**Threads are grouped, not fetched.** `threads.get` costs 40 units
against `messages.get`'s 20, and grouping by `threadId` gives the same
membership for free.

## 4. Traps in the Gmail API

Each of these fails silently: a single run passes.

- **Filter server-side.** `only_extract_labels` narrows
  `messages.list?labelIds=`; checking it after `messages.get` would pay
  20 units for every message in the account to keep a handful. A
  configured name that matches no label is a download problem; when
  *none* match, the run fails, because an empty `labelIds` means
  "everything".
- **Repeated `labelIds` intersect.** `only_extract_labels` means
  "carrying **any** of these", and `messages.list` cannot express a
  union: asking for three labels at once returns the messages carrying
  all three, usually none — and a run that lists nothing reports
  success, so every later run finds nothing new. The enumeration is one
  walk per label, each listing into the same `listed_messages` table
  (`walk_unlisted_scopes` in `src/ingest/gmail_api/mod.rs`), and
  `api::list_messages` takes one `Option<&str>` label so the combined
  request cannot be built. A one-label test cannot tell union from
  intersection; `gmail_label_union` (hermetic) and
  `gmail_live_two_labels_mirror_their_union` use two.

## 5. Why there is no IMAP mode

An IMAP mode was prototyped and removed (#175). The reason generalizes
to any non-HTTP provider.

The client side was not the problem: `async-imap` 0.11 with
`default-features = false, features = ["runtime-tokio"]` pulls in no
async-std, no `async-native-tls` and no OpenSSL, takes a `tokio-rustls`
stream with no compat shim, and `imap-proto` parses `X-GM-MSGID`,
`X-GM-THRID`, `X-GM-LABELS` and MODSEQ natively.

The credentials were. **latchkey is HTTP-only, deliberately and all the
way down.** Verified against latchkey 3.6.0:

- `extractUrlFromCurlArguments` returns `null` unless the URL starts with
  `http://` or `https://` — before any service lookup.
- Every credential class exposes exactly one consumption method,
  `injectIntoCurlCall(curlArguments)`. There is no `getSecret()`.
  Credential values are write-only by construction.
- `auth set-nocurl`, the documented escape hatch for credentials that
  "cannot be expressed as static curl arguments" (AWS sigv4), still
  terminates in `injectIntoCurlCall`.
- All built-in services resolve to https base URLs. The gateway is an
  HTTP proxy. The README documents no non-HTTP scope.

Two things that are *not* the obstacle, contrary to the obvious guess:
curl does speak `imaps`, and IMAP credentials **are** expressible as curl
arguments (`-u`, or `--oauth2-bearer` + `--login-options AUTH=XOAUTH2`),
which `RawCurlCredentials` could hold unchanged. Service matching
(`matchesUrl`) is a scheme-agnostic string prefix, and
`services register` already accepts an `imaps://` base URL.

So one-shot IMAP through latchkey is close. But **curl's IMAP is a
fetcher, not a session client**, and a mailbox mirror needs a session:
`SELECT` context, CONDSTORE MODSEQ, unsolicited untagged responses, and
an adaptive fetch loop. A real client has to be its own client — and then
it needs the credential as *values*, which is precisely what latchkey
exists to prevent. Capturing them (a shim at `$LATCHKEY_CURL` that
records argv, or latchkey's library API `ApiCredentialStore.get`)
defeats latchkey's security property either way.

The fix would be upstream: a **`latchkey imap-gateway`**, the analogue
of `latchkey gateway`, that terminates the client connection on
localhost, does SASL upstream and keeps latchkey in the data path.
Until something like that exists, datalib does not extract secrets from
latchkey, and a non-HTTP provider needs a different plan.

## 6. Testing

Unit tests cover the pure parts (label vocabulary, envelope synthesis,
history parsing, base64url, the quota throttle). Hermetic tests in
`tests/email_tests/` replay synthesized Gmail conversations through
`DATALIB_HTTP_PLAYBACK`: `gmail_label_union`,
`gmail_widened_labels_backfill`, `gmail_failed_fetch_is_owed`,
`gmail_history_replay` and `gmail_interrupt` (a download cut off at each
request in turn and run again must leave the store an uninterrupted run
leaves).
Incremental correctness against the real service needs the **live
test** — the `live` module of `tests/email_tests/`, which the
`email_tests` target skips with `--skip live::`:

```sh
bazelisk run //datalib/backend/etl/providers/email:gmail_live
```

It mirrors one label (`$DATALIB_GMAIL_TEST_LABEL`, default `datalib`;
the two-label test adds `$DATALIB_GMAIL_TEST_LABEL_2`, default
`Starred`) out of a real account into a tempdir and asserts against
**the doltlite store the run wrote**, not against log lines. It asserts
nothing about specific subjects or senders, only invariants that hold
for any label. Two of them need more than one run to fail:

- **A second run must be a no-op** spending less than one
  `messages.get` of quota. Observed: 4 units (profile + labels +
  history).
- **A budget-limited backfill must walk forward.** Observed with
  `message_budget = 2` over an 8-message label: `+2, +2, +2, +2`, then
  incremental.

### Known gaps

- **The JMAP surface has no replayed conversation.**
  `tests/email_tests/jmap_render.rs` builds its input in memory, and
  `tests/email_tests/playback_roundtrip.rs` is a placeholder. A synth +
  playback pair matching the slack/notion pattern would let the live
  test's invariants run in CI.
- **`INGEST.md` is titled "JMAP Extract"** and documents only that
  mode.
