# IMAP as a third mode of the `email` source

**Status:** partially implemented. Written 2026-08-25.

* **Gmail REST API mode: complete.** Added after this document was
  written, and now the recommended mode for Gmail — see §14.
* **IMAP mode: connect half done.** Auth, capability detection, and
  folder discovery land; the message pass does not. §11 step 3.

Green under `bazelisk test //...` (111/111).

Adds live IMAP sync to the existing `email` source type, alongside the
JMAP and `.mbox` modes it already has. Gmail is the first target; the
transport is generic (iCloud, Proton Bridge, Dovecot, Exchange,
Fastmail-over-IMAP all fall out of it).

## 1. Recommendation: a mode of `email`, not a new source type

Agreed with the instinct to keep it under `email`. The tree is already
built for it, and that isn't an aspiration — it's load-bearing today:

- `src/download/schema_raw.rs:3` — *"The schema is the same regardless
  of where the data came from — Mbox and JMAP both populate it."* One
  set of tables (`accounts`, `mailboxes`, `threads`, `emails`,
  `email_blobs`, `email_mailboxes`, `email_keywords`).
- `src/download/mbox.rs` synthesizes a **JMAP-shaped envelope** and
  hands it to `EmailRow::from_jmap_envelope`, so a file-backed message
  and a server-fetched one produce byte-identical rows. IMAP does the
  same thing with a different source of labels/flags.
- `src/mailbox_labels.rs:11` is explicitly source-agnostic: it resolves
  `Parent/Child` label paths the same way for a JMAP `parentId` tree
  and a flat Gmail label list. `only_extract_labels` /
  `only_render_labels` therefore work unchanged.
- `Cargo.toml:12` already declares the intent: *"Lives inside the
  broader email crate because we expect to add IMAP (and possibly
  POP/EWS) under the same roof; each transport gets its own
  `*-download` bin while sharing the translate + render code path."*
- The render path (`src/render/`) reads only the raw store. It needs
  **zero changes**. `outlink_format = "gmail"` already builds its `↗`
  link from `Message-ID` (`src/render/render.rs:73`), which IMAP supplies.

A separate `imap` source type would fork all of that and give users two
different-looking email mirrors in the grid for the same protocol
family. The one real cost of staying under `email` is mode selection —
see §3.

### The Gmail-specific caveat — since resolved by building both

For **Gmail alone**, the Gmail REST API is the better transport. That
was flagged here as a caveat and has since been built as a fourth mode;
see §14 for what it turned out to be. IMAP remains the right thing to
have, because it is the *generic* one — it covers every account Gmail's
API does not.

## 2. Auth

### What latchkey can and cannot do here

latchkey injects credentials **into a curl argv**, keyed by URL host.
IMAP is not HTTP, so there is nothing to inject into. Two consequences,
both verified against the installed latchkey (3.6.0; repo pin is 3.1.0
in `datalib/backend/core/src/node_runtime.rs:46`):

1. **latchkey's built-in `google-gmail` service will not authenticate an
   IMAP session.** It is the Gmail REST API
   (`baseApiUrls: ["https://gmail.googleapis.com/"]`) and it requests
   the scopes `gmail.modify` + `gmail.settings.basic`
   (`services/google/gmail.js`). Gmail's IMAP `AUTHENTICATE XOAUTH2`
   requires the restricted `https://mail.google.com/` scope, which no
   latchkey service requests (`grep -rn "mail.google.com"` over the
   package: no hits). The scope list is hardcoded per service, so it
   cannot be widened from the CLI.

2. **latchkey can still be the credential vault**, because
   `latchkey auth set` stores *arbitrary curl arguments* and
   `$LATCHKEY_CURL` lets us substitute the binary that receives them.
   Verified end to end with a throwaway service:

   ```
   $ latchkey services register probe --base-api-url="https://imap.example.invalid/"
   $ latchkey auth set probe -u "probe@example.invalid:SECRET"
   $ LATCHKEY_CURL=./dump-argv.sh latchkey curl https://imap.example.invalid/probe
   ARG:-u
   ARG:probe@example.invalid:SECRET
   ARG:https://imap.example.invalid/probe
   ```

   No network request is made; the credential comes back verbatim. This
   is the same seam the repo already owns for a different purpose —
   `datalib/backend/etl/src/latchkey.rs` points `$LATCHKEY_CURL` at
   `latchkey-curl-dispatch`.

