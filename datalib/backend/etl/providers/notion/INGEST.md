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

A run is a **listing** and six **loops**. The listing stores page
objects; each loop then fetches what the store lists and does not yet
hold, through `datalib_etl_web::owed`: the databases and blocks a page
sits under, a page's body, an attachment's bytes, a page's comments, a
commented block, a user. What a table holds
is `held_version` in its `_bookkeeping` sidecar, written in the
transaction that wrote the content; what is owed is the difference,
asked of the store each run and never stored. Nothing is marked done
(docs/dev/data_architecture_ingestion.md, "What is left to fetch").

**Listing.** Two modes.

With `api.roots` empty — the default — the mirror is the whole
workspace, listed through `POST /v1/search` sorted `last_edited_time`
descending, 100 at a time. Page objects come back complete, properties
included, so a search result *is* the page: it is stored held at its
`last_edited_time`, and for a database row with no body that single
response is the entire record. A result at the stamp the store already
holds the page at is not written again.

The stretch of edit times a walk has read is a `coverage` span under
the scope `search`, recorded in the transaction that stores each page
of results. A walk that reaches the end of the workspace has covered
everything below its newest result; one that reached the point the
store already covered has covered everything down to it; one cut
short, by `max_pages` or by a page of results that would not come,
covers only what it read, and the next walk reads through the covered
stretch to the gap below it, since search cannot skip. A steady-state
run therefore reads **one page of results** rather than the workspace,
stopping at the first result edited before the covered stretch's top
and listing every result edited at or after it. The compare is on
Notion's own stamp (`2026-09-01T00:00:00.000Z`), inclusive: Notion
reports edit times to the minute, so every page edited in the same
minute as the newest one the last run saw is listed again.
`refresh_window_days` lowers the stopping point by that many days, so
a page shared late, whose edit time is already covered, is listed once
more.

This works only because the ordering is trustworthy: `last_edited_time`
descending was measured strictly monotonic across 12,300 objects and
124 pages of results, with no duplicate ids. The 10,000-result cap in
Notion's changelog applies to data-source queries, not to search: that
walk was stopped by hand with `has_more` still true, so search is
trusted to enumerate the whole workspace. Most of what it returns is
database rows — 87% of the objects in a measured workspace had a
`database_id` parent — and they come back as ordinary page objects, so
the walk lists nothing but `object: "page"` and never queries a data
source for its rows. The data-source objects search also returns are
not listed: each is read for its database's title and parent, which go
into `tree` (below) at no cost in requests.

Do not confuse the covered stretch with `start_cursor` / `next_cursor`,
which page *within* one walk and do not survive it.

