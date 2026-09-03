# The `email` source's download modes

**Status:** current as of 2026-08-25.

`type: email` has three download modes, all writing one raw schema:

| mode | selected by | for |
|------|-------------|-----|
| JMAP | `[steps.params.sync]` | Fastmail, Stalwart, any RFC 8620+8621 server |
| Gmail API | `[steps.params.gmail_api]` | a Gmail / Google Workspace account |
| mbox | neither, plus an `.mbox` at `common.input_path` | a Google Takeout export |

Render reads only the raw store, so it is mode-agnostic and needs no
changes when a mode is added. See
[`data_architecture_ingestion.md`](data_architecture_ingestion.md) for
the surrounding ingestion architecture.

## 1. Why modes of one source, not separate source types

The tree was already built for it, and not aspirationally:

- `src/download/schema_raw.rs:3` — *"The schema is the same regardless of
  where the data came from."* One set of tables (`accounts`,
  `mailboxes`, `threads`, `emails`, `email_blobs`, and the two join
  tables).
- `src/mailbox_labels.rs:11` is explicitly source-agnostic: it resolves
  `Parent/Child` label paths the same way for a JMAP `parentId` tree and
  a flat Gmail label list, so `only_extract_labels` /
  `only_render_labels` mean the same thing in every mode.
- The `↗` outlink for Gmail is built from `Message-ID`
  (`src/render/render.rs:73`), which every mode supplies.

Mode selection is **explicit**: `EmailConfig::live_mode()` returns the
selected transport and errors when more than one live block is set. It
used to be inferable from `sync:` alone; with a second live mode that
became untenable, and silently preferring one would mirror a mailbox the
user didn't ask for. The file-backed mbox mode is deliberately *not* a
`live_mode` variant — choosing it means probing the filesystem for an
`.mbox`, which a schema-only config crate must not do.

## 2. The thing that makes multiple modes worth having

Four transports writing one schema is only worth the trouble if the same
mailbox ingested two ways **dedupes rather than doubles**. That is a
property of three specific pieces of shared code, not of two
implementations happening to agree:

**`src/download/envelope.rs`** — envelope synthesis. Every non-JMAP mode
holds the same two things (the RFC 5322 bytes, plus per-message facts the
transport supplied) and has to produce a JMAP-shaped `Email/get`
envelope. One implementation, so `EmailRow::from_jmap_envelope` — and
therefore every promoted column and the `mailboxIds` / `keywords` join
inputs — is written by exactly one code path.

**Message identity.** `email_id` is the `Message-ID` header, falling back
to the content hash (blake3 of the `.eml`) when there is none. Using a
transport-native id instead (Gmail's hex `id`, JMAP's `Email.id`) would
fork the id space per transport, so a Takeout export followed by a live
sync would double the mailbox.

**`src/download/labels.rs`** — the label vocabulary. Gmail spells one
label differently depending on how you ask:

| concept | Takeout `X-Gmail-Labels` | Gmail API `labels.list` |
|---------|--------------------------|--------------------------|
| inbox   | `Inbox`                  | `INBOX`                  |
| promos  | `Category Promotions`    | `CATEGORY_PROMOTIONS`    |

Left alone that is two `mailboxes` rows and two `mailbox_id`s for one
label — the user's Inbox appearing twice in the grid. `canonical_name`
collapses them onto **Takeout's** spelling, chosen because that is what
existing raw stores already contain, so nothing on disk has to migrate.

A subtlety worth keeping: **`mailbox_id` does not canonicalize.** Google
lets you create a *user* label named literally `INBOX`, and
canonicalizing behind the caller's back merged it with the system inbox.
Only the caller knows whether Google marked a label `type: system`, so
canonicalization is the caller's job. A test pins it.

**Thread ids.** Gmail's API `threadId` is hex; Takeout's `X-GM-THRID`
header is the same 64-bit number in decimal. `normalize_thread_id`
converts to decimal so one conversation stays one thread.

## 3. The Gmail API mode

### Auth is free

latchkey ships a built-in `google-gmail` service and routes by URL host,
so `datalib_etl::http::latchkey_curl` — the same path every other HTTP
provider uses — injects and refreshes the token. Setup is one command:

```sh
latchkey auth browser google-gmail
```

(If it reports no OAuth client, `latchkey auth browser-prepare
google-gmail` creates one via the Cloud Console and takes a few minutes.)

