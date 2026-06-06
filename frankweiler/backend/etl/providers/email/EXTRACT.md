# JMAP Extract

`jmap-download` mirrors a JMAP mail account (RFC 8620 core + RFC 8621
mail) into a single doltlite db at `<out>.doltlite_db`. Generic across
JMAP servers — tested against Fastmail (`api.fastmail.com`), works
against any RFC 8620–conformant server in principle (Stalwart, etc.).

Each phase upserts upstream payloads as JSONB into per-type tables;
attachments and the full RFC5322 `.eml` source for every email land in
the shared `blobs` table. See `db.rs` for the schema and
[`DOLTLITE_RAW_PORT_GUIDE.md`](../../DOLTLITE_RAW_PORT_GUIDE.md) for
the rationale behind the table shape.

## Auth (Fastmail)

`jmap-download` does not handle credentials directly — it shells out
to [`latchkey curl`](https://github.com/imbue-ai/latchkey), which
injects `Authorization: Bearer <token>` on every outbound request
based on the request's URL host.

Unlike claude.ai / chatgpt.com, Fastmail's API token is a normal
user-facing thing: no DevTools snippet, no Cloudflare TLS
fingerprinting, no rotating session token. The whole flow:

### 1. Create a Fastmail API token

1. Open <https://app.fastmail.com/settings/security/tokens>.
2. Click **New API token**.
3. Scopes — the JMAP `urn:ietf:params:jmap:mail` capability needs at
   minimum **Read-only access to mail**. If you want this provider to
   round-trip writes in the future (we don't today), grant **Read and
   write access to mail** instead. Leave Calendar / Contacts /
   Files unchecked unless you've taught the provider to use them.
4. Copy the token to the clipboard — it's shown exactly once.

### 2. Register the service(s) with latchkey

Fastmail isn't in latchkey's built-in catalog, so register two
self-hosted services pointing at the two hosts Fastmail uses — the
JMAP API endpoint plus the file-content CDN that hosts
`{downloadUrl}`-resolved blobs:

```sh
latchkey services register fastmail \
    --base-api-url="https://api.fastmail.com/"
latchkey services register fastmail-content \
    --base-api-url="https://www.fastmailusercontent.com/"
```

(latchkey's `register` CLI only takes one `--base-api-url` per
service, so two separate registrations is the workaround. Built-in
services like `slack` ship with multi-host `baseApiUrls` baked in,
but user-registered ones don't.)

Attach the same token to both — `pbpaste` reads from your clipboard
at exec time so the literal token never lands in your shell history:

```sh
latchkey auth set fastmail         -H "Authorization: Bearer $(pbpaste)"
latchkey auth set fastmail-content -H "Authorization: Bearer $(pbpaste)"
```

Latchkey routes by URL host, so requests to `api.fastmail.com` pick
up the `fastmail` service's credentials, and requests to
`www.fastmailusercontent.com` pick up `fastmail-content`'s.

### 3. Smoke test

```sh
# JMAP API host — RFC 8620 says .well-known/jmap may 302; -L follows.
latchkey curl -sSL https://api.fastmail.com/.well-known/jmap \
    | jq '.primaryAccounts."urn:ietf:params:jmap:mail"'

# Blob CDN host — pick any blob_id from the emails table once you've
# done a first run, or just confirm the 401-with-WWW-Authenticate is
# coming back as 401 (not "No service matches URL"):
latchkey curl -sSI "https://www.fastmailusercontent.com/jmap/download/u1a2b3c4d/Gtest/x?type=x"
```

Expect a JMAP account id on the first and an `HTTP/2 …` line on the
second. If you get a 401 with a Fastmail error body, the token isn't
being injected — re-check `latchkey services info <name>` for both
services and confirm the URL host is right.

## Auth (other JMAP servers)

The exact same flow works against any JMAP server — Stalwart Mail
Server, etc. Pick a service name (latchkey only uses it as a label;
the URL host is what drives auth routing):

```sh
latchkey services register mail-example --base-api-url="https://mail.example.com/"
latchkey auth set mail-example -H "Authorization: Bearer $(pbpaste)"
```

Then run with `--hostname mail.example.com`. The JMAP session
discovery does the rest.

## Run it

```sh
bazelisk run //frankweiler/backend/etl/providers/jmap:jmap_download -- \
    --out ~/backups/fastmail \
    --hostname api.fastmail.com
```

Subsequent runs are incremental — the state token from `Email/changes`
is persisted per-account in `sync_scope_state`, so only created /
updated / destroyed emails since the last run get touched. Force a
full re-enumeration with `--full-resync`.

To restrict to specific mailboxes (the JMAP-id, not the human name):

```sh
jmap-download --hostname api.fastmail.com --out ~/backups/fastmail \
    --only-mailbox-ids "abc123,def456"
```

A list of mailbox ids comes from the first run's `Mailbox/get`
response — peek at the resulting db:

```sh
sqlite3 ~/backups/fastmail.doltlite_db \
    "SELECT id, name, role FROM mailboxes ORDER BY name"
```

## API surface used

| JMAP method        | Purpose                                                 |
|--------------------|---------------------------------------------------------|
| `.well-known/jmap` | Session discovery → `apiUrl`, `downloadUrl`, accounts   |
| `Mailbox/get`      | Full mailbox list (first run + fallback)                |
| `Mailbox/changes`  | Incremental: created / updated / destroyed mailbox ids  |
| `Email/get`        | Per-email detail with bodyValues, attachments, headers  |
| `Email/changes`    | Incremental: created / updated / destroyed email ids    |
| `Email/query`      | Full enumeration when no state token exists             |
| `Thread/get`       | Thread membership for every touched threadId            |
| `downloadUrl`      | Blob bytes (`.eml` source + each attachment's blobId)   |

## Incrementality

State-token-first; falls back to enumeration on `cannotCalculateChanges`
or first run. Cursors persisted per `(account_id, type_name)` in the
shared `sync_scope_state` table under `jmap:<account_id>:state:<type>`
keys. `--full-resync` clears the cursor for this run only; the next
run re-establishes incremental sync from the post-resync state.

Destroyed emails (per `Email/changes`) hard-delete the row, its
mailbox / keyword / attachment joins, and its bookkeeping. Blobs are
left in place — another email may share the same `.eml` blob or
attachment blob, and doltlite's history retains the prior state
either way.

## Rate limits

Fastmail doesn't 429 us in practice — JMAP's batch shape (one
methodCalls envelope = one HTTP request, regardless of how many
created/updated ids it carries) keeps the request count tame. There's
no in-process backoff loop today. If a future server returns 429,
model the loop on `chatgpt/src/extract/api.rs`.

## Sample data

No checked-in fixture tree yet — `tests/jmap_render.rs` builds a small
`LoadedRaw` in memory to exercise the renderer. A synth + playback
fixture pair (matching the slack/notion pattern) is a planned
follow-up; until then, the live test (`tests/jmap_live.rs`, currently
a stub) is the only path that exercises the real wire format.