### Design: `latchkey-cred-extract`

Add a small binary next to the existing dispatch curl (or a mode flag on
it) that prints the injected args as JSON and exits without touching the
network. `datalib_etl::latchkey` grows:

```rust
/// Ask latchkey for the credential registered for `service`, without
/// making a request. Returns the parsed curl args.
pub async fn extract_credential(service: &str) -> Result<LatchkeyCredential>;

pub enum LatchkeyCredential {
    /// `-u user:pass` — IMAP LOGIN / AUTH=PLAIN.
    Basic { username: String, password: String },
    /// `-H "Authorization: Bearer …"` — IMAP AUTHENTICATE XOAUTH2.
    Bearer { token: String },
}
```

Rules of the road:

- The extractor runs `latchkey curl <sentinel-url>` with
  `$LATCHKEY_CURL` pointed at itself. The sentinel URL must match the
  registered service's `base-api-url`; nothing dials it.
- The `Bearer` arm is what makes OAuth work the day a token with the
  right scope exists — latchkey already refreshes Google OAuth access
  tokens on use (`services/google/base.js:711`), so a per-run
  extraction always yields a live token.
- Secrets never enter `config.toml`, datalib's raw stores, the wire
  tape, or a tracing event. The config holds only the **service name**.
- Secrets do transit argv of the extractor process, visible to other
  local processes — no worse than what `latchkey curl` already does for
  every provider in the tree, but worth stating.

### Concrete setup: Gmail over IMAP with an app password

Requires 2-Step Verification on the account. Google retired
password-only IMAP in March 2025 but app passwords remain available for
2SV accounts. **Google Workspace admins can disable app passwords
org-wide** — check before promising this works for an `@imbue.com`
account.

```sh
# 1. https://myaccount.google.com/apppasswords → create one, copy it.

# 2. Register a latchkey service. The URL is a routing key only; it is
#    never dialed. Use the real IMAP host so it reads correctly.
latchkey services register gmail-imap --base-api-url="https://imap.gmail.com/"

# 3. Store user + app password as curl `-u` args. `pbpaste` keeps the
#    literal secret out of shell history.
latchkey auth set gmail-imap -u "you@gmail.com:$(pbpaste)"
```

Then in the config: `[steps.params.imap] latchkey_service = "gmail-imap"`.

### The OAuth path, if app passwords are unavailable

If the Workspace policy blocks app passwords, XOAUTH2 needs a Google
Cloud OAuth client that requests `https://mail.google.com/`. latchkey's
built-in service cannot be told to ask for it. Options, in order of
effort:

1. Upstream a `google-gmail-imap` service to latchkey with the
   `https://mail.google.com/` scope. Cleanest; unblocks every latchkey
   user. `latchkey auth browser-prepare` already automates creating the
   Cloud project + OAuth client via Playwright.
2. Create the OAuth client by hand, run the consent flow out of band,
   and `latchkey auth set gmail-imap -H "Authorization: Bearer <token>"`.
   Works, but the token expires hourly and latchkey will not refresh a
   credential it did not mint.
3. Skip IMAP for Gmail and use the Gmail REST API mode (§1 caveat),
   which latchkey supports today with no new scope.

Recommend (1) if app passwords turn out to be blocked, and treat it as a
prerequisite rather than folding an OAuth dance into this provider.

## 3. Config surface

`EmailConfig` picks its mode implicitly today: `sync:` present → JMAP,
else an `.mbox` under `input_path` → mbox (`src/processor.rs:44`). A
third mode makes implicit selection untenable. Add a sibling block and
make ambiguity a validation error — this is the first real cross-field
rule email has had, and `EmailConfig::validate()` is already stubbed
waiting for one (`email_config/src/lib.rs`).

