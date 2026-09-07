# Notion

Mirrors a Notion workspace through the official REST API
(`api.notion.com/v1`) at `Notion-Version: 2026-03-11`.

## Auth: one credential, and the version rides with it

A **personal access token**, stored under the latchkey `notion` service.
A PAT acts as the person who created it and inherits their permissions,
so nothing has to be shared with an integration — which is what lets the
mirror default to the whole workspace with no configured starting point.

The stored credential must carry **both** headers:

```sh
latchkey auth set notion \
  -H "Authorization: Bearer ntn_..." \
  -H "Notion-Version: 2026-03-11"
```

The version cannot be set per request. A second `Notion-Version` on the
wire does not override the stored one — it concatenates, and Notion
rejects the pair with `instead was "2022-06-28, 2026-03-11"`. Bumping
the version means re-running `auth set`.

Two limits worth knowing: `GET /v1/users` (list all) is unavailable to
PATs, and `latchkey auth list` reports this service's
`credentialStatus` as `invalid` even when it works — only a real request
tells you.

## What a run does

**Discovery.** Two modes.

With `sync.roots` empty — the default — the mirror is the whole
workspace, discovered through `POST /v1/search` sorted
`last_edited_time` descending, 100 at a time, **stopping at the first
result older than where the last run finished**. Page objects come back
complete, properties included, so for a database row with no body that
single response is the entire record.

That stopping point is Notion's answer to "since you last looked". The
API offers no delta token — no Gmail `historyId`, no JMAP `state` — so
the resume cursor is a timestamp, and it works only because the ordering
is trustworthy: `last_edited_time` descending was measured strictly
monotonic across 12,300 objects and 124 pages of results, with no
duplicate ids. A steady-state run therefore reads **one page of results**
rather than the workspace.

It is stored per source as `sync_scope_state.last_seen_at`, alongside a
config blob, so widening `refresh_window_days` re-examines that window
instead of being suppressed by a point recorded under the narrower
setting. It is written only after the pages land — a point recorded over
a failed pass would skip that window forever.

Do not confuse it with `start_cursor` / `next_cursor`, which page
*within* one walk and do not survive it.

With `sync.roots` set, the mirror is those pages and everything under
them, walked through the `<page>` and `<database>` links in each body.
No search, and no resume cursor: the walk is the enumeration.

**Per page**, two requests where the block walk needed one per container
block:

1. `GET /v1/pages/{id}` — properties, parent, icon, cover, `in_trash`.
2. `GET /v1/pages/{id}/markdown` — the body, already rendered.

Plus `GET /v1/comments?block_id={page_id}` when comments are enabled;
one call returns the page's whole discussion set, including threads
anchored to blocks inside it.

Measured over 12 pages of a real workspace: the old block walk needed a
median of 11 requests per page (≥60 on the deepest), 241 in total
against 36 for the same pages now.

**The block tree is not mirrored.** There is no `blocks` table and no
block renderer. Notion renders the page; we store what it returns.

## The one rule about stored markdown

**Never store a Notion file URL as returned.** Every Notion-hosted file
link is pre-signed and re-minted on each fetch, valid about an hour. Two
fetches of an unchanged page differ only in `X-Amz-Signature` and
friends — proven against a live page whose `last_edited_time` had not
moved.

Left alone, that makes an unchanged page differ from itself every run:
`dolt_diff_page_markdown` reports a modification, the page re-renders,
and the `--reset-and-redownload` stability check fails on content nobody
touched.

So every signed URL is reduced to its **slot** — scheme + host + path,
query discarded — before the body is stored (`download::slots`). The
slot is also the CAS edge's `ref_id`, because it is the one identifier
upstream keeps stable across both re-signing and a byte replacement.

The rewrite is deliberately narrow: it fires only on a Notion file host
**and** a signature parameter. A measured page carried 1,472 ordinary
links with meaningful query strings (DoorDash orders, Google Docs
`gid=`) against 26 real attachments; stripping queries indiscriminately
would have corrupted all 1,472.

