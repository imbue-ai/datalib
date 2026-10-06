# Notion

Mirrors a Notion workspace through the official REST API
(`api.notion.com/v1`) at `Notion-Version: 2026-03-11`.

## Auth: one credential, and the version rides with it

A **personal access token**, stored under the latchkey `notion` service.
A PAT acts as the person who created it and inherits their permissions,
so nothing has to be shared with an integration — which is what lets the
mirror default to the whole workspace with no configured starting point.
`GET /v1/users/me` answers `type: "bot"` for a PAT and for an internal
integration alike; `bot.owner.type` is what tells them apart — `user`
for a PAT, `workspace` for an integration. Nothing here checks it.

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

With `api.roots` empty — the default — the mirror is the whole
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

The 10,000-result cap in Notion's changelog applies to data-source
queries, not to search: that walk was stopped by hand with `has_more`
still true, so search is trusted to enumerate the whole workspace.
Most of what it returns is database rows — 87% of the objects in a
measured workspace had a `database_id` parent — and they come back as
ordinary page objects, so the walk queues nothing but `object: "page"`
and never queries a data source for its rows. The data-source objects
search also returns are containers, and are skipped.

It is stored per source as `sync_scope_state.last_seen_at_utc`, alongside a
config blob (`sync_scope_config`), so widening `refresh_window_days` re-examines that window
instead of being suppressed by a point recorded under the narrower
setting. It is written only after the pages land — a point recorded over
a failed pass would skip that window forever.

Do not confuse it with `start_cursor` / `next_cursor`, which page
*within* one walk and do not survive it.

With `api.roots` set, the mirror is those pages and everything under
them, walked through the `<page>` and `<database>` links in each body.
No search, and no resume cursor: the walk is the enumeration.

**Per page**, two requests:

1. `GET /v1/pages/{id}` — properties, parent, icon, cover, `in_trash`.
2. `GET /v1/pages/{id}/markdown` — the body, already rendered.