```toml
[[steps]]
id = "gmail.download"
command = "datalib-step download email"
outputs = ["gmail/raw"]

[steps.params]
only_extract_labels = []        # unchanged; matches X-GM-LABELS / folder paths

[steps.params.imap]
host = "imap.gmail.com"
port = 993                       # default
latchkey_service = "gmail-imap"  # credential lookup key; never the secret
# Which folder carries the canonical single copy of each message. Default
# is the \All special-use folder, which is locale-safe (Gmail localizes
# "[Gmail]/All Mail"). Set explicitly for servers without SPECIAL-USE.
# all_mail_folder = "[Gmail]/All Mail"
# folders = []                   # non-Gmail: which folders to mirror; empty = all
# full_resync = false
# connection_concurrency = 4     # Gmail allows 15 simultaneous connections
# daily_download_budget_bytes = 2_000_000_000  # stay under Gmail's 2.5 GB/day
```

`[steps.params.sync]` keeps its current JMAP meaning. It is a poor name
in hindsight, but renaming costs a migration for no user-visible gain;
document it instead.

```rust
// email_config: replaces the ad-hoc `is_jmap()` predicate.
pub enum EmailMode { Jmap(EmailSync), Imap(EmailImap), Mbox(MboxSync), None }
impl EmailConfig {
    pub fn mode(&self, input_path: &Path) -> Result<EmailMode>;  // errors if >1 set
}
```

## 4. Crate layout

Everything lands in `datalib-etl-email`. No new crate.

```
providers/email/src/download/
  imap/
    mod.rs        fetch() — the run loop, mirrors mbox::fetch's shape
    conn.rs       TLS connect + AUTHENTICATE (PLAIN | XOAUTH2), connection pool
    folders.rs    LIST (SPECIAL-USE), folder → label-path mapping
    sync.rs       UID/MODSEQ cursor arithmetic
    ingest.rs     fetched bytes + IMAP metadata → JMAP-shaped envelope
providers/email/src/bin/imap_download.rs   # sibling of jmap_download.rs
providers/email_config/src/lib.rs          # EmailImap, EmailMode
```

`ingest.rs` is a light generalization of the accumulator in
`mbox.rs:649-800`, which today reads labels from the `X-Gmail-Labels`
header and the thread id from `X-GM-THRID`. Factor `ingest_message` so
labels/keywords/thread-id arrive as a parameter rather than being read
out of headers; mbox passes what it parsed from the headers, IMAP passes
what it got from the FETCH. Everything downstream — envelope synthesis,
`EmailRow::from_jmap_envelope`, CAS accumulation, join refresh — is
shared unchanged.

## 5. Download algorithm

### Message identity — reuse mbox's derivation exactly

`email_id = Message-ID header, falling back to blake3(eml_bytes)`. This
is what `mbox.rs:667` does, and matching it is the point: a user who
ingested a Google Takeout `.mbox` and then switches to live IMAP gets
**deduped rows**, not a doubled mailbox.

Deliberately *not* `X-GM-MSGID`, even though it is stable and
permanent — it would fork the id space from the mbox path. Store it in
the envelope payload for outlinks and debugging instead.

### Gmail: one folder, not N

`[Gmail]/All Mail` holds exactly one copy of every message;
per-label folders are views onto it. Select it (found via
`LIST (SPECIAL-USE) "" "*"` → the `\All` flag, so it works under any
UI locale) and fetch `X-GM-LABELS` for label membership. Fetching
per-folder would download a multi-labelled message once per label —
straight into the bandwidth cap.

For non-Gmail servers there is no `\All`: mirror each folder in
`folders` (or every folder), and a message present in two folders
dedupes on `Message-ID` at the accumulator, with both folders recorded
in `mailboxIds`.

### Phases

1. **Connect + authenticate.** TLS on 993. `CAPABILITY` → record
   `X-GM-EXT-1`, `CONDSTORE`, `OBJECTID`, `MOVE`, `COMPRESS=DEFLATE`.
   Feature detection drives everything below; nothing is Gmail-only by
   assumption.
2. **Folders.** `LIST (SPECIAL-USE) "" "*"` → upsert `mailboxes`. IMAP
   hierarchy delimiters (`/` or `.`) normalize to the `Parent/Child`
   path form `mailbox_labels.rs` already expects. On Gmail, the label
   set from `X-GM-LABELS` is the authority and folders are secondary.
3. **New messages.** `SELECT` (or `EXAMINE`) → compare `UIDVALIDITY`
   against the cursor; on mismatch, discard the cursor and re-enumerate.
   Then `UID FETCH <last_uid+1>:* (UID FLAGS INTERNALDATE RFC822.SIZE
   X-GM-MSGID X-GM-THRID X-GM-LABELS BODY.PEEK[])`.