With `api.roots` set — each a page or a database, by id or pasted link —
the mirror is what sits under each root, and the pages a root's body
links. The same search, walked the same way under its own `coverage`
scope, `roots`, places every page and database in `tree` ("Where a
page sits"): a page the store does not mirror as an `unmirrored_page`.
It also lists each page's stamp in `listed_pages`, which is what a page
under a root is owed against. Then the `tree` loop below places the
blocks and hidden parents search does not, and a page is under a root
when its chain of parents reaches the root (`ingest::scope`): a child
page at any depth, whether or not the root's body links it; a
database's rows and what hangs off them; a database inside a page, and
its rows; a page inside a toggle or a column.

One hop more: every page a root's body links, as a child (`<page>`) or
a mention (`<mention-page>`), is mirrored too, but not what sits under
it, and not what it links. A mention is how a person points at a page
that lives elsewhere, and following mentions further reaches most of a
workspace in a few hops.

Only those pages enter `pages`, each with `GET /v1/pages/{id}`, owed at
the stamp `listed_pages` holds, so a page whose stamp has not moved
costs nothing and a steady-state run reads one page of search results.
A page that leaves the roots — moved elsewhere, or its root taken out of
the config — stays as it was, as a deleted one does. A root the search
does not place is asked for as a page, so one Notion has not got, or
will not show, is a `config:` row.

The first run of a roots source lists the whole workspace: one request
per hundred objects, about nine minutes for 53,000 pages, against the
hours their bodies and comments would take.

**Loops.** One request per record, in this order, each over the whole
store:

1. `GET /v1/databases/{id}` or `GET /v1/blocks/{id}` for every parent a
   `tree` row names that has no row of its own, round by round until
   the top of what the credential can see (see "Where a page sits").
2. `GET /v1/pages/{id}/markdown` for every page whose body is not held
   at the page's `last_edited_time`: pages that are not database rows
   first, then rows, each newest first. A workspace's rows are mostly
   records another tool wrote — event sign-ups, form responses, synced
   contacts — and outnumber its pages many times over; fetched newest
   first together, the rows came first and the pages people write
   waited days.
3. The bytes of every attachment edge without a `blake3`, when
   `api.attachments` is on.
4. `GET /v1/comments?block_id={page_id}` for every page whose comments
   are not held at its `last_edited_time`, when `api.comments` is on, in
   the bodies' order;
   one call (paged) returns the page's whole discussion set, including
   threads anchored to blocks inside it.
5. `GET /v1/blocks/{id}` for every block a stored comment hangs off that
   has no `comment_anchors` row.
6. `GET /v1/users/{id}` for every user a page or a comment names that
   has no `users` row.

A loop writes a flush of records in one transaction, with what each is
held at, and seals through the step's sealer, so a run killed anywhere
leaves a store a later run finishes.

**Progress.** The run's one bar counts requests the run can see waiting:
each page object roots mode asks for, and each record a loop owes.
A page the listing stores at a new stamp announces its body, and its
comment listing, as it is stored, so the queue grows with the search
walk and the ETA covers the loops ahead. A page of search results is
not counted, since its size is unknown until it arrives. Attachments,
anchors and users join the queue when their loop starts.

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
the listing not writing a page held at the stamp it is listed at; a run
that does write the row writes a different payload each time.

The signed URL is the only way to fetch an attachment's bytes, and it
lives only in the body's response. The attachment loop takes it from
the body the body loop read this run, or reads the body again; a slot
the body no longer links is gone, and so is a file Notion answers 404
or 410 for.

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
(401) before anything was listed, the first page of search results does
not come back, the store will not take a write, or in roots mode every
page it tried failed. Anything smaller is a `problems` row, and the rest
of the run goes on.

Two things end the run early without failing the step: the shared
retry guard giving up on the service (every request after would give up
too), and a 401 once pages have been listed. Whatever loop is running
stops where it is, the loops after it are not started, the run leaves
one row (`phase:rate_limit` or `phase:credential`), and returns as a
success so what it fetched is kept. Nothing past that point is marked
anything: it was not held, so it is owed, and the next run fetches it.

| what failed | its row | what clears it |
|---|---|---|
| a page object, in roots mode | `pages:<id>` | the page fetching; it is owed until held at the stamp `listed_pages` holds |
| a page's comments listing | `page_comments:<id>` | the listing fetching whole |
| comments the credential may not read (403: an integration without the read-comments capability) | one `listing:comments`, a warning; comments are not asked for again that run, and no page is marked failed | a run that reads comments, or has none to read |
| a page's body | `page_markdown:<id>` | the body fetching |
| a truncated subtree's follow-up | `page_markdown:<id>`; the body is owed, not stored short | the body fetching whole |
| more subtrees than `MAX_HOLE_FOLLOWUPS` | `page_markdown:<id>`, a warning; the body is stored and held | the page changing so it needs fewer |
| an attachment's bytes | `notion_attachments:<page>#<slot>` | the bytes landing, or the body no longer linking it |
| a commented block | `comment_anchors:<id>` | the block fetching |
| a database or block a page sits under | `tree:<id>` | the parent fetching |
| a user, other than with a 403 or a 404 | `users:<id>` | the user fetching |
| a configured root Notion has not got (404) or will not show (403) | `config:roots:<value>` | the root fetching, or leaving the config |
| a search page after the first, or `max_pages` cutting the listing short | `listing:search` | a search that reaches what the store covers |
| twenty-five requests of one loop failing in a row | `phase:<table>` | the loop running its course |

Each of those clears only by being tried again, and a run tries them on
its own: a record not held at its listed stamp is owed, whatever the
run before did with it. A record a loop never reached, because the run
was stopped or ended early, has no row: it is owed, not failed.

**A 404 is a deletion, not a failure.** Notion answers it for a page
deleted or no longer shared with the credential. The ingest deletes no
page, so a page the store holds stays as it was. A body that answers
404 is held empty at the page's `last_edited_time`, so it is asked for
again only once the page is edited; comments that answer 404 read as
none, and the page's stored comments go; a user or a block that answers
404 keeps an id-only row, held, so it is asked for once. So does a
parent in `tree` that answers 404 or 403, which is what a page shared
on its own has above it. A root that
answers 404 is a `config:` row; a page under a root whose object does
(in roots mode) is gone, and the store keeps what it had.

A request the transport refused because of the stop is not a problem,
and a stopped run leaves the last run's `listing:` and `config:` rows
standing.

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
(`datalib_etl_web::http::default_retryability`). Expect roughly one
empty-body response per 130 requests on a long walk; a loop that read
one as the end of a listing would silently truncate.

## Schema

`<data_root>/<group>/ingest/entities.doltlite_db`:

| table | holds | held at |
|---|---|---|
| `pages` | the page object: properties, parent, `in_trash`, timestamps | its `last_edited_time` |
| `page_markdown` | the body, slots not signatures | the page's `last_edited_time` it was read for |
| `page_comments` | one id-only row per page whose comments were listed | the page's `last_edited_time` it was read for |
| `comments` | one row per comment, with `page_id` and `discussion_id` | — (written with its page's listing) |
| `comment_anchors` | the text a block-anchored comment hangs off | read once |
| `users` | display names: from a people property, `GET /v1/users/{id}`, or a comment's name for its author | read once |
| `notion_attachments` | CAS edge, `ref_id` = the slot, written with the body | its bytes, once |
| `tree` | each page, database and block a page sits under: kind, title, parent; in roots mode every page search lists | read once; a page's with the page, a database's again when search lists it |
| `listed_pages` | roots mode only: each page search listed, at its `last_edited_time` | — |
| `coverage` | the stretch of edit times the search has read, scope `search` (`roots` in roots mode) | — |

`page_markdown` is its own table so `dolt_diff_page_markdown` means
exactly "the body changed", separate from "a property changed".
`page_comments` is its own row so that a page being written again does
not clear a listing that failed. The ladder (`schema_raw::LADDER`)
carries a store from before any of this over: rung 1 moves the body
stamp a column held into the sidecar, holds every page's comments
unless its row said they failed, drops a page stub whose object never
came, and drops the search mark, so the first run lists the workspace
whole once — listing requests only; nothing held is fetched again.
Rung 2 gives every page held its `tree` row, since a search walk past
it would not list it again; its databases and blocks are fetched by the
next run's first loop.

## Where a page sits

A page document opens with the path above it — `Bridge › Crew
manifest` for a row of the crew database — so a row of a synced
database says where it came from. Each page on the path links its
document; a database or a page this source does not render (an
`unmirrored_page`, a block) links Notion. `tree` holds what that path is walked through: one row per
page, database and block, with its kind, title and parent. Its own
table rather than read from `pages`, because a page's `pages` row
changes on every edit and a render that read its parents' rows would
render every page under a parent again each time the parent was
edited. A `tree` row changes only when a title, a parent or a kind
does, and doltlite diffs by content, so writing one again unchanged
names nothing.

Where each row comes from:

- **A page the store holds**: written with the page, its title read from
  its `title`-typed property.
- **A page roots mode places and does not mirror**: an
  `unmirrored_page`, from the search result, its title read the same
  way. Fetching the page makes it a `page`, and the search never makes a
  held page's row anything else. It is what lets a root's sub-pages be
  found by parent, and a page above a root be named in the path.
- **A database**: from the data-source object search returns for it,
  when the search walk reads one; otherwise `GET /v1/databases/{id}`,
  once. A database renamed is seen again only when search lists it.
- **Anything else** — a column or a toggle a page sits in, a page this
  credential cannot list: `GET /v1/blocks/{id}`, once. A child page or
  child database block carries its title; any other block, none, and
  the path walks through it without showing it.

A database row's `parent_id` is its database's id, though Notion names
its data source too; the path names the database.

## People and anchors

Two things Notion does not hand over with the object that needs them.

**Comment authors need nothing** — every comment carries
`display_name.resolved_name`.

**Page authors do.** A page object gives only `created_by.id`, so every
user a page, a people property or a comment names gets a `users` row,
once ever. A people property already carries the whole user object,
name and address included, so a user named in one costs no request.
Anyone else is asked for with `GET /v1/users/{id}`. A personal access
token may read only its own user — `GET /v1/users` (list all) is
closed to it too — so for everyone else Notion answers 403. That
answer, like a 404, keeps what the store knows: a comment's name for
its author, or the id alone, which render shows as an id prefix. The
row is held, so the user is not asked for again and is no problem. A
user whose request failed some other way is a `users:<id>` row until a
later run reads it.

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
not — its author's `users` row, found or not, and every `tree` row on
its path, found or not; a thread its
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

The ingest never deletes a page: search by edit time never reports a
deletion, so a page deleted or trashed in Notion stays in the mirror.
`pages.in_trash` is stored when a trashed page is seen, and render does
not read it. A trash pass (`filter: {in_trash: true}`) is not built,
nor is a data-source schema. A page's comments are listed whole, so a
comment the listing no longer returns is deleted with that listing, and
an attachment a complete body no longer links loses its edge.

Every number above was measured against a live workspace, not read off
Notion's documentation.
