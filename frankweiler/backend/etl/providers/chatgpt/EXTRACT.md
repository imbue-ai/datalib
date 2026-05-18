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
injects the cookies registered under the `chatgpt` service.

`chatgpt.com` is fronted by Cloudflare's managed-challenge system,
which fingerprints TLS handshakes. To clear the challenge, point
`LATCHKEY_CURL` at a `curl-impersonate` build before running:

```sh
export LATCHKEY_CURL=/path/to/curl_impersonate-chrome
chatgpt-download --out ~/backups/chatgpt_api
```

Both `latchkey` and `curl_impersonate-chrome` must be on `PATH` /
referenced by absolute path; the binary fails loudly if `latchkey`
isn't found.

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

## Rate limits

ChatGPT returns `429` with a `Retry-After` header during enforced
backoff. `api::ChatGPTClient::get` honors that header, then falls
back to exponential backoff (5 · 2^n seconds, capped at 60s) if
absent. After `RATE_LIMIT_GIVE_UP_AFTER` (300s) of cumulative wait
without a 200, the request returns `ChatGPTError::RateLimited` and
the run stops — the user resumes later from the same incremental
checkpoint.

## Sample data

A curated TNG-themed fixture lives at `tests/fixtures/chatgpt_api/`
and is exposed through the Bazel `tng_fixture` filegroup. The
translator's `chatgpt_render` golden test reads it directly.