## Truncation: two cases, one attribute tells them apart

A response may set `truncated: true`. Every hole leaves a marker; what
differs is whether it can be filled.

| marker | means | follow-up |
|---|---|---|
| `<unknown url="…#id"/>` — **no `alt`** | subtree too large to inline | fetch it as its own page of markdown; it resolves |
| `<unknown url="…#id" alt="button"/>` — **has `alt`** | a block type markdown cannot express | never resolves — the fetch returns a stub carrying the same id, an infinite regress |

`alt` present ⇒ record it and stop. `alt` absent ⇒ fetch and splice.
Unrepresentable types seen so far: `button`, `alias`, `drive`.

Always read `unresolved_block_ids` rather than inferring the hole set
from the text: an id can be listed with no marker in the body.

Follow-ups are capped per page (`MAX_HOLE_FOLLOWUPS`) — one measured
page wanted ~1,385 of them, which would otherwise dominate a run.

## Most pages have no body

In a random 70-page sample of a real workspace, **50 (71%) returned
empty markdown**, and 63 of the 70 were database rows. A database row's
content usually *is* its properties. Render must not treat an empty body
as nothing to render; for those pages the properties table is the
document.

## Rate limits

~3 requests/second per connection, plus a workspace-wide limit that
scales with plan. `429`/`5xx` retry with `Retry-After` is handled
centrally in `latchkey_curl`. Expect roughly one empty-body response per
130 requests on a long walk — retry covers it, but a naive loop would
silently truncate.

## Schema

`<root>/<name>/raw/entities.doltlite_db`:

| table | holds |
|---|---|
| `pages` | the page object: properties, parent, `in_trash`, timestamps |
| `page_markdown` | the body, slots not signatures |
| `comments` | one row per comment, with `page_id` and `discussion_id` |
| `comment_anchors` | the text a block-anchored comment hangs off |
| `users` | display names, resolved one id at a time |
| `notion_attachments` | CAS edge, `ref_id` = the slot |

`page_markdown` is its own table so `dolt_diff_page_markdown` means
exactly "the body changed", separate from "a property changed".

## People and anchors

Two things Notion does not hand over with the object that needs them.

**Comment authors need nothing** — every comment carries
`display_name.resolved_name`.

**Page authors do.** A page object gives only `created_by.id`, so ids
seen on pages, people properties and comments are resolved with
`GET /v1/users/{id}` and cached in `users`. One request per user, once
ever. It has to work this way: `GET /v1/users` (list all) is not
available to a personal access token. A user that cannot be read falls
back to an id prefix rather than failing the page.

**A comment names a `block_id` and carries no quote of what it is
about.** So `GET /v1/blocks/{id}` runs for **commented blocks only** —
one request per commented block, not per block (11 across 25 pages in a
measured workspace) — and the text lands in `comment_anchors`. The
thread file opens with it as a blockquote, and it leads the thread row's
searchable text. When `original_content_deleted` is set, the thread says
so instead of quoting something that no longer exists.

## Deletions

Render walks the whole raw store every run and hands the driver every
document it considered — skipped ones included — through
`RunCtx::retain_documents`. Anything the render store holds and that set
does not name is a document whose source is gone, and the driver sweeps
it.

This is the stronger of the two mechanisms the tree offers: it needs no
`dolt_diff` (which notion is not on yet) and cannot miss a deletion a
diff failed to mention. The cost is that render re-reads the local store
each run — no API requests, but real work, and porting notion to
incremental render is the follow-up.

## Not built yet

- A trash pass (`filter: {in_trash: true}`) — deletions are currently
  noticed by absence from the render sweep rather than by asking Notion
  what it trashed.
- Data-source schema, and the `entity_id_str` port (see
  `docs/dev/entity_ids.md`, which still lists notion as pending).
- Incremental render (`docs/dev/provider_migration_dolt_diff_and_cas_edge.md`).

The design and the measurements behind it are in
[`docs/dev/archived/notion_redesign.md`](../../../../../docs/dev/archived/notion_redesign.md).