4. **Changed flags/labels.** `UID FETCH 1:* (UID FLAGS X-GM-LABELS)
   (CHANGEDSINCE <modseq>)` when CONDSTORE is advertised; otherwise a
   full `UID FETCH 1:* (UID FLAGS)` sweep. No bodies re-fetched.
5. **Deletions.** Gmail advertises CONDSTORE but **not** QRESYNC, so
   there is no `VANISHED` response to lean on. Reconcile with a periodic
   `UID SEARCH ALL` (a bare UID list, cheap in bytes) and hard-delete
   rows whose UID disappeared — matching what the JMAP path does for
   `Email/changes` destroyed ids. Gate it behind a
   `reconcile_every_n_runs` knob so it is not paid every run.
6. **Threads.** Gmail: group by `X-GM-THRID`, same as the mbox path.
   Non-Gmail: derive from `References` / `In-Reply-To`, falling back to
   one thread per message — the mbox path's existing fallback.

### `BODY.PEEK[]`, always

Never `BODY[]`. `BODY[]` sets `\Seen` on the server and would silently
mark the user's unread mail as read. This is the single most damaging
mistake available in this provider; it belongs in a test, not just a
comment.

## 6. Schema changes

Almost none. `accounts` / `mailboxes` / `threads` / `emails` /
`email_blobs` / the two join tables are all reused as-is, and the `.eml`
lands in the shared CAS exactly as both existing modes do.

One new table, analogous to `mbox_files_checkpoint`:

```rust
/// `imap_uids` — per (folder, uidvalidity, uid), which email row it is.
/// Needed to route CHANGEDSINCE flag/label updates and to resolve
/// deletions, neither of which carry a Message-ID.
#[derive(RawTable)]
#[raw_table(table = "imap_uids")]     // PK (folder, uidvalidity, uid)
pub struct ImapUidRow {
    pub folder: String,
    pub uidvalidity: i64,
    pub uid: i64,
    pub email_id: String,
    pub last_modseq: Option<i64>,
}
```

Alternative considered: skip the table and re-derive `email_id` by
fetching `BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)]` alongside every
CHANGEDSINCE pass. Cheaper on disk, but it cannot answer "which row did
this now-vanished UID refer to", so deletion reconciliation would be
impossible. Keep the table.

Cursors go in the shared `sync_scope_state` under
`imap:<account_id>:<folder>`, holding
`{uidvalidity, last_uid, highest_modseq}` — the same shape and the same
namespacing discipline as `db::state_scope`'s `jmap:` keys
(`db.rs:44`). `RawDb::reset` grows a `DELETE ... WHERE scope LIKE
'imap:%'` next to the existing `jmap:%` one.

## 7. Resource limits

Gmail caps IMAP at **2500 MB/day download** and **15 simultaneous
connections** per account. A 20 GB mailbox is therefore a multi-day
backfill no matter what we do, and hitting the cap gets the account
throttled rather than erroring cleanly.

So the run loop must be **budget-aware and resumable**, not
best-effort:

- `daily_download_budget_bytes` (default ~2 GB, under the cap). Track
  bytes fetched; when the budget is spent, commit the cursor and exit
  **successfully** with a summary saying how far it got. A partial
  backfill is the correct outcome, not a failure — the DAG scheduler
  should not poison the subtree over it.
- `RFC822.SIZE` comes back in the metadata FETCH before the body does,
  so the budget can be checked per message rather than discovered after
  the fact. It also drives `blob_size_limit_bytes`, which the existing
  modes already honor.
- `connection_concurrency` default 4, hard-capped at 10. The JMAP path's
  `DEFAULT_BLOB_CONCURRENCY = 8` is tuned for HTTP GETs, not IMAP
  sessions; IMAP connections are expensive to open and Gmail counts them.
- Negotiate `COMPRESS=DEFLATE` when advertised. Mail compresses well and
  the cap is measured in bytes on the wire.

## 8. Render

