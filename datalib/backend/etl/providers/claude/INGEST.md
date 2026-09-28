# Claude: download

The `claude` source has **two ingest methods**, one table each on the
ingest step's params, sharing one renderer:

| method table               | ingest wave                              | needs credentials |
|----------------------------|--------------------------------------------|-------------------|
| `[steps.params.api]`       | walks the live `claude.ai` API             | yes               |
| `[steps.params.export]`    | reads an unpacked bulk export off its `path` | no              |

Both write **the same six tables of the same raw store** — `users`,
`orgs`, `projects`, `project_docs`, `conversations`,
`claude_attachments` — which is the whole point: `render` has one
input shape to be correct against, and there is exactly one parser.

The store is `<data_root>/<group>/ingest/entities.doltlite_db`, with a
`<table>_bookkeeping` sidecar per table and the blob CAS beside it; the
schema is `src/ingest/schema_raw.rs`. `claude-ingest --out <dir>` runs
the `api` method from the command line into `<dir>/entities.doltlite_db`.

## Why "export shape" if we hit the live API?

The parser is written against the bulk-export shape, so an API-fetched
conversation is coerced into it on its way out of the store, by
[`normalize::normalize_to_export_shape`](src/ingest/normalize.rs):

  * Inserts a synthetic `account: { uuid }` (live API omits this).
  * Backfills `message.text` from `content[].text` /
    `content[].thinking` via `synthesize_message_text`.
  * Restores `flags: null` on every content block.
  * Adds `_source: { via: "claude.ai/api", org_uuid }` provenance.

What goes *in* is the API response with every null-valued object key
dropped (`canonicalize_conversation_payload`). claude.ai's replicas do
not agree on whether a field with no value is sent as `null` or left
out — the same untouched conversation came back both ways five minutes
apart, on `chat_messages[].content[]` down to
`display_content.link.*` — and either spelling reads the same
everywhere here, so storing one of them is what keeps a no-change
refetch from counting as `modified` and re-rendering. Null array
elements stay; they are positional. The export ingest stores its file
as written: one export comes from one serializer.

## Auth + Cloudflare

