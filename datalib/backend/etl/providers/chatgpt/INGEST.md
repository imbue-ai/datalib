# ChatGPT: download

The `chatgpt` source mirrors a chatgpt.com account's conversations into
a raw doltlite store. It has one ingest method, the live web API, so
its ingest step's params are one table:

```toml
[[groups]]
id = "chatgpt"
type = "chatgpt"

[[steps]]
group = "chatgpt"
function = "ingest"
[steps.params.api]
since = "2025-01-01"   # only conversations updated at/after this; move it back to backfill
max_pages = 20         # cap on listing pages (100 conversations each)
limit = 100            # cap on detail fetches per run
sleep_between = 0.5    # seconds between detail fetches
# conv_uuids = ["https://chatgpt.com/c/<id>"]   # fetch exactly these, skip the listing
```

`[steps.params.latchkey_settings]` picks which latchkey identity to
authenticate as when more than one is stored for the service
(`datalib_source_common::LatchkeySettings`).

## What the store holds

```
<data_root>/<group>/ingest/
  entities.doltlite_db   # the tables below, plus a <table>_bookkeeping sidecar each
  blobs.doltlite_db      # attachment bytes, content-addressed by blake3
```

`entities.doltlite_db` — the schema is `ingest/schema_raw.rs`:

| table                 | one row per                | columns beside `id` + `payload`                     |
|-----------------------|----------------------------|-----------------------------------------------------|
| `me`                  | account                    | `email`, `name`                                     |
| `conversations`       | conversation               | `title`, `update_time`                              |
| `chatgpt_attachments` | (conversation, attachment) | `conversation_id`, `file_id`, `blake3`              |

`payload` is the endpoint's JSON response as text, with one change:
the top-level arrays the API returns as a *set* (`safe_urls`,
`blocked_urls`, `disabled_tool_ids`, `plugin_ids`) are sorted before
the write, because the API returns them in a different order on every
fetch and an unchanged conversation has to serialize identically to
itself (`canonicalize_conversation_payload`). Every other array keeps
the order the API sent — `mapping.*.children` is branch order,
`content.parts` reading order. Stock `sqlite3` cannot open the store;
`docs/dev/doltlite.md` has the one-pipe export.

The `_bookkeeping` sidecars (`fetched_at_utc`, `attempt_count`,
`last_error`, …) are per-fetch state, kept out of the data tables so
that a doltlite diff between two syncs shows only what changed
upstream. `datalib/backend/etl/README.md` explains the split.

## One run

1. **`/backend-api/me`** — always fetched; it pins the account the run
   reports under and is cheap.
2. **List** `/backend-api/conversations?offset=&limit=100&order=updated`,
   newest-updated first, until the pages run out, `max_pages` is hit,
   or — with `since` set — a page ends past the cutoff. The walk is
   *complete* only in the first case.
3. **Prune**, only after a complete walk: a conversation the store
   holds that the listing did not name was deleted on chatgpt.com, so
   it is deleted here (its row stays in doltlite history). An
   incomplete walk says nothing about the pages it never asked for,
   so it prunes nothing; with `since` configured, most runs stop early
   and prune nothing.
4. **Skip-check**: one bulk read of `conversations.update_time` for
   every listed id. A listed conversation is *missing* (no row),
   *stale* (row, but the stored `update_time` differs), or up to date.
   Missing ones are fetched first, then stale ones, so a run cut
   short by a rate limit spent its budget on new work. Conversations
   older than `since` are never detail-fetched (`out_of_scope` in the
   summary), but rows already stored stay — moving `since` further
   back later backfills the newly in-scope ones as missing.
5. **Detail** `/backend-api/conversation/{id}` for each, written as
   one row; then every attachment that conversation's messages name.
6. **Seal** after each conversation and its blobs have both landed,
   never between the two, so a render that starts early never sees a
   message pointing at bytes it cannot resolve.

`conv_uuids` replaces steps 2–4 with exactly the named conversations
(bare ids or paste-able `https://chatgpt.com/c/<id>` URLs); the
listing is never walked and nothing is pruned.