Plus `GET /v1/comments?block_id={page_id}` when `api.comments` is on
(the default); one call returns the page's whole discussion set,
including threads anchored to blocks inside it. A page whose
`last_edited_time` has not moved since the stored row is not fetched
again — unless part of its last fetch failed (see "When part of a sync
fails") — but the walk still descends into its stored child pages.

**The block tree is not mirrored.** There is no `blocks` table and no
block renderer. Notion renders the page; we store what it returns.

What it returns is Notion's *enhanced* markdown, not plain markdown.
Besides `<unknown>`, `<page>` and `<database>`, which ingest reads, a
body carries `<callout>`, `<columns>`/`<column>`, `<details>`/
`<summary>`, `<table>` with `<colgroup>`/`<col>`, `<table_of_contents>`,
`<span>` and `<br>`. Ingest stores them untouched and render passes
them through; whatever displays the document has to cope with them.

## The one rule about stored markdown

**Never store a Notion file URL as returned.** Every Notion-hosted file
link is pre-signed and re-minted on each fetch, valid about an hour. Two
fetches of an unchanged page differ only in `X-Amz-Signature` and
friends — proven against a live page whose `last_edited_time` had not
moved.

Left alone, that makes an unchanged page differ from itself every run:
`dolt_diff_page_markdown` reports a modification, the page re-renders,
and the reset-then-resync stability check fails on content nobody
touched.

So every signed URL is reduced to its **slot** — scheme + host + path,
query discarded — before the body is stored (`ingest::slots`). The
slot is also the CAS edge's `ref_id`, because it is the one identifier
upstream keeps stable across both re-signing and a byte replacement.

The rewrite is deliberately narrow: it fires only on a Notion file host
**and** a signature parameter. A measured page carried 1,472 ordinary
links with meaningful query strings (DoorDash orders, Google Docs
`gid=`) against 26 real attachments; stripping queries indiscriminately
would have corrupted all 1,472.

The rewrite covers `page_markdown` only. `pages.payload` is the page
object as returned, and a Notion-hosted cover or icon — about one page
in sixty, measured — sits in it as a signed URL. Nothing declares those
paths volatile. What keeps an unchanged page from rewriting itself is
`mirror_page` skipping the upsert when `last_edited_time` has not moved
since the stored row; a run that does write the row writes a different
payload each time.

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

Follow-ups are capped per page (`MAX_HOLE_FOLLOWUPS`, 64) — one measured
page wanted ~1,385 of them, which would otherwise dominate a run.
Truncation itself is rare: one page in a random 70, and that one a
permanent `drive` embed. Size truncation is rarer still and sits in a
few very large pages, which is why the cap is per page.

## When part of a sync fails

The step fails only when it can do nothing: the credential is refused
(401) before anything was fetched, the first page of search results does
not come back, the store will not take a write, or every page it tried
failed. Anything smaller is a `problems` row, and the rest of the run
goes on.

Two things end the walk early without failing the step: the shared
retry guard giving up on the service (every request after would give up
too), and a 401 once pages have been fetched. The walk stops where it
is, the run leaves one row (`phase:rate_limit` or `phase:credential`),
and returns as a success so what it fetched is committed: the store
commits only when the download succeeds. The resume cursor does not move
and nothing past that point is marked failed, so the next run picks up
the rest through search and the retry set.

| what failed | its row | what clears it |
|---|---|---|
| a page object | `pages:<id>` | the page fetching |
| a page's comments listing | `pages:<id>`, a warning (the page is stored) | the page fetching whole |
| comments the credential may not read (403: an integration without the read-comments capability) | one `listing:comments`, a warning; comments are not asked for again that run, and no page is marked failed | a run that reads comments |
| a page's body | `page_markdown:<id>` | the body fetching |
| a truncated subtree's follow-up | `page_markdown:<id>`, a warning | the body fetching whole |
| more subtrees than `MAX_HOLE_FOLLOWUPS` | `page_markdown:<id>`, a deliberate-loss warning | the page changing so it needs fewer |
| an attachment's bytes | `notion_attachments:<page>#<slot>` | the bytes landing, or the body no longer linking it |
| a user | `users:<id>` | the user fetching |
| a configured root Notion has not got (404) or will not show (403) | `config:roots:<value>` | the root fetching, or leaving the config |
| a search page after the first | `listing:search` | a search that reaches the resume cursor |

Each of those clears only by being tried again, and upstream has not
moved any of them, so a run fetches them again on its own:
`RawDb::pages_to_refetch` names every page with a failed object, comments
listing, body or attachment, plus any whose stored body is older than
its stored object. Such a page is not skipped as unchanged, and in search
mode it is queued beside what search named, since search names only what
moved. An attachment is retried by fetching its page again because its
signed URL lives only in the response that named it. A failed user is
asked for again at the end of every run. The follow-up cap is left out:
fetching the page again gets the same body.

**A 404 is a deletion, not a failure.** Notion answers it for a page
deleted or no longer shared with the credential. The ingest deletes
nothing (see "Deletions"), so a page the store holds stays as it was;
what goes is its failure rows and the reasons it was in the retry set,
and a stub that never fetched goes whole (`RawDb::retire_page`). A body
that answers 404 is marked current at the page's `last_edited_time`, so
it is asked for again only once the page is edited; a user that answers
404 loses its failure, an attachment its failed edge, and comments read
as none. A configured root that answers 404 is still a `config:` row.

A search cut short holds the resume cursor where it was, so the next run
reads that window again. A page that failed does not hold it: it is in
the retry set. A page fetched again only to fail again does not count
toward "every page failed", so one page a 5xx keeps failing does not fail
every steady-state run.

The body is stored last, after the attachments, comments and users: a
stop part-way through a page leaves its stored body behind its stored
object, which puts it in the retry set. A request the transport refused
because of the stop is not a problem, and a stopped run leaves the last
run's `listing:` and `config:` rows standing.

## Most pages have no body

In a random 70-page sample of a real workspace, **50 (71%) returned
empty markdown**, and 63 of the 70 were database rows. A database row's
content usually *is* its properties. Render must not treat an empty body
as nothing to render; for those pages the properties table is the
document.

## Rate limits

~3 requests/second per connection, plus a workspace-wide limit that
scales with plan. `429` and `502`–`504` are retried, honouring
`Retry-After`, by the shared HTTP layer
(`datalib_etl::http::default_retryability`). Expect roughly one
empty-body response per 130 requests on a long walk; a loop that read
one as the end of a listing would silently truncate.

## Schema

`<data_root>/<group>/ingest/entities.doltlite_db`:

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
back to an id prefix rather than failing the page, and is a `users:<id>`
row until a later run reads it.

**A comment names a `block_id` and carries no quote of what it is
about.** So `GET /v1/blocks/{id}` runs for **commented blocks only** —
one request per commented block, not per block (11 across 25 pages in a
measured workspace) — and the text lands in `comment_anchors`. The
thread file opens with it as a blockquote, and it leads the thread row's
searchable text. When `original_content_deleted` is set, the thread says
so instead of quoting something that no longer exists.

## Incremental render, and what it costs

A page is one bucket and a comment thread another, each keyed by its
Notion id. When a bucket renders it declares every raw row it read
(`render_inputs` in the render store): a page its `pages` row, its
`page_markdown` row, every `notion_attachments` row — bytes fetched or
not — and its author's `users` row, found or not; a thread its
`comments`, its page (for the title) and its `comment_anchors` block.
The render step diffs those tables from the commit it last completed
against and hands render the buckets whose rows moved; render's own
scan adds what a new row names through the rows still there (a new
comment its thread, a new attachment its page, an edited page its
threads). A bucket whose rows are gone is declared with nothing, and
its documents go. The resume cursor is the `render_cursor` row in the
render store, written in the same transaction as the last document of
the run.

What this narrows is the **render** — writing files, building
`grid_rows`, hashing — which is where the cost is. It does not narrow
the read: the store's rows are still walked once and filtered in
memory.

## Deletions

Render does not walk everything, so absence from a run means nothing
and a deletion has to be **named**. Two passes, because a page and its
threads are separate documents with separate `conversation_uuid`s:

- a page the diff named whose `pages` row is gone — the page document,
  and every discussion the store still remembers hanging off it, or its
  threads become orphans nothing will ever name again;
- a discussion the diff named with no `comments` rows left — a thread
  resolved away while its page survived.

Both ask the store rather than inferring from what the parse returned.
`load_pages` filters on `payload IS NOT NULL`, so a page missing from a
parse result may simply be one whose body has not arrived yet; deleting
on that reading would destroy a live document.

The ingest itself never deletes a page or a comment, so a page deleted
or trashed in Notion stays in the mirror. `pages.in_trash` is stored
when a trashed page is seen, and render does not read it. A trash pass
(`filter: {in_trash: true}`) is not built, nor is a data-source schema.

Every number above was measured against a live workspace, not read off
Notion's documentation.
