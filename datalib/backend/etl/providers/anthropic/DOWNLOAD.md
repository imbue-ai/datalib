# Anthropic Extract

This provider serves **two source types** that share one renderer:

| type            | download wave                                      | needs credentials |
|-----------------|----------------------------------------------------|-------------------|
| `claude_api`    | walks the live `claude.ai` API                     | yes               |
| `claude_export` | reads an unpacked bulk export off disk             | no                |

Both write **the same six tables of the same raw store** — `users`,
`orgs`, `projects`, `project_docs`, `conversations`,
`anthropic_attachments` — which is the whole point: `render` has one
input shape to be correct against, and there is exactly one parser.

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
response through [`normalize::normalize_to_export_shape`](src/download/normalize.rs)
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
curl. The simplest option is the in-tree `latchkey-curl-impersonate` bin
(a `wreq`-backed shim, mirror of
`src/download/latchkey_curl_impersonate.py`):

```sh
bazelisk build //datalib/backend/etl:latchkey_curl_impersonate
export LATCHKEY_CURL="$(pwd)/bazel-bin/datalib/backend/etl/latchkey_curl_impersonate"
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
| `/organizations/{org}/projects`                                     | Per-org project listing            |
| `/organizations/{org}/projects/{id}/docs`                           | One project's knowledge documents  |

`403` on the listing endpoint is treated as "no chat permission for
this org" — we count it and continue rather than abort. Same for the
project listing ("no project permission for this org").

## Projects

Claude Projects ride the same source type, the same credentials and the
same raw store as conversations. `sync.projects` (default **on**) turns
the walk on and off; `sync.project_uuids` narrows it to a named set
(bare UUIDs or paste-able `https://claude.ai/project/<uuid>` URLs) — the
per-org listing still runs, since that is one request and it is where
the metadata comes from.

Two tables: `projects` (the listing entry, `org_uuid` / `org_name` /
`name` / `updated_at` promoted out of the payload) and `project_docs`
(one row per knowledge document, `project_uuid` promoted).

### Why knowledge docs never touch the CAS

`…/projects/{id}/docs` returns each document's **full text inline** in
`content`. There is no `preview_url`, no second fetch, and no binary
retained server-side — Claude keeps only its own text extraction. Same
shape as `chat_messages[*].attachments[]` (see the table above), and the
same conclusion: nothing to put in the blob CAS, so `project_docs` is a
plain payload table rather than a CAS edge.

One consequence worth knowing: Claude extracts text from *any* upload,
so a project whose "knowledge document" is a 500-page EPUB stores half a
megabyte of pandoc-flavored markup. The raw store keeps all of it; the
**render** step clamps what reaches the page and the grid row at
`max_project_doc_bytes` (render-step param, default 128 KiB) and appends
a visible truncation marker. Raising it and re-rendering backfills.

### Incrementality

The project listing is refetched every run — one request per org — and a
project whose `updated_at` matches the stored row is not re-written.
Knowledge docs are the awkward part: **we have not confirmed that
editing a document bumps its project's `updated_at`**, and if it does
not, an `updated_at`-only rule would let docs go stale forever. So the
docs listing sits behind a per-project sweep marker in
`sync_scope_state` (`anthropic:sweep:project_docs:<uuid>`) with a
`PROJECT_DOCS_TTL` of 24h. Docs are refetched when the project's
metadata changed, when no sweep has ever completed, or when the last one
aged out — worst case one extra request per project per day.

`--reset-and-redownload` truncates the data tables but not
`sync_scope_state`, so the docs refetch after a reset is driven by the
metadata skip-check finding an empty `projects` table, not by the TTL.
`tests/reset_and_redownload.rs` pins that.

**Deletions are not mirrored.** A project or knowledge document removed
upstream keeps its row (and keeps rendering) — the walk only ever
upserts what the listing returns, same as conversations. A
`--reset-and-redownload` is the way to drop them today. A UUID in
`sync.project_uuids` that matches nothing in any visible org logs
`anthropic_project_uuid_not_found` rather than quietly mirroring
nothing.

## `claude_export`: ingesting a bulk export

`type: claude_export` points `common.input_path` at the directory you
unpacked Anthropic's data export into:

```
<input_path>/
  users.json            # array of accounts (optional)
  conversations.json    # array of conversations, in export shape
  projects/*.json       # one Claude Project per file, `docs` nested
```

The download step (`datalib-step download claude_export`,
[`src/download/export.rs`](src/download/export.rs)) reads those files
and writes the same rows the API walk writes: `users` from
`users.json`, `conversations` from `conversations.json`, and each
project split into a `projects` row plus one `project_docs` row per
nested knowledge document — the same split the API gets from its two
separate endpoints. Then render reads the store, exactly as it does for
`claude_api`.