No changes. Render reads the raw store, and the raw store looks
identical to the JMAP/mbox output. `outlink_format = "gmail"` already
builds `#search/rfc822msgid:<message-id>` (`src/render/render.rs:73`), which is
exactly what an IMAP-sourced Gmail row carries.

## 9. Testing

The email provider's live test is a stub and there is no checked-in
fixture tree (`DOWNLOAD.md`, "Sample data"). Do not extend that gap.

- **`imap_ingest` (insta, hermetic).** Feed canned `(FETCH metadata,
  eml bytes)` tuples through `ingest.rs` and snapshot the synthesized
  envelopes. Asserts the IMAP path and the mbox path produce identical
  rows for the same message — the dedup claim in §5, made testable.
- **`imap_server` (hermetic).** A minimal in-process IMAP server
  speaking enough of RFC 3501 + `X-GM-EXT-1` to drive the whole run
  loop. This is what makes the interesting cases testable at all:
  UIDVALIDITY rollover, CONDSTORE incremental, deletion reconciliation,
  budget exhaustion mid-run, and — per §5 — that we never issue a
  non-PEEK `BODY[]`. Assert the last one by having the fake server
  **fail the test** if it sees one.
- **`imap_live` (`external` tag).** Real Gmail, opt-in, gated on the
  latchkey credential being present.
- Follow the store-not-the-log rule from `AGENTS.md`: assert against the
  doltlite raw store the run wrote, not against tracing output.

Every insta test needs its sibling `.update` target via
`//tools:insta.bzl`, with `extra_data` / `extra_env` mirrored — they do
not propagate through the wrapper.

## 10. Dependencies and Bazel