### The skip key compares at whole seconds

The listing endpoint reports `update_time` as an ISO-8601 string; the
detail endpoint reports the same instant as a Unix-epoch float, and
that is what the row stores (JSON-encoded, `1710959331.420159`). The
two never match as text, so `update_time_secs` reduces both to whole
seconds before comparing. Either side failing to parse counts as stale
— the safe direction is to refetch. `since` is compared at the same
grain.

### Attachments

For every `metadata.attachments[]` entry and `asset_pointer` in a
conversation, the walk asks `/backend-api/files/{id}/download` for a
signed URL, fetches the bytes through `latchkey curl`, and stores them
in `blobs.doltlite_db` keyed by blake3, with a `chatgpt_attachments`
row linking the conversation's `file_id` to that hash. Signed URLs
rotate; bytes do not, so a file whose `blake3` is already on its edge
row is not fetched again (`--refetch-blobs` clears the column and
re-pulls; the CAS is never truncated, and re-fetched bytes land on the
same hash). A failed blob bumps its `attempt_count` and `last_error`
and does not fail the sync. The name and MIME type render needs stay
in the conversation payload; the edge table holds only the mapping.

### Errors and rate limits

A conversation the API refuses (`ChatGPTError::Permanent`) is
recorded on its bookkeeping row through `record_object_error`, which
is how it reaches the `problems` table and the Manage screen; the run
moves on. A `429` is retried with `Retry-After`, or exponential
backoff when the header is absent, inside the shared `latchkey_curl`
chokepoint; when that gives up, `api::ChatGPTClient::get` maps the
`HttpError::GaveUp` to `ChatGPTError::RateLimited` and the run stops
cleanly, to resume from the same store next time.

`--reset-and-redownload` truncates all three tables and their
bookkeeping before the run, so the next doltlite commit's diff is
exactly what upstream changed. The CAS bytes survive, but with the
edge rows gone every attachment is fetched over the wire again and
lands on the hash it already had.

## Auth + Cloudflare