The config is an empty table:

```toml
[steps.params.gmail_api]
```

Which Google account this source mirrors is **not** a Gmail knob — it is
a latchkey one, and it lives in the source-level block every
latchkey-backed provider shares:

```toml
[steps.params.latchkey_settings]
account = "you@gmail.com"
```

Name it only when `google-gmail` holds more than one credential —
latchkey keys by `(service, account)` and requires the selector once
there are two, which is the normal case for work + personal. Omitting it
means "the only stored account"; it is deliberately *not* a
pick-the-first fallback, so with two stored and none named latchkey
fails the request as ambiguous rather than mirroring the wrong mailbox.

The setting reaches the wire as `HttpRequest::latchkey`. It is
deliberately **not** part of `fixture_key`: which identity fetched a
response doesn't change the response's shape, and folding it in would
make one user's playback fixtures unusable by another.

> The knob used to be `gmail_api.account`. It moved because the JMAP
> mode needs it too (Fastmail is just as capable of holding two
> accounts), and because the account is latchkey's namespace rather than
> any one provider's. A config still using the old location fails at
> load time with the replacement spelled out — it is not silently
> ignored.

### Sync

The cursor is the mailbox `historyId`, stored per account in the shared
`sync_scope_state` table under a `gmail:<account>:historyId` key — the
same namespacing discipline as the JMAP path's `jmap:` keys.

- no cursor, or `full_resync` → full sync: `messages.list` paged, then
  `messages.get?format=RAW` per id.
- a cursor → `history.list`. `messagesAdded` and relabeled ids are
  fetched; `messagesDeleted` hard-deletes the row (doltlite history
  retains the prior state), matching what the JMAP path does with
  `Email/changes` destroyed ids.
- `history.list` 404 means the cursor aged out of Google's retention
  window (documented as "typically at least one week"). That is not an
  error — it is the documented signal to fall back to a full sync,
  structurally the same as JMAP's `cannotCalculateChanges`.

Deletions carry Gmail's own message id, but rows are keyed by
`Message-ID`, so the mapping is not local. Ingest stamps
`_source: { via, gmailMessageId, gmailThreadId }` into the envelope
payload (the same provenance pattern
`datalib_etl_anthropic::normalize_to_export_shape` uses) and the delete
matches on it.

### Throughput and the budget

Quota-limited rather than byte-limited:

- 6000 quota units per user per minute; `messages.get` costs 20 ⇒ **~300
  messages/minute**, regardless of message size.
- The daily project ceiling (80M units) is not the binding constraint —
  the per-minute cap holds one account to ~8.6M units/day. Worth knowing
  anyway: as of 2026-05-01 Google bills for usage past the daily
  threshold.

`QuotaThrottle` is a leaky bucket priced in **units**, not requests, so a
mixed workload meters accurately. `message_budget` stops a run early,
commits the cursor, and exits **successfully** with a partial result — a
100k-message mailbox is ~6 hours of backfill, and the honest way to model
that is a run that says how far it got rather than one that fails and
poisons the DAG subtree.

**Batching was considered and skipped.** Gmail's batch endpoint saves
round trips, not quota units, and quota is what binds — so it would add
multipart encoding for no throughput.

**Threads are grouped, not fetched.** `threads.get` costs 40 units
against `messages.get`'s 20, and grouping by `threadId` gives the same
membership for free.

## 4. Appendix: IMAP was built and removed

An IMAP mode was prototyped (commit `b9810b70`) and removed. Recording
why, because the reasoning generalizes to any future non-HTTP provider.

### What worked

`async-imap` 0.11 with `default-features = false, features =
["runtime-tokio"]` pulls in no async-std, no `async-native-tls`, and no
OpenSSL, and its `Read`/`Write` bounds are tokio's under that feature, so
a `tokio-rustls` stream drops in with no compat shim. `imap-proto` parses
`X-GM-MSGID`, `X-GM-THRID`, `X-GM-LABELS`, and MODSEQ natively. None of
that was the problem.

### What didn't: credentials

**latchkey is HTTP-only, deliberately and all the way down.** Verified
against latchkey 3.6.0:

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
exists to prevent.

