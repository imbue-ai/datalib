# Claude: download

The `claude` source has **two ingest methods**, one table each on the
ingest step's params, sharing one renderer:

| method table               | ingest wave                              | needs credentials |
|----------------------------|--------------------------------------------|-------------------|
| `[steps.params.api]`       | walks the live `claude.ai` API             | yes               |
| `[steps.params.export]`    | reads an unpacked bulk export off its `path` | no              |

Both write **the same tables of the same raw store** — `users`,
`orgs`, `projects`, `project_docs`, `conversations`,
`claude_attachments`, and for the `api` method `project_docs_listings`
— which is the whole point: `render` has one
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

Cloudflare sits in front of `claude.ai`; requests clear its managed
challenge by going through the bundled curl that impersonates Chrome's
TLS handshake ([`docs/dev/curl_impersonate.md`](/docs/dev/curl_impersonate.md)
has the pieces and `LATCHKEY_CURL`). That handshake is never asked for a
`cf_clearance` cookie, so `sessionKey` is the whole credential. Should
Cloudflare start demanding one, take `cf_clearance` (HttpOnly) from
DevTools → Application → Cookies → `claude.ai` and store it via
`$(pbpaste)`, keeping it out of shell history:

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

An org whose `capabilities` list is there and leaves out `chat` — an
API-console org, `["api", …]` — is not walked at all: no listing, no
project, no detail is asked of it, it is counted (`non_chat_orgs`) and
logged, and it is not a problem. Nothing of it is pruned either, so a
conversation the store holds for it stays. An org with no
`capabilities`, or something other than a list there, is walked.

A `403` on a walked org's conversation listing means "no chat permission
for this org": the org is counted (`forbidden_orgs`), reported as one
`problems` row (`listing:org:<org uuid>`, its name in the text, since two
orgs of one account can share a name), and skipped. Same for the project
listing. A `403` on a detail fetch is retried twice first (0.5 s, then
2 s), because claude.ai answers 403 now and then to a detail GET issued
right after the listing, and the same UUID a moment later returns 200.

## When part of a sync fails

Two things fail the step: the `/organizations` listing failing (it is
the credential preflight, and with no orgs there is nothing to walk),
and no org's conversation listing answering at all — every org refused
or failed, which is a credential that stopped working inside the org
listing's 6h cache, not a partial sync. Nothing is pruned on such a
run. Every other failure is a `problems` row, and the sync goes on with
what it has. A store that will not take a read or a write still fails
the step.

| what failed | row | cleared by |
|---|---|---|
| `/account`, with no user stored | `phase:account` | the next run, which asks again while there is no user |
| one org's conversation listing (not a 403) | `listing:conversations org:<org uuid>` | the next run that lists it; until then nothing of that org is pruned |
| one org's project listing | `listing:projects org:<org uuid>` | the next run that lists it |
| one project's docs listing | `project_docs_listings:<project uuid>` (a warning for a 403, or when docs from an earlier listing are held) | the next listing of them: a listing that failed is not held, so it is due again whatever its sweep marker says |
| a configured `project_uuids` entry no listed org has | `config:project_uuids:<value>` | a run in which it matches, or the config dropping it |
| a configured `conv_uuids` entry every org answers 404 or 403 for | `config:conv_uuids:<value>` | as above |
| one conversation's detail fetch | `conversations:<id>` on its bookkeeping row | its next successful fetch: a conversation not held at the `updated_at` listed is owed, so the next run asks again |
| one file | `claude_attachments:<conversation>#<file>`, with the real reason | the file landing; see Attachments |
| the rate limit (the shared give-up guard tripped), or 25 failures in a row in one loop | `phase:conversations`, `phase:projects` or `phase:attachments`, wherever it stopped | the next run that gets through; the walk stops there, since every later request would be refused too |
| a conversation or project in a bulk export with no `uuid` | `phase:export` | the next export ingest without one |

