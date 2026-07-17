# Notion Extract

`notion-download` mirrors Notion pages via the public `api.notion.com/v1`
API, with optional inbox discovery via the unofficial
`www.notion.so/api/v3/getNotificationLog` endpoint when the public API
isn't enough (it has no notifications equivalent).

Output is an **event-store JSONL** layout — one append-only stream per
entity, keyed by stable Notion ID, so reruns are incremental and
self-describing:

```
<out>/
  notion_official_page/{created,updated}/events.jsonl     # page records
  notion_official_block/{created,updated}/events.jsonl    # block records
  notion_official_comment/{created,updated}/events.jsonl  # comment records
```

Each line is `{ "key": {...}, "raw": {...}, "ts": "<iso8601>" }`. New
ids land in `created/`, ones whose key fields change get appended to
`updated/`. The `key` is a minimal stable identity (id +
last_edited_time, etc.) used for change detection; `raw` carries the
full untouched Notion payload for the translate stage to chew on.

## Auth

Two latchkey services must be registered. The `notion` service is
required; `notion_unofficial` is only needed for `--inbox` discovery.

| Service             | Used for                                        | Wire auth        |
|---------------------|-------------------------------------------------|------------------|
| `notion`            | `api.notion.com/v1` (pages, blocks, comments)   | Bearer token (PAT or integration token); latchkey also injects `Notion-Version` |
| `notion_unofficial` | `www.notion.so/api/v3` (getNotificationLog etc.) | Cookie session via a logged-in browser |

See `src/download/NOTION_AUTH.md` for the keyring / cookie setup.

## Cloudflare

`api.notion.com` accepts vanilla HTTP, but `www.notion.so` sits behind
Cloudflare and rejects anything without a browser TLS fingerprint.
`notion-download` shells out via `latchkey curl` and expects
`LATCHKEY_CURL` to point at a Chrome-impersonating curl. The in-tree
`latchkey-curl-shim` binary (Chrome 131 fingerprint via `wreq`) is the
canonical choice:

```sh
bazelisk build //frankweiler/backend/etl:latchkey_curl_shim
export LATCHKEY_CURL=$(pwd)/bazel-bin/frankweiler/backend/etl/latchkey_curl_shim
```

If `LATCHKEY_CURL` is unset, the downloader looks for the shim in the
standard `target/{debug,release}/` locations and auto-sets it.

## Modes

Two source modes; combine them or pass one alone.

**Subtree**: BFS-mirror one page hierarchy. Uses only the official API.

```sh
notion-download --out ~/backups/notion --subtree-page <page_id>
```

`child_page` blocks are enqueued as new BFS roots; `child_database`
blocks are recorded but not walked into. UUIDs may be dashed or
undashed.

**Inbox**: walk `getNotificationLog` per visible space, then fetch
every referenced page via the official API. Requires
`notion_unofficial`.

```sh
notion-download --out ~/backups/notion --inbox
notion-download --out ~/backups/notion --inbox --space <space_id>
```

**Single page**: fetch just one page (handy for snapshot tests).

```sh
notion-download --out ~/backups/notion --page <page_id>
```

## Reliability

`429`/`502`/`503`/`504` responses are retried (with `Retry-After` /
exponential backoff) centrally by the shared `latchkey_curl` chokepoint,
bounded by the source's `extract_params` give-up policy. Per-page errors
are logged and the BFS continues — one bad page doesn't kill the whole
mirror.

## Schema

| Entity                    | Key fields                                                                                  |
|---------------------------|---------------------------------------------------------------------------------------------|
| `notion_official_page`    | `id`, `last_edited_time`, `parent`                                                          |
| `notion_official_block`   | `id`, `page_id`, `type`, `last_edited_time`                                                 |
| `notion_official_comment` | `id`, `page_id`, `discussion_id`, `parent_block_id`, `parent_page_id`, `created_time`, `last_edited_time` |