The prototype got them as values by pointing `$LATCHKEY_CURL` at a shim
that captured argv. That works, but it depends on two implementation
details that are not contracts (that `$LATCHKEY_CURL` names the binary
latchkey spawns, and that credentials arrive in its argv), and more
importantly it defeats latchkey's actual security property. latchkey does
expose a supported library API (`ApiCredentialStore.get`) that would have
been better-mannered, but it is the same hole with better manners.

### The upstream ask

The shape that would actually fix this is **`latchkey imap-gateway`** —
the direct analogue of `latchkey gateway`: terminate the client
connection on localhost, do SASL upstream, keep latchkey in the data path
for a protocol curl cannot hold open. Until something like that exists,
datalib should not be in the business of extracting secrets from
latchkey, and a non-HTTP provider here needs a different plan.

### What survived the removal

The IMAP work paid for itself anyway — `envelope.rs`, `labels.rs`, the
per-mode cursor namespacing (`RawDb::load_scope` / `save_scope`), and
`HttpRequest::latchkey_account` all came out of it and are load-bearing
for the Gmail API mode.

## 5. Testing

Unit tests cover the pure parts (label vocabulary, envelope synthesis,
history parsing, base64url, the quota throttle). What they cannot cover
is incremental correctness, so there is a **live test** —
`tests/gmail_live.rs`, tagged `manual` + `external` + `no-sandbox`:

```sh
bazelisk test //datalib/backend/etl/providers/email:gmail_live \
    --test_arg=--ignored --test_arg=--nocapture --test_output=all \
    --test_env=PATH --test_env=HOME --test_env=USER
```

It mirrors one label (`$DATALIB_GMAIL_TEST_LABEL`, default `datalib`)
out of a real account into a tempdir and asserts against **the doltlite
store the run wrote**, not against log lines — per AGENTS.md, a log line
tells you what the code said, the store tells you what it did. It
asserts nothing about specific subjects or senders, only invariants that
hold for any label.

Two of its assertions exist because they caught real bugs, and neither
failure is visible from a single run:

- **A second run must be a no-op** spending less than one
  `messages.get` of quota. Observed: 4 units (profile + labels +
  history).
- **A budget-limited backfill must walk forward.** Observed with
  `message_budget = 2` over an 8-message label: `+2, +2, +2, +2`, then
  incremental.

### Known gaps

- **No checked-in wire fixtures.** `DOWNLOAD.md`'s "Sample data" section
  is still accurate: `tests/jmap_render.rs` builds a `LoadedRaw` in
  memory, and the Gmail API surface is covered by unit tests over canned
  JSON rather than a replayed conversation. A synth + playback pair
  matching the slack/notion pattern is the obvious next step, and would
  let the live test's invariants run hermetically in CI.
- **`DOWNLOAD.md` is titled "JMAP Extract"** and documents only that
  mode. It predates the other two.
- **Render stamps `provider: jmap`** in QMD frontmatter and
  `class="msg msg--jmap"` in the body, whatever mode produced the row.
  Pre-existing (the mbox mode has always done it too) and harmless —
  nothing keys off it — but misleading to read.

## 6. Bugs the first cut had, and what they teach

Recorded because each was invisible from a passing single run:

1. **The label filter was applied client-side.** `only_extract_labels`
   was checked after `messages.get`, so mirroring an 8-message label out
   of a 26k-message account would have cost 522k quota units — ~105
   minutes of throttled fetching — to keep 8 messages. Now
   `messages.list?labelIds=` narrows it server-side, and a name that
   matches no label is a hard error, because an empty `labelIds` means
   "everything".
2. **A budget-limited run advanced the cursor**, so run 2 went
   incremental and silently abandoned the rest of the mailbox. Fixed by
   holding the cursor when `budget_exhausted`, *and* by skipping
   already-mirrored Gmail ids before spending quota — without the second
   half the run re-fetches the same prefix forever.
3. **Incremental runs clobbered thread membership.** Thread rows were
   built from the messages *this run* fetched, so relabeling one message
   of a ten-message thread rewrote the thread to contain only that one.
   Membership is now read back out of the `emails` table.
4. **Deletes matched on `payload LIKE '%"gmailMessageId":"…"%'`** —
   O(rows) per deletion and silently dependent on serde's exact key
   spacing. Replaced by the `gmail_messages` mapping table, which the
   resumable backfill needed anyway.
5. **`loaded_blob_ids()` was reloaded per `messages.list` page.** Now
   once per run.