Render shows a `conversations:` or `claude_attachments:` row on the
conversation's page. A `404` on the detail of a conversation the
listing names is a failure like any other: what the mirror holds stays,
and only a listing that leaves the conversation out deletes it. The `listing:`/`phase:` rows and the `config:` rows
of a run that got to its end replace the last run's, on both the listing
path and the `conv_uuids` path. A run the rate limit cut short adds its
`listing:`/`phase:` rows and clears none, and does not rewrite the
`config:` rows, since it did not check every configured entry. A run
that was asked to stop clears none either, and records nothing about a
request the stop refused. What it did not reach is still owed.

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

The project listing is refetched every run — one request per org — and
it is the whole of a project's metadata: a project is held, in its
sidecar, at the `updated_at` the listing names, and one held at that
stamp is not written again. Knowledge docs are a listing of their own,
`…/projects/{id}/docs`, one row per project in `project_docs_listings`
whose sidecar holds the project `updated_at` the docs were listed for.
They are written in one transaction with the docs and the sweep marker.

**We have not confirmed that editing a document bumps its project's
`updated_at`**, so the docs are owed when either is true: the listing is
not held at the project's current `updated_at` (never listed, failed,
cut off, or the project changed), or its sweep marker in
`sync_scope_state` (`claude:sweep:project_docs:<uuid>`) is older than
`PROJECT_DOCS_TTL`, 24h — worst case one extra request per project per
day. The `/organizations` listing sits behind the same kind of marker
(`claude:sweep:orgs`, 6h), written with the orgs it lists. Both are
stamped with, and aged against, the run's pinned now
(`DATALIB_DAG_NOW`), not the wall clock, so whether a run asks for a
listing is the same on every replay of it.

A reset (`datalib-dag --reset`) empties `sync_scope_state` with the
rest, so the next sync sweeps every project again;
`tests/claude_tests/reset_and_resync.rs` pins that the rows come back identical.

**Project deletions are not mirrored.** A project or knowledge document
removed upstream keeps its row (and keeps rendering): the project walk
only upserts what the listing returns. A reset (`datalib-dag --reset`)
is the way to drop them. A UUID in
`api.project_uuids` that matches nothing in any visible org is a
`config:project_uuids:<value>` problem rather than quietly mirroring
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
the `api` method.

