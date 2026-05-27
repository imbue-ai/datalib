# Anthropic Extract

`anthropic-download` incrementally mirrors `claude.ai` conversations
into a local JSON cache that matches Anthropic's bulk-export shape so
the existing translator consumes either source indistinguishably:

```
<out>/
  conversations.json    # array of conversations in export shape
  users.json            # copied from --export-dir if present
```

## Why "export shape" if we hit the live API?

The bulk-export format is deprecated upstream, but the on-disk shape
is stable and the parser layer is already written against it. The
downloader fetches from `https://claude.ai/api` and runs each
response through [`normalize::normalize_to_export_shape`](src/extract/normalize.rs)
to coerce it into the export format:

  * Inserts a synthetic `account: { uuid }` (live API omits this).
  * Backfills `message.text` from `content[].text` /
    `content[].thinking` via `synthesize_message_text`.
  * Restores `flags: null` on every content block.
  * Adds `_source: { via: "claude.ai/api", org_uuid }` provenance.

## Auth + Cloudflare

The downloader does not handle claude.ai cookies directly. It shells
out to [`latchkey curl`](https://github.com/imbue-ai/latchkey), which
injects the cookies registered under the `claude-ai` service.

`claude.ai` is fronted by Cloudflare's managed-challenge system. To
clear the challenge, point `LATCHKEY_CURL` at a Chrome-impersonating
curl. The simplest option is the in-tree `latchkey-curl-shim` bin
(a `wreq`-backed shim, mirror of
`src/download/latchkey_curl_shim.py`):

```sh
cargo build -p frankweiler-etl --bin latchkey-curl-shim
export LATCHKEY_CURL="$(pwd)/frankweiler/backend/target/debug/latchkey-curl-shim"
anthropic-download --out ~/backups/claude_api
```

A standalone `curl-impersonate` binary works too — point
`LATCHKEY_CURL` at it instead.

### Why no `cf_clearance` cookie?

Cloudflare gates clients in two layers: the TLS fingerprint (JA3/JA4)
and, when that looks suspect, a JS challenge that issues a
`cf_clearance` cookie. The shim's Chrome 131 handshake (boring-ssl
with real-Chrome cipher ordering / ALPN / extensions) keeps us on
the green path, so `cf_clearance` is never issued — and not needed
in the latchkey credential set. The `sessionKey` cookie is the full
auth surface. If a future CF tightening flips us into challenge
land, grab `cf_clearance` from DevTools → Application → Cookies →
`claude.ai` (HttpOnly), copy its value to the clipboard, and add it
via `$(pbpaste)` so the cookie doesn't land in shell history:

```sh
latchkey auth set claude-ai -H "Cookie: cf_clearance=$(pbpaste)"
```

## API surface used

| Path                                                                | Purpose                            |
|---------------------------------------------------------------------|------------------------------------|
| `/organizations`                                                    | Enumerate orgs the user belongs to |
| `/organizations/{org}/chat_conversations`                           | Per-org conversation listing       |
| `/organizations/{org}/chat_conversations/{id}?tree=True&rendering_mode=messages&render_all_tools=true&consistency=strong` | Full conversation with all blocks  |

`403` on the listing endpoint is treated as "no chat permission for
this org" — we count it and continue rather than abort.

## Resume + prioritization

There is no checkpoint file. On each run the downloader classifies
every listing item into one of:

  1. **new** — not in either the API cache or the export seed.
  2. **overlap** — one of the N most-recently-updated export
     conversations (controlled by `--overlap`, default 3); refetched
     as a live-vs-export sanity check.
  3. **updated** — in the API cache but with a different `updated_at`.
  4. **export-stale** — in the export seed but not the API cache, and
     the export's `updated_at` is stale.

Everything else is skipped. The per-org work queue is sorted by
priority ascending so genuinely-new conversations are fetched first.

## Single-conversation mode

Pass `--conv-uuid <UUID>` to fetch one specific conversation instead
of walking the listing. Each org is tried in turn; `403`/`HTTP 404`
on `get_conversation` are treated as "wrong org, continue". The
result is merged into the existing `conversations.json`, so prior
cache entries are preserved.

```sh
export LATCHKEY_CURL=/path/to/curl_impersonate-chrome
anthropic-download --out ~/backups/claude_api \
    --conv-uuid 12345678-90ab-cdef-1234-567890abcdef
```

## Rate limits

`claude.ai` doesn't 429 us in practice today, so `api::ClaudeClient`
is a single-shot shell-out without a backoff loop. If that ever
changes, model the loop on `chatgpt/src/extract/api.rs`.

## Sample data

A curated TNG-themed fixture lives at `tests/fixtures/anthropic_api/`
and is exposed through the Bazel `tng_fixture` filegroup.