The downloader never handles claude.ai cookies. It shells out to
[`latchkey curl`](https://github.com/imbue-ai/latchkey), which injects
the `sessionKey` cookie registered under the `claude-ai` service.

`claude.ai` is fronted by Cloudflare's managed challenge, so every
request goes out through the bundled Chrome-impersonating curl. Leave
`LATCHKEY_CURL` unset and the downloader finds it; setting it by hand is
in [`docs/dev/curl_impersonate.md`](/docs/dev/curl_impersonate.md). A
Chrome handshake is never escalated to the challenge that issues a
`cf_clearance` cookie, so `sessionKey` is the whole credential. If a
tightening upstream ever changes that, copy `cf_clearance` from
DevTools → Application → Cookies → `claude.ai` (HttpOnly) and add it
through `$(pbpaste)`, so it stays out of shell history:

```sh
latchkey auth set claude-ai -H "Cookie: cf_clearance=$(pbpaste)"
```

## API surface used

| Path                                                                | Purpose                            |
|---------------------------------------------------------------------|------------------------------------|
| `/account`                                                          | The account the run mirrors        |
| `/organizations`                                                    | Enumerate orgs the user belongs to |
| `/organizations/{org}/chat_conversations`                           | Per-org conversation listing       |
| `/organizations/{org}/chat_conversations/{id}?tree=True&rendering_mode=messages&render_all_tools=true&consistency=strong` | Full conversation with all blocks  |
| `/organizations/{org}/projects`                                     | Per-org project listing            |
| `/organizations/{org}/projects/{id}/docs`                           | One project's knowledge documents  |

All paths are under `https://claude.ai/api`.

A `403` on the conversation listing means "no chat permission for this
org": the org is counted (`forbidden_orgs`), reported as one `problems`
row, and skipped. Same for the project listing. A `403` on a detail
fetch is retried twice first (0.5 s, then 2 s), because claude.ai
answers 403 now and then to a detail GET issued right after the
listing, and the same UUID a moment later returns 200.

## Projects

Claude Projects ride the same source type, the same credentials and the
same raw store as conversations. `api.projects` (default **on**) turns
the walk on and off; `api.project_uuids` narrows it to a named set
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
`sync_scope_state` (`claude:sweep:project_docs:<uuid>`) with a
`PROJECT_DOCS_TTL` of 24h. Docs are refetched when the project's
metadata changed, when no sweep has ever completed, or when the last one
aged out — worst case one extra request per project per day.

A reset (`datalib-dag --reset`) empties `sync_scope_state` with the
rest, so the next sync sweeps every project again;
`tests/claude_tests/reset_and_resync.rs` pins that the rows come back identical.

**Project deletions are not mirrored.** A project or knowledge document
removed upstream keeps its row (and keeps rendering): the project walk
only upserts what the listing returns. A reset (`datalib-dag --reset`)
is the way to drop them. A UUID in
`api.project_uuids` that matches nothing in any visible org logs
`claude_project_uuid_not_found` rather than quietly mirroring
nothing.

## The `export` method: ingesting a bulk export

`[steps.params.export] path = …` names the directory you unpacked
Anthropic's data export into:

```
<path>/
  users.json            # array of accounts (optional)
  conversations.json    # array of conversations, in export shape
  projects/*.json       # one Claude Project per file, `docs` nested
```

The ingest step with that table (
[`src/ingest/export.rs`](src/ingest/export.rs)) reads those files
and writes the same rows the API walk writes: `users` from
`users.json`, `conversations` from `conversations.json`, and each
project split into a `projects` row plus one `project_docs` row per
nested knowledge document — the same split the API gets from its two
separate endpoints. Render then reads the store exactly as it does for
the `api` method, and each run is a `sync_runs` row like any download's.

A bulk export is a complete snapshot, so an id it stops mentioning has
been deleted upstream: after upserting, the ingest drops the rows the
export no longer names. Pruning is per table and runs only when that
table's file is present, so a partially unpacked export can't wipe the
store.

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
the `export` method deliberately does not have;
`chat_messages[*].attachments[]` carry their text inline and have no
bytes to fetch at all (see the next section). So there is nothing on
disk to content-address, and `claude_attachments` stays empty for an
export-backed store. If Anthropic ever ships the binaries inside the
export, `src/ingest/export.rs` is where the CAS walk goes.

## Attachments: `files[]` vs `attachments[]`

Each message in a conversation has **two** distinct attachment
slots, which Claude exposes as separate JSON arrays. They look
similar but they are not interchangeable, and the bytes-at-rest
treatment differs.

| Slot | What it carries | Ingest | Render |
|---|---|---|---|
| `chat_messages[*].files[]` | A downloadable upload — image, PDF, … Has `file_uuid`, `file_name`, `preview_url`, `document_asset.url`. | `fetch_files_for` → `download_one_file` → the blob CAS, with a `claude_attachments` edge from the conversation's `file_uuid` to the bytes. | chat-common materializes it by `file_uuid`: an image inline, anything else as a link. |
| `chat_messages[*].attachments[]` | **Text** Claude extracted from an upload: `id`, `file_name`, `file_type`, `file_size`, `extracted_content`. **No `preview_url`** — the binary is not retained server-side. | Nothing to fetch; no edge row. | `render_extracted_attachment`: a quoted block headed `**[attachment: <name>]**`. |

An `attachments[]` item has no CAS edge because there are no bytes to
address: its content is already in `conversations.payload`. If Claude
ever starts keeping those binaries (a download URL appears in the
payload), they would get edges like `files[]`.

## Resume + prioritization

There is no checkpoint file. On each run the downloader classifies
every listing item. Items whose listing `updated_at` predates the
configured `since` (`api.since` / CLI `--since`; RFC 3339 or
`YYYY-MM-DD`, assumed UTC) are out of scope: they are never
detail-fetched and are invisible to overlap selection. The filter only
gates fetching — already-stored rows are untouched — so moving `since`
further back later backfills the newly-in-scope conversations as
"missing" on that run. Everything in scope is classified into one of:

  1. **missing** — no row in `conversations` yet.
  2. **stale** — a row exists, but its stored `updated_at` differs from
     the listing's. Also where an **overlap** item lands: the N
     most-recently-updated conversations (`refresh_most_recent_n_chat_count`,
     CLI `--overlap`) are forced into this bucket regardless of
     `updated_at`, as a sanity check against the live copy.
  3. **up to date** — stored `updated_at` matches the listing's. Skipped.

The per-org work queue is `missing` first, then `stale`, so genuinely
new conversations are fetched first. The comparison is against the
`conversations` table and nothing else, which is what makes
bootstrapping from an export work (next section).

`/chat_conversations` returns an org's whole list in one response, so a
conversation the store holds for that org that the listing does not
name was deleted on claude.ai, and the walk deletes it
(`prune_org_conversations`; the row stays in doltlite history). The
prune reads the unfiltered listing, not the `since`-narrowed one, and
never touches a row whose `org_uuid` is NULL (an export-ingested one)
or an org whose listing was refused.

## Bootstrapping from an export, then keeping it fresh with the API

**Not built, and not quite safe to do by hand yet — read the hazard at
the end before trying it.** Written down because it is the obvious thing
to want: seed years of history from a bulk export (which needs no
credentials and no rate limit), then let the `api` method keep it
current.

Because both methods write the same tables of the same store, most
of this already works:

  * **Incrementality falls out for free.** The API listing pass compares
    each conversation's stored `updated_at` against the listing's, and
    the export ingest fills that column from the export payload. So an
    export-seeded store is already "up to date" for everything that has
    not changed since the export was taken — the first API run fetches
    only what actually moved, not the whole account.
  * **Identity survives the switch.** `grid_rows.uuid` is minted from
    Anthropic's own conversation UUID under the group id
    (`docs/dev/entity_ids.md`), and neither changes when the ingest step
    swaps `export` for `api`, so a conversation keeps the same id, the
    same rendered path, and its feedback history.

Two things stay half-filled, and both come from the same root: a row is
only ever enriched when the API *detail-fetches* it, and the whole point
above is that it mostly won't.

  * **`org_uuid` / `org_name`.** An export carries no organization, so
    those columns are NULL on export-ingested rows and the Org grid
    column is empty for them. They fill in per conversation as the API
    re-fetches it. This is not wrong — the row really did come from
    somewhere with no org — but a mixed store shows Org only for the
    part the API has touched.
  * **Attachments.** Export rows have no CAS edges (there are no bytes
    in an export to hash), so their attachments keep rendering as
    un-fetched until the same re-fetch happens.

To force either one, make the API re-fetch: widen `api.since`, raise
`refresh_most_recent_n_chat_count`, name the conversations in
`api.conv_uuids`, or `datalib-dag --reset` the whole store.

### The hazard: don't leave both download steps pointed at one store

The export ingest treats the export as a complete snapshot and **prunes
every conversation the export does not mention**. That is correct when
the export is the only writer. It is destructive once the API has added
conversations the export predates: re-running the export ingest over
that store would delete exactly the rows the API just fetched.

So the bootstrap is a one-way door — ingest the export, then
replace the ingest step's `export` table with `api` and don't run the
export ingest against that store again. (The step refuses a config
naming both, which is what keeps the door one-way.)

Making it a supported configuration means teaching the export prune
whose rows it owns. The API prune already does: it leaves NULL-`org_uuid`
rows alone. The export prune has no such limit, and `users` /
`projects` / `project_docs` would need their own answer.

## Named conversations

`api.conv_uuids` (CLI `--conv-uuid`, once per target; bare UUIDs or
`https://claude.ai/chat/<uuid>` URLs) fetches exactly those
conversations instead of walking the listing, and prunes nothing. Each
org is tried in turn; a `404`, or a `403` that outlasts the retries,
means "wrong org, try the next". The rows are upserted beside what the
store already holds.

## Rate limits

Every request goes through the shared `latchkey_curl` chokepoint, which
retries a `429` or `502`–`504`, honoring `Retry-After`, within the
source's `download_params` give-up bounds. When it gives up, the
request fails as `ClaudeError::Permanent`, like any other error.

## Sample data

A curated TNG-themed fixture lives at `tests/fixtures/claude_export/`
and is exposed through the Bazel `tng_fixture` filegroup.