Every run reads the whole export, so a row's `_bookkeeping` sidecar is
stamped the first time a run reads the row and left alone after (its
`held_version` still follows the export's `updated_at`), and a run
keeps no `sync_runs` row. Reading an unchanged export again commits
nothing (`reading_an_unchanged_export_again_commits_nothing`).

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
| `chat_messages[*].files[]` | A downloadable file — an upload (image, PDF, …) or one Claude's sandbox made (`file_kind: "blob"`, which names no URL). Has `file_uuid`, `file_name`, and for an upload `preview_url` and `document_asset.url`. | The file loop → the blob CAS, with a `claude_attachments` edge from the conversation's `file_uuid` to the bytes. | chat-common materializes it by `file_uuid`: an image inline, anything else as a link. |
| `chat_messages[*].attachments[]` | **Text** Claude extracted from an upload: `id`, `file_name`, `file_type`, `file_size`, `extracted_content`. **No `preview_url`** — the binary is not retained server-side. | Nothing to fetch; no edge row. | `render_extracted_attachment`: a quoted block headed `**[attachment: <name>]**`. |

An `attachments[]` item has no CAS edge because there are no bytes to
address: its content is already in `conversations.payload`. If Claude
ever starts keeping those binaries (a download URL appears in the
payload), they would get edges like `files[]`.

The edges are written in the conversation's transaction, one per
`file_uuid` its messages name, with no `blake3`; a refetch that no
longer names a file drops its edge. The file loop then fetches every
edge not held at its conversation's version, from the file object the
conversation carries (`fetchers::file_url`): a document from its
`document_asset.url`, which is the upload's exact bytes, and every
other file from `/api/organizations/{org}/files/{file_uuid}/contents`,
which serves a picture at full resolution and a sandbox file whole. A
picture's `preview_url` is a re-encoded webp, so it is not used. Bytes
the CAS already holds for the file, under any conversation, are not
fetched again. A file that does not land is
owed, its bookkeeping and its `problems` row saying why, and the next
run asks again.

A file claude.ai answers `404` or `410` for, or one in a conversation
that names no org, is not there to fetch rather than failed: its edge is held with
a `not_found` warning, and asked for again only when its conversation
changes and is refetched.

## What is owed

There is no checkpoint file and no cursor. Each run lists every org's
conversations, and a listed conversation is owed when the store does
not hold it at the `updated_at` listed: the sidecar's `held_version`,
written in the transaction that stores the row and its attachment
edges, so a run cut off anywhere leaves nothing reading as done that
is not (`tests/claude_tests/interrupt.rs` cuts a replayed run at every
request and requires the store an uninterrupted run leaves). The ones
never fetched come first, then the stale, so genuinely new
conversations are fetched first. The N most recently updated
conversations of each org (`refresh_most_recent_n_chat_count`, CLI
`--overlap`) are fetched every run, held or not, as a check against the
live copy.

Items whose listing `updated_at` predates the configured `since`
(`api.since` / CLI `--since`; RFC 3339 or `YYYY-MM-DD`, assumed UTC)
are out of scope: they are never detail-fetched and are invisible to
the overlap. The filter only gates fetching — already-stored rows are
untouched — so moving `since` further back later lists the newly in
scope conversations as owed on that run.

`/chat_conversations` returns an org's whole list in one response, so a
conversation the store holds for that org that the listing does not
name was deleted on claude.ai, and the walk deletes it
(`prune_org_conversations`; the row stays in doltlite history). The
prune reads the unfiltered listing, not the `since`-narrowed one, and
never touches a row whose `org_uuid` is NULL (an export-ingested one),
an org whose listing was refused or failed, or an org with no chat. A pruned conversation
takes its attachment edges, their bookkeeping and the `problems` rows
of both with it.

A conversation whose every fetch failed is an id-only stub — no
payload, so no org. When every walked org listed, a stub no listing
names is pruned too, problem and all; otherwise stubs wait for a run in
which they all did. An org with no chat lists nothing, so it holds no
stub back.

## Bootstrapping from an export, then keeping it fresh with the API

**Not built, and not quite safe to do by hand yet — read the hazard at
the end before trying it.** Written down because it is the obvious thing
to want: seed years of history from a bulk export (which needs no
credentials and no rate limit), then let the `api` method keep it
current.

Because both methods write the same tables of the same store, most
of this already works:

  * **Incrementality falls out for free.** The API walk owes what its
    sidecars do not hold at the listed `updated_at`, and the export
    ingest holds each conversation at the `updated_at` its payload
    carries. So an export-seeded store is already "up to date" for
    everything that has not changed since the export was taken — the
    first API run fetches only what actually moved, not the whole
    account.
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
conversations, every run, instead of walking the listing, and prunes
nothing. Each
org is tried in turn; a `404`, or a `403` that outlasts the retries,
means "wrong org, try the next". The rows are upserted beside what the
store already holds.

## Rate limits

Every request goes through the shared `latchkey_curl` chokepoint, which
retries a `429` or `502`–`504`, honoring `Retry-After`, within the
source's `download_params` give-up bounds. When it gives up, the
request fails as `ClaudeError::RateLimited` and the run does no more
requests; see the table above.

## Sample data

A curated TNG-themed fixture lives at `tests/fixtures/claude_export/`
and is exposed through the Bazel `tng_fixture` filegroup. Its
`files/<file_uuid>.<ext>` hold the bytes the synthesizer serves where a
download asks for them (`fetchers::file_url`).