- **`async-imap`** 0.11.3 (July 2026, actively maintained — it is Delta
  Chat's IMAP stack), taken with `default-features = false,
  features = ["runtime-tokio"]`. **Confirmed**: that combination adds
  exactly `async-imap`, `imap-proto`, `async-channel`,
  `async-compression`, `event-listener`, `pin-utils`, `self_cell`, and
  `stop-token` to `Cargo.lock` — no `async-std`, no `async-native-tls`,
  no OpenSSL, and no version churn in anything already pinned. With
  `runtime-tokio`, async-imap's `Read`/`Write` bounds *are*
  `tokio::io::AsyncRead`/`AsyncWrite`, so a `tokio-rustls` stream drops
  straight in with no compat shim.
- `imap-proto` parses `X-GM-MSGID`, `X-GM-THRID`, `X-GM-LABELS`, and
  MODSEQ natively — better than expected. But `async_imap::types::Fetch`
  exposes accessors for only *some* of them (there is `gmail_labels()`
  and `gmail_msg_id()`, no `gmail_thr_id()`) and its inner response is
  private. Hence `conn::fetch_raw`: issue the command with
  `run_command` and read `ResponseData::parsed()` ourselves, which
  reaches every attribute and is also the only way to express the
  CONDSTORE `(CHANGEDSINCE n)` modifier `uid_fetch` cannot.
- rustls needs `ClientConfig::builder_with_provider(ring)` rather than
  the provider-sniffing `builder()`: feature unification across this
  workspace can leave more than one provider compiled in, and the
  default-picking builder panics at runtime when it cannot choose —
  a failure that would first appear on the first real connection.
- **TLS**: `tokio-rustls` + `webpki-roots`, both already in
  `datalib/backend/Cargo.lock` via reqwest/sqlx
  (`runtime-tokio-rustls`). No new TLS stack.
- CONDSTORE / `X-GM-*` are not first-class in `async-imap`
  (`extensions/` covers only ID, IDLE, QUOTA). Issue them via
  `Session::run_command_and_read_response` and parse the untagged FETCH
  responses. Budget real time for this — it is the fiddly part.
- Wiring: `datalib/backend/Cargo.toml` `[workspace.dependencies]` →
  regenerate `Cargo.lock` → crate_universe re-pins on the next
  `bazelisk build` (`MODULE.bazel:622`). No `requirements.txt` involved
  (that is Python only).
- `datalib_step`: no new `SOURCE_TYPES` entry — `email` already
  dispatches. `hints.rs`'s `"email"` arm grows an IMAP branch.

## 11. Phasing

1. **Credential extraction — DONE.** `latchkey-cred-extract` +
   `datalib_etl::latchkey::extract_credential`, with unit tests. Lands
   independently and is useful to any future non-HTTP provider. Verified
   end to end against latchkey 3.6.0 for the `-u`, `Bearer`, and
   no-credential cases.
2. **Config + mode selection — DONE.** `EmailImap`,
   `EmailConfig::live_mode()`, ambiguity as a validation error,
   `all_sources.toml` stanza. (Named `live_mode` rather than `mode`: the
   mbox mode is chosen by probing the filesystem, which a schema-only
   crate must not do.)
3. **Connect + full backfill — IN PROGRESS.** Done: TLS + SASL
   (PLAIN and XOAUTH2), capability detection, `LIST` + `\All`
   discovery, the label/flag vocabulary, and the raw FETCH driver.
   Remaining: UID enumeration with `BODY.PEEK[]`, the ingest refactor
   in §4, CAS writes, and the fake-server tests. `fetch()` currently
   performs discovery and then errors rather than writing a partial
   mirror — which makes it a usable connectivity check for a new
   credential in the meantime.
4. **Incrementality.** UID/MODSEQ cursors, `imap_uids`, CHANGEDSINCE.
5. **Budget + deletions.** Byte budget with clean partial exit,
   `UID SEARCH ALL` reconciliation, `COMPRESS=DEFLATE`.
6. **Docs.** Rewrite `providers/email/DOWNLOAD.md` to cover three modes
   (it is titled "JMAP Extract" today); add the Gmail IMAP setup to
   `docs/user/getting_your_data.md`.

Steps 1–3 are the ones that prove the design. If `async-imap` fights us
(§10) it will show up in step 3, before any schema is committed to.

## 12. Drive-by fixes

**Fixed.** Three stale paths pointed at a `providers/jmap/` directory
that does not exist (the crate is `providers/email/`):

- `datalib/backend/datalib_step/src/hints.rs:201` — *"See
  datalib/backend/etl/providers/jmap/DOWNLOAD.md"*.
- `providers/email/DOWNLOAD.md` — *"Run it"* showed
  `bazelisk run //datalib/backend/etl/providers/jmap:jmap_download`;
  the real target is `//datalib/backend/etl/providers/email:jmap_download`.
- `providers/email/tests/jmap_live.rs` — the same wrong package in its
  run instructions.

The same file's Fastmail auth instructions and the `hints.rs` email arm
are both correct as of this writing; only the trailing pointers are wrong.

## 13. Decisions needed before step 2

1. **App password or OAuth?** Determines whether §2's "concrete setup"
   is the whole story or whether upstreaming a latchkey service is a
   prerequisite. Check whether the `@imbue.com` Workspace policy allows
   app passwords.
2. **Deletion semantics.** JMAP hard-deletes destroyed rows and leans on
   doltlite history. Same for IMAP, or soft-delete? Hard-delete matches
   precedent; the reconciliation sweep makes it more expensive to get
   right.
3. **Gmail REST API as a fourth mode** — worth scheduling now, or park
   it until IMAP's bandwidth cap actually bites?


## 14. The Gmail REST API mode (built)

Added after §1's caveat, and now the recommended mode for a Gmail
account. Same raw schema, so §8 (render needs no changes) holds
unchanged.

### What it cost, versus what §1 guessed

Less than expected, in one specific way: **it needs no credential
machinery at all.** latchkey ships a built-in `google-gmail` service and
routes by URL host, so `datalib_etl::http::latchkey_curl` — the same
path every other HTTP provider uses — injects and refreshes the token.
The whole config is an empty `[steps.params.gmail_api]` table. Compare
IMAP, which needed `latchkey-cred-extract` before it could authenticate
at all.

The one thing that did have to be added to the shared HTTP layer is
`HttpRequest::latchkey_account` (`latchkey --account <acct> curl …`).
latchkey keys credentials by (service, account) and *requires* the flag
once a service holds two — which is the normal case for Gmail, where
work and personal live under one `google-gmail`. It is deliberately not
part of `fixture_key`: which identity fetched a response doesn't change
the response's shape.

### Sync

Better than IMAP's, on the axis that matters most:

| | Gmail API | Gmail over IMAP |
|---|---|---|
| cursor | `historyId` | `UIDVALIDITY` + UID + `HIGHESTMODSEQ` |
| new mail | `messagesAdded` | `UID FETCH <last+1>:*` |
| flag/label change | `labelsAdded` / `labelsRemoved` | `(CHANGEDSINCE n)` |
| **deletions** | **`messagesDeleted`** | **not reported** — Gmail advertises CONDSTORE but not QRESYNC, so there is no `VANISHED` and the only way to find a deletion is to re-list every UID |
| cursor expiry | 404 → full sync | `UIDVALIDITY` change → re-enumerate |

Google retains history "typically at least one week"; past that
`history.list` returns 404, which is the documented signal to fall back
to a full sync — structurally the same as JMAP's
`cannotCalculateChanges`, and handled the same way.

### Throughput

Quota-limited, not byte-limited, which is the more forgiving shape:

- 6000 quota units per user per minute; `messages.get` costs 20 ⇒ **~300
  messages/minute**, regardless of message size.
- IMAP's cap is 2500 MB/day. At ~75 KB/message that is ~33k messages/day
  against the API's ~432k — roughly **13×**.
- The daily project ceiling (80M units) is not the binding constraint:
  the per-minute cap holds us to ~8.6M units/day. Worth knowing anyway,
  since as of 2026-05-01 Google bills for usage past the daily threshold.

`QuotaThrottle` is a leaky bucket priced in units rather than requests,
so a mixed workload is metered accurately. **Batching was considered and
skipped**: Gmail's batch endpoint saves round trips, not quota units, and
quota is what binds — so it would add multipart encoding for no
throughput.

### The part that took the most care: making the modes agree

Four modes writing one schema is only worth it if the same mailbox
ingested two ways *dedupes*. Three things had to be pulled out to make
that a property of the code rather than a coincidence:

1. **`download/envelope.rs`** — envelope synthesis, previously inline in
   `mbox.rs`. The `Message-ID`-or-content-hash id rule now has one
   implementation.
2. **`download/labels.rs`** — the label vocabulary. Gmail spells one
   label three ways depending on how you ask (`Inbox` in Takeout,
   `\Inbox` over IMAP, `INBOX` over the API); left alone that is three
   `mailboxes` rows and three ids for one label. `canonical_name`
   collapses them onto Takeout's spelling, chosen because it is what
   existing raw stores already contain, so nothing on disk migrates.
3. **Thread ids.** Gmail's API `threadId` is hex; Takeout's `X-GM-THRID`
   header is the same 64-bit number in decimal. Normalizing to decimal
   is what keeps one conversation from becoming two.

A bug worth recording, because the fix is counterintuitive:
`mailbox_id` must **not** canonicalize internally. Google lets a user
create a label literally named `INBOX`, and canonicalizing behind the
caller's back merged it with the system inbox. Only the caller knows
whether Google marked a label `type: system`, so canonicalization is the
caller's job. A test pins it.

### Deletions

`messagesDeleted` gives Gmail's own message id, but our rows are keyed
by `Message-ID`, so the mapping is not local. Ingest stamps
`_source: { via, gmailMessageId, gmailThreadId }` into the envelope
payload (the same provenance pattern
`datalib_etl_anthropic`'s `normalize_to_export_shape` uses), and the
delete matches on it. Hard delete, matching the JMAP path — doltlite's
history retains the prior state.

### Cursor namespacing — a bug this caught

`RawDb::load_state` hard-codes a `jmap:` scope prefix, so the first
version of this mode stored its `historyId` under a `jmap:` key. Added
`load_scope` / `save_scope` taking the full key, gave each mode its own
namespace (`gmail:`, `imap:`), and widened `RawDb::reset` to clear all
three — it had only ever cleared `jmap:%`, so a reset would have left a
stale cursor pointing into a store that no longer had the rows it named.

### Still open

- **A playback fixture.** The provider still has no checked-in wire
  fixtures (`DOWNLOAD.md`, "Sample data"), so the API surface is covered
  by unit tests over canned JSON, not by a replayed conversation. That
  gap predates this mode but this mode makes it worth closing.
- **Threads are grouped, not fetched.** `threads.get` costs 40 units
  against `messages.get`'s 20, and grouping by `threadId` gives the same
  membership for free. If we ever want Gmail's own thread metadata, that
  is where the cost lands.