This replaced a renderer that walked the export tree in place
(issue #207). What that bought:

  * **One input shape.** There is no second parser to keep in agreement
    with the doltlite one, and the golden test now exercises the path
    production actually runs.
  * **Deletions.** A bulk export is a complete snapshot, so an id it
    stops mentioning has been deleted upstream. After upserting, the
    ingest drops the rows the export no longer names. Pruning is
    per-table and only runs when that table's file was actually present,
    so a partially-unpacked export can't wipe the store. This is the one
    place in this provider that removes rows; the API walk deliberately
    does not (a listing can omit a conversation for reasons other than
    deletion).
  * **The ordinary bookkeeping.** A `sync_runs` row per ingest —
    `started_at` / `finished_at` / `elapsed_ms` / `status`, the
    `deltas` summary — plus `dolt_diff`-driven incremental render, none
    of which a directory read in place could provide.

### Org columns

`conversations.org_uuid` / `org_name` stay **NULL** for an
export-ingested row. An export carries no organization anywhere; only
the API walk learns one, from `/organizations`. That NULL is
load-bearing rather than merely absent: `render::parse::parse_loaded`
reads it as "this payload is already export-shaped" and skips
`normalize_to_export_shape`, which would otherwise stamp the row
`_source: {via: "claude.ai/api", org_uuid: ""}` — a lie about its
provenance and an empty org on every grid row. A conversation whose
payload happens to carry its own `_source.org_uuid` (an export produced
from our own API mirror) still gets its org onto the grid row: that is
read from the payload, not the column.

Projects are the other way round — the render side reads a project's
org from the **column** — so `_source.org_uuid` / `_source.org_name`
are lifted out of the project payload at ingest time when the file has
them.

### No blob CAS

A Claude bulk export ships JSON only. `chat_messages[*].files[]` name a
`preview_url` back on claude.ai, and fetching it needs the credentials
this source type deliberately does not have;
`chat_messages[*].attachments[]` carry their text inline and have no
bytes to fetch at all (see the next section). So there is nothing on
disk to content-address, and `anthropic_attachments` stays empty for an
export-backed store. If Anthropic ever ships the binaries inside the
export, `src/download/export.rs` is where the CAS walk goes.

## Attachments: `files[]` vs `attachments[]`

Each message in a conversation has **two** distinct attachment
slots, which Claude exposes as separate JSON arrays. They look
similar but they are not interchangeable, and the bytes-at-rest
treatment differs.

| Slot                              | What it carries                                                                                              | Extract action                                                                  | Translate rendering                                                  |
|-----------------------------------|--------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------|----------------------------------------------------------------------|
| `chat_messages[*].files[]`        | Downloadable upload — image / PDF / etc. Has `file_uuid`, `file_name`, `preview_url`, `document_asset.url`. | Walk via `fetch_files_for` → `download_one_file` → blob CAS (`db.store_blob`). | Per-conversation `BlobBundle::load` + provider-local `attachment_md` → `![alt](blobs/<hash>.<ext>)` for images, `[\[file\] alt](blobs/<hash>.<ext>)` otherwise. |
| `chat_messages[*].attachments[]`  | **Text** Claude pre-extracted from a user upload. Carries `id`, `file_name`, `file_type`, `file_size`, and `extracted_content`. **No `preview_url`** — the binary is not retained server-side. | **Skipped.** There is no resource to fetch.                                     | `render_extracted_attachment` → inline blockquote with a `**[attachment: <name>]**` header.                          |

This split was confirmed by querying a live raw store with the
doltlite CLI: every "attachment-not-yet-fetched" placeholder in the
old goldens turned out to be an `attachments[]` item with
non-empty `extracted_content` and no download URL — exactly what
the schema docs describe but easy to miss in code review.

**Why extract doesn't pre-seed `blob_refs` rows for
`attachments[]`**: `blob_refs` is a cache index over the CAS (see
[`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md)
§"Blobs and the CAS split"). The `attachments[]` content lives
inline in `conversations.payload` as `extracted_content`; there's
no separate fetch, no skip-check semantics, and no bytes to land
in the CAS. Same shape as contacts photos
(§"Why contacts doesn't participate"). The id is per-message
slot bookkeeping, not a cache key.

**Future-work note**: if Claude ever starts retaining the original
binaries for `attachments[]` items (i.e. a download URL appears in
the payload), the durable-evidence pattern would have us pre-seed
a `blob_refs` row per attachments[] id with `blake3=NULL` and
`last_error="no_download_url"`, so a later "rescan when bytes
become available" pass has something to walk. Until then,
recording these in `blob_refs` would muddy the cache-index
semantics for zero benefit.

## Resume + prioritization

There is no checkpoint file. On each run the downloader classifies
every listing item. Items whose listing `updated_at` predates the
configured `since` (config `sync.since:` / CLI `--since`; RFC 3339 or
`YYYY-MM-DD`, assumed UTC) are out of scope: they are never
detail-fetched and are invisible to overlap selection. The filter only
gates fetching — already-stored rows are untouched — so moving `since`
further back later backfills the newly-in-scope conversations as
"new" on that run. Everything in scope is classified into one of:

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