The downloader never handles ChatGPT cookies. It shells out to
[`latchkey curl`](https://github.com/imbue-ai/latchkey), which injects
the `Authorization: Bearer <accessToken>` registered under the
`chatgpt` service.

### Refreshing the access token

ChatGPT rotates the bearer token frequently. When `latchkey services
info chatgpt` reports `invalid`, or requests come back `HTTP 401
token_expired`, refresh it from the browser:

```sh
latchkey auth browser chatgpt
```

That opens chatgpt.com, waits for you to log in, reads the fresh
`accessToken` itself and stores it. No DevTools, no clipboard.

It works because the `chatgpt` service is registered with latchkey's
`token-capture` login flow. `latchkey services info chatgpt` says
whether yours is: `authOptions` lists `browser` if it is, and only
`set` if it was registered any other way. latchkey refuses to
re-register a name that already exists, so switching an old
registration over means dropping it first:

```sh
latchkey services deregister chatgpt
latchkey services register chatgpt \
  --base-api-url=https://chatgpt.com/ \
  --login-url=https://chatgpt.com/auth/login \
  --login-flow=token-capture \
  --login-flow-params='{"tokenUrl": "https://chatgpt.com/api/auth/session", "tokenField": "accessToken"}'
latchkey auth browser chatgpt
```

Needs latchkey >= 3.11.0 (the version this repo pins in
`datalib/backend/runtime/src/node_runtime.rs`). chatgpt.com's page
never calls `/api/auth/session` itself, so before that version the
capture waited for a request that never came — and every failure mode
of the flow is a silent hang, with no timeout.

Smoke test after either path:

```sh
latchkey curl -s https://chatgpt.com/backend-api/me | head -c 200
```

Expect a JSON `{id, email, …}`.

#### Pasting the token by hand

Still the fallback where the browser flow can't run — a headless box,
or a machine whose `chatgpt` service you would rather not deregister.

1. Open <https://chatgpt.com> in a logged-in browser tab.
2. DevTools → **Console** → paste:

   ```js
   (async () => {
     const r = await fetch('/api/auth/session', { credentials: 'include' });
     const j = await r.json();
     if (!j.accessToken) { console.error('no accessToken:', j); return; }
     // navigator.clipboard.writeText only works when the page is focused,
     // and pressing Enter in DevTools leaves DevTools focused. Wait for
     // the next click anywhere on the page, then copy.
     console.log('click anywhere on the page to copy the token to clipboard...');
     addEventListener('click', async () => {
       await navigator.clipboard.writeText(j.accessToken);
       console.log('access token copied to clipboard. Run:');
       console.log('  latchkey auth set chatgpt -H "Authorization: Bearer $(pbpaste)"');
     }, { once: true });
   })();
   ```

   The clipboard holds *only the access token*; the printed command
   uses `$(pbpaste)` so the token never appears in console output or
   shell history.

3. Paste the printed `latchkey auth set …` line into your terminal
   and run it. zsh/bash record the literal `$(pbpaste)`, not the
   resolved token, so nothing sensitive lands in `~/.zsh_history`.

### The Chrome-impersonating curl

`chatgpt.com` is fronted by Cloudflare's managed-challenge system,
which fingerprints TLS handshakes. To clear it, requests go out
through a Chrome-impersonating curl — the bundled `curl-impersonate`,
reached via the router curl (`docs/dev/curl_impersonate.md`). Leave
`LATCHKEY_CURL` unset and the downloader finds the router itself
(`ensure_curl_router`); to set it by hand, point it at the **router**,
which brings the impersonator along as a sibling:

```sh
bazelisk build //third-party/latchkey-curl-shims
export LATCHKEY_CURL="$(pwd)/bazel-bin/third-party/latchkey-curl-shims/latchkey-curl-router"
```

### Why no `cf_clearance` cookie?

Cloudflare gates clients with two layered checks:

1. **TLS fingerprint** (JA3/JA4) — what the handshake *looks* like.
2. **JS challenge → `cf_clearance` cookie** — issued only when the
   fingerprint is suspect, to certify "this client passed the
   challenge once."

Because `curl-impersonate` performs a Chrome handshake from byte zero
(patched BoringSSL + the same cipher suite ordering / ALPN / extensions
as real Chrome), Cloudflare never elevates us to the challenge tier in
the first place. The `cf_clearance` cookie therefore never gets
issued and is not needed in the latchkey credential set — a single
`Authorization: Bearer …` header is the full auth surface.

If you ever *did* need it (some future tightening, or running with
plain `curl` as `LATCHKEY_CURL`), grab it from DevTools → Application
→ Cookies → `chatgpt.com` → row `cf_clearance` (HttpOnly, so the JS
snippet above can't read it), copy its value to the clipboard, and
add another header via `$(pbpaste)` so the cookie doesn't land in
shell history either:

```sh
latchkey auth set chatgpt -H "Cookie: cf_clearance=$(pbpaste)"
```

## API surface used

| Path                                                       | Purpose                               |
|------------------------------------------------------------|---------------------------------------|
| `/backend-api/me`                                          | Identify the calling user             |
| `/backend-api/conversations?offset=&limit=&order=updated`  | Paginated listing, newest first       |
| `/backend-api/conversation/{id}`                           | Full message tree for one conversation |
| `/backend-api/files/{id}/download`                         | Signed URL for one attachment         |

## Tests and sample data

A TNG-themed fixture of the API's shapes lives at
`tests/fixtures/chatgpt_api/` (`me.json`, `conversations.json`, one
`conversations/<id>.json` per conversation), exposed as the Bazel
`tng_fixture` filegroup. It is what `chatgpt_render` renders against,
what `chatgpt_incremental_skip` and `chatgpt_playback_roundtrip` replay
through a playback tape, and what the shared `tests/fixtures` root
ingests (`docs/dev/testing.md` § "Watching a sync stream" is where the
tapes are explained). `chatgpt_live`
downloads one real conversation and snapshots it; it is `manual` and
`#[ignore]`d, run with
`bazelisk run //datalib/backend/etl/providers/chatgpt:chatgpt_live.update`.
