# ChatGPT Extract

`chatgpt-download` incrementally mirrors `chatgpt.com` conversations
into a local JSON cache that matches the Python downloader byte-for-byte:

```
<out>/
  me.json                       # current user profile
  conversations.json            # combined paginated listing index
  conversations/<id>.json       # full per-conversation tree
```

Each per-conversation file gets two synthetic keys stamped in by the
downloader: `_fetched_at` (RFC3339 with local offset) and
`_listing_update_time` (verbatim from the listing endpoint, used as
the incremental-skip key on the next run).

## Auth + Cloudflare

The downloader does not handle ChatGPT cookies directly. It shells
out to [`latchkey curl`](https://github.com/imbue-ai/latchkey), which
injects the `Authorization: Bearer <accessToken>` registered under
the `chatgpt` service.

### Refreshing the access token

ChatGPT rotates the bearer token frequently. When `latchkey services
info chatgpt` reports `invalid` or requests come back with
`HTTP 401 token_expired`, re-run this:

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

4. Smoke test:

   ```sh
   latchkey curl -s https://chatgpt.com/backend-api/me | head -c 200
   ```

   Expect a JSON `{id, email, …}`.

`chatgpt.com` is fronted by Cloudflare's managed-challenge system,
which fingerprints TLS handshakes. To clear the challenge, point
`LATCHKEY_CURL` at a Chrome-impersonating curl. The simplest option
is the in-tree `latchkey-curl-impersonate` bin (a `wreq`-backed shim, mirror
of `src/download/latchkey_curl_impersonate.py`):

```sh
bazelisk build //frankweiler/backend/etl:latchkey_curl_impersonate
export LATCHKEY_CURL="$(pwd)/bazel-bin/frankweiler/backend/etl/latchkey_curl_impersonate"
chatgpt-download --out ~/backups/chatgpt_api
```

A standalone `curl-impersonate` binary works too — point
`LATCHKEY_CURL` at it instead.

### Why no `cf_clearance` cookie?

Cloudflare gates clients with two layered checks:

1. **TLS fingerprint** (JA3/JA4) — what the handshake *looks* like.
2. **JS challenge → `cf_clearance` cookie** — issued only when the
   fingerprint is suspect, to certify "this client passed the
   challenge once."

Because the shim performs a Chrome 131 handshake from byte zero
(boring-ssl + the same cipher suite ordering / ALPN / extensions as
real Chrome), Cloudflare never elevates us to the challenge tier in
the first place. The `cf_clearance` cookie therefore never gets
issued and is not needed in the latchkey credential set — a single
`Authorization: Bearer …` header is the full auth surface.

If you ever *did* need it (e.g. some future tightening, or running
with plain `curl` as `LATCHKEY_CURL`), grab it from DevTools →
Application → Cookies → `chatgpt.com` → row `cf_clearance` (HttpOnly,
so the JS snippet above can't read it), copy its value to the
clipboard, and add another header via `$(pbpaste)` so the cookie
doesn't land in shell history either:

```sh
latchkey auth set chatgpt -H "Cookie: cf_clearance=$(pbpaste)"
```

## API surface used

| Path                                          | Purpose                            |
|-----------------------------------------------|------------------------------------|
| `/backend-api/me`                             | Identify the calling user          |
| `/backend-api/conversations?offset=&limit=&order=updated` | Paginated listing       |
| `/backend-api/conversation/{id}`              | Full mapping/DAG for one conversation |

## Resume + incremental skip

There is no checkpoint file. Resume is per-conversation:

  * The listing endpoint returns `update_time` as an **ISO-8601 string**.
  * The detail endpoint returns `update_time` as a **Unix-epoch float**.

Since those don't compare directly, the downloader stashes the listing
value as `_listing_update_time` in each cached per-conversation file.
A subsequent run skips the detail fetch when
`cached["_listing_update_time"] == api_update`.

The fetch loop reorders the listing so genuinely-missing conversations
come first — if a 429 cuts the run short, the budget went to forward
progress rather than revalidating cache hits.

An optional `since` (config `sync.since:` / CLI `--since`; RFC 3339 or
`YYYY-MM-DD`, assumed UTC) bounds the work: conversations whose
`update_time` predates it are never detail-fetched, and because the
listing is `order=updated` (newest first) the pagination walk stops
early once a page ends past the cutoff. The filter only gates
fetching — already-stored rows are untouched — so moving `since`
further back later backfills the newly-in-scope conversations on that
run. Comparison happens at whole-second grain, the same
canonicalization the skip-check uses.

## Rate limits

ChatGPT returns `429` with a `Retry-After` header during enforced
backoff. Honoring it (and exponential backoff when it's absent) is
handled centrally by the shared `latchkey_curl` chokepoint, bounded by
the source's `extract_params` give-up policy. When the chokepoint gives
up, `api::ChatGPTClient::get` maps the resulting `HttpError::GaveUp` to
`ChatGPTError::RateLimited` so the run stops cleanly — the user resumes
later from the same incremental checkpoint.

## Sample data

A curated TNG-themed fixture lives at `tests/fixtures/chatgpt_api/`
and is exposed through the Bazel `tng_fixture` filegroup. The
translator's `chatgpt_render` golden test reads it directly.
