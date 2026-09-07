# Notion, rebuilt on Notion's own API

**Status: largely built, 2026-09-07.** §2's measurements are real —
run against a live workspace, not read off documentation. §5's design
is implemented except where §8 says otherwise; the provider's own
[`DOWNLOAD.md`](../../datalib/backend/etl/providers/notion/DOWNLOAD.md)
is the current reference for how it behaves, and this file is the
argument for why. Where the two disagree, that one is newer.

Notion has shipped, over the last year, most of the things our provider
was built to work around. The provider we have predates all of it and
still carries the workarounds. This proposes replacing it.

## 1. What Notion shipped that we don't use

Four of these change the design; the rest are context.

| Capability | Endpoint | What it replaces here |
|---|---|---|
| **Page as markdown** | `GET /v1/pages/{id}/markdown` | our 1,273-line block→markdown renderer, and the recursive block walk that feeds it |
| **Personal access tokens** | Developer portal | the "share each page with the integration" ceremony, and so the hand-configured subtree seeds |
| **Workspace listing** | `POST /v1/search`, empty query, `sort: last_edited_time desc` | the unofficial `getNotificationLog` inbox, the BFS, and the missing incremental cursor |
| **Data sources** | `POST /v1/data_sources/{id}/query` | nothing — this is content we have never mirrored at all |
| Trash filter | `POST /v1/search`, `filter.in_trash` | nothing — we have never noticed a deleted page |
| Comment attachments, `display_name` | `GET /v1/comments` | nothing new, but see §4 |
| Webhooks | 31 event types | not usable here; see §3 |

The API version to target is **`2026-03-11`** (`in_trash` fully replaces
`archived`; `position` replaces `after`). Data sources split from
databases in `2025-09-03`.

Rate limits: ~3 requests/second per connection, plus a workspace-wide
limit that scales with plan. That number is what makes the request
count per page (§4) a design constraint rather than a detail.

## 2. What the live spike measured

Run 2026-09-07 against the "Imbue" workspace with the existing latchkey
`notion` credential. **These are measurements, not doc claims.** Where
this section and §1 disagree, this section wins.

The credential is a **personal access token**. `GET /v1/users/me`
returns `type: "bot"` for a PAT as well as for an internal integration —
the discriminator is `bot.owner.type`: `"user"` means user-scoped,
`"workspace"` means an internal integration. This one says `"user"`, and
the API confirms it directly: `GET /v1/users` returns
`restricted_resource — "Personal access tokens cannot list users."`
So search here enumerates what Thad can see, not a hand-curated share
list. `latchkey auth list` reports the service
`"credentialStatus": "invalid"`; it is not, it works fine. Don't trust
that field.

**Search enumerates far more than the docs' 10,000-result cap suggests.**
Walking `POST /v1/search` sorted `last_edited_time desc`, 100 at a time:

- **12,300 distinct objects retrieved, `has_more` still true.** The
  10,000 cap in the changelog applies to *data source queries*, not to
  search. Stopped voluntarily at ~124 requests, not by any limit.
- **Zero duplicate ids** across 124 cursor pages. Cursor pagination is
  stable enough to walk.
- **`last_edited_time` is strictly monotonic descending** across the
  whole walk. The watermark design works: stop at the first result
  older than the stored mark.
- One request in ~130 returned an empty body (transport, not an API
  error). Retry has to cover this; a naive walk would silently truncate.

**87% of this workspace is database rows, and we mirror none of it.**
Of the first 6,000 results:

| parent type | count | mirrored today? |
|---|---|---|
| `page:database_id` (a database row) | 5,174 | **no** |
| `page:page_id` | 426 | yes |
| `page:workspace` | 218 | yes |
| `page:block_id` | 102 | yes |
| `database:*` | 80 | no |

`walk_page_blocks` records `child_database` blocks and refuses to
descend. So the current provider can see, at most, about one page in
eight of this workspace. **Search returns database rows directly**, as
plain page objects — settling §7's question 3: we do not need a
per-data-source query just to enumerate them.

**Signed URLs rotate on every fetch — proven.** Two back-to-back
`GET /v1/pages/{id}` on a page whose `last_edited_time` did not move:

```
last_edited_time identical : True   (2026-08-27T23:32:00.000Z)
full URL identical         : False
scheme+host+path identical : True
query params that differ   : X-Amz-Credential, X-Amz-Security-Token, X-Amz-Signature
whole-payload identical    : False
```

Two things follow. First, the Ship-of-Theseus key works exactly as
designed: **scheme + host + path is stable, the query string is not.**
Second — and this is a bug in what we ship today, not just a risk in the
redesign — the current downloader stores the whole page object in
`pages.payload`, and the notion provider declares **no volatile paths at
all** (slack does; notion has none). So every re-download of an
unchanged page with a Notion-hosted cover or icon writes a different
payload, `dolt_diff_pages` calls it modified, and the page re-renders
for nothing. 106 pages in the first 6,000 results carry such a cover or
icon.

**Comments are better than we assumed.** Probing 25 pages with
`GET /v1/comments?block_id={page_id}`: 4 pages had comments, 19 comments
across 11 discussions.

- **One call per page gets the whole page's discussions**, including
  ones anchored to child blocks: 18 of the 19 came back with
  `parent.type == "block_id"`, not `page_id`. Settling §7's question 4.
- **`display_name.resolved_name` carries the author's name** — e.g.
  `{"type":"user","resolved_name":"Cathy Zhao"}`. We do not need the
  users API, or the retired unofficial `notion_user` table, to render an
  author. Today every Notion comment author renders as a raw UUID
  because `user_names` is always empty; this one field fixes it.
- `original_content_deleted: true` appears on real comments — the
  commented-on content is gone upstream.
- What a comment does **not** carry is the anchored text or any offset.
  It names a `block_id` and nothing more. See §5 for what that costs.

**The API version is four years stale and cannot be overridden per
request.** latchkey injects `Notion-Version: 2022-06-28`. Passing
`-H "Notion-Version: 2026-03-11"` yields
`"instead was \"2022-06-28, 2026-03-11\""` and a 400 — the two headers
concatenate. The header is set once, in the stored credential. Notion
currently accepts `2021-05-11`, `2021-05-13`, `2021-08-16`,
`2022-02-22`, `2022-06-28`, `2025-09-03`, `2026-03-11`.

### The markdown endpoint, measured

Re-run 2026-09-07 after re-setting the credential to
`Notion-Version: 2026-03-11`. Twelve pages, spread across
workspace-parented, page-parented and database-row pages.

**It works, and the slot rewrite closes the volatility exactly.** On a
page with two images, two consecutive fetches:

```
raw markdown identical    : False
slot-rewritten identical  : True
bytes dropped by rewrite  : 3002   (31% of a 9.5 KB page — pure signature)
```

The only differences between the two fetches were the two `![](…)`
lines. Stripping the query string makes them byte-identical. This is the
§5 design proven on the exact table it applies to.

**The request saving is real but smaller than "7 → 2", and it is very
uneven.** Block-walk requests per page, measured over the same 12 pages:

| | requests |
|---|---|
| block walk, min / median / max | 1 / **11** / ≥60 (two pages hit my cap) |
| old path, 12 pages (page + walk + comments) | **241+** |
| new path, 12 pages (page + markdown + comments) | **36** |

So **~6.7× fewer requests, and that is a floor** — two pages were still
producing children when the walk was capped. At 3 req/s the same 12
pages cost ~80s the old way and ~12s the new way. A flat page saves
nothing; a deep one saves enormously. Extrapolated across 12,300
objects that is roughly the difference between a mirror that finishes
overnight and one that does not.

**`truncated` covers two different things, and the tell is one
attribute.** Three of twelve pages, then 1 of a further 70, came back
`truncated: true`. Every hole leaves a marker in the text; what differs
is whether it can be filled:

| marker in markdown | means | follow-up fetch |
|---|---|---|
| `<unknown url="…#id"/>` — **no `alt`** | a subtree the response was too large to inline | **resolves cleanly** |
| `<unknown url="…#id" alt="button"/>` — **has `alt`** | a block type markdown cannot express | **never resolves** |

Both were tested. Four no-`alt` ids from the 1.38 MB page fetched as
`/v1/pages/{id}/markdown` returned real content — 1,620, 1,626, 49 and
53 chars, `truncated: false`, zero nested unknowns. An `alt`-bearing id
returned 120 chars, `truncated: true`, and **the same id back** — an
infinite regress. So the rule is simply: **`alt` present ⇒
unrepresentable, don't retry; `alt` absent ⇒ fetch it and splice it in.**
No block walk and no second renderer are needed for either.

Unrepresentable `alt` types seen so far: `button`, `alias`, `drive`.

*(An earlier draft of this section claimed 1,385 blocks vanish from the
markdown with no marker. That was wrong — an artifact of counting only
`alt`-bearing tags and calling it the tag count. The page has 1,386
markers for 1,387 ids; exactly one id has no marker.)*

**Truncation is rare; the follow-up cost is concentrated.** In a random
70-page sample, **1 page (1.4%)** truncated, and that one was a
permanent `alt="drive"` embed, not size. Size truncation is rarer still
— the 1.38 MB page is an outlier, and it alone would need ~1,385
follow-up fetches. Bound the follow-up pass per page rather than letting
one pathological page dominate a run.

**Most pages in this workspace have no body at all — 71% of them.** In
the same 70-page random sample (63 of which were database rows, matching
the workspace's 87% composition), **50 returned `markdown: ""`**. A
database row's content usually *is* its properties.

Two consequences, and the second is the bigger one:

1. The renderer cannot treat empty markdown as "nothing to render". For
   most of this workspace the properties are the document.
2. **Search already returns the full page object, properties included.**
   So for a property-only row, the search result *is* the complete
   record — no per-page fetch needed at all. That reframes the request
   budget: ~124 search requests cover the properties of all 12,300
   objects, and the per-page markdown call is only interesting for the
   ~29% that have a body. The open question is how to know which those
   are without paying a request to find out (§7).

**Tag vocabulary actually observed**, which is what the renderer has to
resolve: `callout`, `page`, `database`, `details`/`summary`, `table`/
`tr`/`td`/`colgroup`/`col`, `columns`/`column`, `table_of_contents`,
`span`, `br`, `unknown`.

## 3. The three toolkits we would *not* build on

Worth stating, because all three are plausible-looking and all three are
wrong for a mirror.

**Notion MCP** is a remote server hosted by Notion, reached over OAuth,
exposing agent-facing tools (`notion-search`, `notion-fetch`,
`notion-query-data-sources`). Its results are shaped for an agent's
context window — truncated, summarized, with tool-access states and
upgrade prompts in the payload. A mirror wants the opposite: complete,
verbatim, stable across fetches. It would also route a local backup
through Notion's AI infrastructure on an 8-hour token. Use REST.

**The `ntn` CLI** is an npm package wrapping the same REST API, aimed at
humans and coding agents at a terminal. It adds a Node runtime
dependency and a text-formatted interface between us and JSON we can
fetch directly. No.

**The Agent APIs** manage Custom Agents and chat sessions. Nothing there
belongs in a page mirror. But `Query sessions` / `Query session events`
return a user's conversations with their Notion agents, which is the
same shape as the Claude and ChatGPT sources we already mirror. That is
a *separate future source type*, not part of this.

**Webhooks** need a public HTTPS endpoint. datalib runs on the user's
machine, so they are not available to us, and that is precisely why the
`last_edited_time` watermark in §4 is the incrementality mechanism
rather than a fallback.

## 4. What is actually wrong with what we have

Each of these was checked against the tree, not against the provider's
own docs — which are stale in at least two places (noted below).

1. **Discovery runs on an unofficial, cookie-authenticated endpoint.**
   `www.notion.so/api/v3/getNotificationLog`, behind Cloudflare, which
   is the entire reason this provider needs `latchkey-curl-impersonate`
   and a second latchkey service (`notion_unofficial`). It exists only
   because the official API appeared to have no listing.

2. **Page discovery is hand-configured and mandatory.**
   `NotionConfig::validate` *rejects* a sync block that names neither an
   inbox nor a subtree page. Setting the source up means pasting page
   URLs.

3. **There is no incremental cursor, and the code says so.**
   `bfs_drain` carries the comment "Notion's official API has no global
   list endpoint … Skip-on-unchanged for those will land when we add
   cursored search; for now every queued page is fetched."
   `NotionApiSync.refresh_window_days` is declared in the config crate
   and read nowhere (one grep hit — the declaration).

4. **Request amplification.** Per page: one `GET /pages/{id}`, then one
   `GET /blocks/{id}/children` per container block recursively, then one
   `GET /comments`. A modestly nested page is 6–8 requests; at 3/s that
   is two-plus seconds per page.

5. **Databases are missing entirely.** `walk_page_blocks` records
   `child_database` blocks and refuses to walk into them. The
   `databases` and `users` tables are declared in `schema_raw.rs` and
   are **never written or read** by any code in the crate. For a
   database-heavy workspace, most of the content is simply absent.

6. **Nothing notices a deletion.** A trashed page stays in the mirror
   and in the grid forever.

7. **Attachments are images only.** `fetch_image_blobs` skips any block
   whose `type != "image"`. Files, PDFs, video, audio, page icons and
   covers are never archived — and the URLs we *do* keep for them are
   signed S3 links that expire in one hour, so what is stored is a dead
   link.

8. **Three lookup tables are permanently empty, and the provider's own
   doc says otherwise.** `parse_api_dir` constructs `user_names`,
   `media_urls` and `bookmark_titles` as empty maps unconditionally, then
   threads them through about fifteen functions. So today every comment
   author renders as a raw UUID, every video/audio block gets an empty
   URL, and every bookmark loses its title. `TRANSLATE.md` still
   describes these as working fallbacks read from an unofficial capture.
   `DOWNLOAD.md` and `DOLTLITE_RAW.md` are also stale — they describe a
   JSONL event-store layout the provider stopped using.

9. **Two credentials, one of them a browser cookie.**

10. **`grid_rows.uuid` is minted as the bare page id**, not through
    `entity_id_str`. `docs/dev/entity_ids.md` lists notion as `pending`.

## 5. The design

### Auth: one credential

A Notion **personal access token**, through the existing latchkey
`notion` service. A PAT acts as its creator and inherits that person's
permissions, so nothing needs to be shared with an integration — which
is what makes whole-workspace discovery possible. Tokens are created in
the developer portal with a chosen lifetime (7/30/90/180 days, or 1
year).

`notion_unofficial` is deleted. `api.notion.com` accepts vanilla HTTP,
so the Cloudflare impersonation shim is no longer on this provider's
path at all.

One documented PAT limitation to design around: **`GET /v1/users` (list
all users) is not available to PATs.** Retrieving a single user by id
is. See "Users" below.

### Discovery: cursored search, not BFS

```
POST /v1/search
{ "sort": { "timestamp": "last_edited_time", "direction": "descending" },
  "page_size": 100, "start_cursor": … }
```

Walk pages of results and **stop at the first result older than the
stored watermark**. Results are `page` and `data_source` objects. Store
the new watermark with the shared `datalib_etl::scope_state` helper,
scoped on the workspace id, with `refresh_window_days` finally wired to
the overlap floor it was declared for.

A second pass with `filter: { "in_trash": true }` finds deletions.

For each `data_source` seen, `POST /v1/data_sources/{id}/query` with a
`last_edited_time` `on_or_after` filter enumerates its changed rows.
Rows come back as page objects and join the same page pipeline. (Whether
search *also* surfaces data-source rows directly is an open question —
§7 — but the design doesn't depend on the answer: dedupe by page id.)

This is the cursored search the existing code's own comment asks for.

### Per page: two requests, not seven

1. `GET /v1/pages/{id}` — properties, parent, icon, cover, created/edited
   by and time, `in_trash`, url.
2. `GET /v1/pages/{id}/markdown` — the whole body, in Notion's enhanced
   markdown, in one request.

Plus `GET /v1/comments?block_id={id}` when comments are enabled.

**We stop mirroring the block tree.** No `blocks` table, no
`walk_page_blocks`, no block-type matrix. What we give up and why it is
affordable:

- *Child-page discovery* — no longer needed; search enumerates
  everything, and the markdown carries `<page url="…">` links anyway.
- *Per-block ids for anchors* — a Notion page is already **one** document
  and one grid row; only comment threads need anchors, and those are
  separate documents keyed on `discussion_id`. Nothing we render today
  uses a block id as an anchor.
- *Attachment URLs* — the markdown carries them inline (see below), and
  coverage goes **up**, not down.

Enhanced markdown represents what plain markdown can't with XML-ish
tags: `<callout>`, `<columns>`/`<column>`, `<details>`/`<summary>`,
`<table>`, `<synced_block>`, `<page url>`, `<database url>`,
`<mention-user url>`, `<mention-page url>`, `<mention-date>`,
`<audio|video|file|pdf src=…>`, `![caption](url)` for images. Block
types it cannot express (bookmarks, embeds, link previews, breadcrumbs)
come through as `<unknown url="…" alt="block_type"/>` — which is more
than we render for several of them today.

**Truncation is real and must be recorded.** Pages beyond ~20,000 blocks
come back `truncated: true` with up to 100 `unknown_block_ids`. Each of
those ids can be fetched as a page of markdown in its own right. Store
`truncated` and the id list, fetch the follow-ups, and attach a `problem`
to the rendered document if anything is still missing — per the
data-quality rule in `data_architecture_parse_and_render.md` §4, an
incomplete record says so rather than looking whole.

### Attachments, and the Ship-of-Theseus id

Every Notion-hosted file URL is a pre-signed S3 link, re-minted on every
fetch and valid for about an hour. §2 proves it. So a URL is a *plank*,
not the *ship*: it is replaced constantly while naming the same file.
Nothing durable may be keyed on one.

Three identities, kept deliberately separate:

| | what it is | stable when | lives in |
|---|---|---|---|
| **slot** | the unsigned URL — scheme + host + path, query discarded | forever, for a given attachment slot | `page_markdown.markdown`, and the edge's `ref_id` |
| **content** | `blake3` of the bytes | until the bytes change | `notion_attachments.blake3`, `cas_objects` |
| **signature** | `?X-Amz-…` | ~60 minutes | **nowhere** — never stored |

The rule: **the markdown stores the slot, the edge table maps slot →
blake3.** Before the markdown is written to the raw store, every signed
URL in it is rewritten to its slot. Then:

- Re-fetching an unchanged page produces byte-identical markdown, so
  `dolt_diff_page_markdown` stays empty and render skips it. That is the
  "nothing changes unless something actually changed" property.
- Replacing an image *in place* — same slot, new bytes — changes
  `notion_attachments.blake3` and nothing else. The diff lands on the
  table whose subject is the bytes, which is where it belongs.
- A failed blob fetch leaves an edge row with `blake3` NULL. The
  markdown is still correct and still stable; the retry pass fills the
  edge in later. Keying the markdown on `blake3` instead would have made
  the body un-writable until every download succeeded.

Fetching is: extract each slot from the markdown (`![](…)`,
`<file src>`, `<pdf src>`, `<video src>`, `<audio src>`) plus the page
object's `icon` and `cover`; skip any slot whose edge already has a
`blake3`; fetch the rest **in the same run**, before the signature
expires.

Coverage goes from images-only to every attachment kind plus icons and
covers.

**What does *not* go in the CAS: the markdown.** The CAS is for opaque,
dedupable bytes. Page markdown is neither — it is unique per page, so
content-addressing dedupes nothing, and putting it there would make
`dolt_diff` report "the hash changed" where a text column reports *what*
changed. The render cursor consumes that diff, so a hash would cost us
the incrementality we are building this for. It also would have made the
text-at-rest count in `multimodal_retrieval.md` §4 worse, not better.
Markdown goes in a `page_markdown` **column**; only attachment bytes go
in the CAS.

**The same rewrite fixes a bug we ship today.** `pages.payload` stores
the whole page object, signed cover/icon URL included, and the notion
provider declares no volatile paths. Applying the slot rewrite to the
page payload too — or declaring the signature query params volatile —
stops 106 pages of this workspace from re-rendering on every run.

### Comments, and what they attach to

One `GET /v1/comments?block_id={page_id}` per page returns the page's
whole discussion set, block-anchored threads included (§2). Group by
`discussion_id` into thread documents, exactly as today.

A comment names the `block_id` it hangs off and **nothing else** — no
quoted text, no offset. Since we are no longer mirroring the block tree,
that id would otherwise resolve to nothing. So: **fetch
`GET /v1/blocks/{id}` for commented blocks only.** That is one request
per *commented* block, not per block — 11 of them across the 25 pages
probed — and it buys the thread a real anchor ("comment on: …") instead
of a bare UUID. Store them in a small `comment_anchors` table, not a
revived `blocks` table; the shape is `id`, `page_id`, `plain_text`.

When `original_content_deleted` is true the anchor is gone upstream;
record that rather than rendering a dangling reference.

Resolved threads appear to be unavailable at any endpoint. If the spike
confirms it, that is an upstream gap to record in the source's
data-quality notes — a record we cannot store — not something to work
around.

### Users

`display_name.resolved_name` on each comment already carries the author
name (§2), so the common case needs no user lookup at all. For ids that
appear without one — page `created_by`, `last_edited_by`, people
properties — resolve with `GET /v1/users/{id}` and cache in the `users`
table (at last, populated). `GET /v1/users` (list all) is unavailable to PATs — confirmed
against this workspace, not just documented — so resolve lazily by id.
Never add a list-users pass; it cannot work here.

### Deletions

Search's `in_trash` pass, plus a detail fetch that 404s or comes back
`in_trash: true`, sets a flag on the row. **Mark, don't delete.** The
row stays, render drops it from `grid_rows`, and the versioned raw store
keeps the history — which is the property the store exists for (#292).

### Raw schema

```
workspace           one row: bot user + workspace identity (GET /v1/users/me)
pages               id PK, parent_type, parent_id, data_source_id,
                    in_trash, created_time, last_edited_time, url, payload
page_markdown       id PK (= page id), markdown, truncated,
                    unknown_block_ids, source_last_edited_time
databases           id PK, parent_id, last_edited_time, payload
data_sources        id PK, database_id, parent_id, last_edited_time, payload
comments            id PK, discussion_id, parent_type, parent_id,
                    page_id, created_time, last_edited_time, payload
users               id PK, payload
comment_anchors     id PK (= block id), page_id, plain_text
notion_attachments  CAS edge: id, page_id, ref_id (unsigned URL), blake3
```

plus the `<table>_bookkeeping` sidecars, `sync_runs`, `sync_scope_state`
and `sync_scope_config` the shared layer provides.

`page_markdown` is a separate table from `pages` on purpose: it makes
`dolt_diff_page_markdown` mean exactly "the body changed", distinct from
"a property changed", and keeps a large blob out of the cheap
properties diff.

`blocks` is gone. `databases` and `users` stop being decorative.

### Render

The render step shrinks to: read `page_markdown`, resolve attachment
references to on-disk filenames, add QMD frontmatter, and wrap comment
threads. The 1,273-line block matrix and the 165-line parse shim both
go; a few hundred lines should cover it.

The document model stays as it is — one document per page, one per
discussion thread — because it is right and it is why an active thread
doesn't churn the page's fingerprint.

Two things to do while the file is open:

- **Database rows.** A data-source row is a page with typed properties.
  Project the ones that map (title, dates, people, status, select) into
  `grid_rows`, and add a `Notion Database Row` kind.
- **Entity ids.** Port to `entity_id_str("notion", Scope::ProviderGlobal,
  kind, id)` with a `source_native_id` backpointer, per
  `docs/dev/entity_ids.md`, which already lists notion as pending and
  already names `ProviderGlobal` as the right scope.

### Config

```toml
[[steps]]
id = "notion/raw"
command = ["datalib-step", "download", "notion_api"]
params = { latchkey_settings = { service = "notion" }, sync = {
    refresh_window_days = 30,
    databases = true,
    comments = true,
    attachments = true,
    roots = [],           # empty = the whole workspace
} }
```

`roots` becomes an optional *allowlist*, not a requirement. The
"must enable inbox or list at least one subtree page" validation rule
disappears, because whole-workspace is now the default and it works.

## 6. What gets deleted

`download/unofficial.rs`, `download/mod.rs`'s BFS and inbox passes,
`walk_page_blocks`/`fetch_all_children`, the `blocks` table,
`render/render.rs`, `render/parse.rs`'s empty-map plumbing, the
`notion_unofficial` latchkey service, the `--inbox` CLI mode,
`tests/fixtures/notion_web/notion_block/` and `notion_user/` (unofficial
captures), and the Cloudflare-impersonation requirement from this
provider's setup path. All three provider `.md` files are rewritten;
they currently describe a JSONL layout that no longer exists.

## 7. What is still open

Five questions were listed here before the spike. Three are settled and
one is settled in a better direction than expected:

| # | question | answer |
|---|---|---|
| 1 | Do signed file URLs rotate per fetch? | **Yes, proven** (§2). Ship-of-Theseus slot key confirmed viable. |
| 2 | Does search enumerate the workspace, and how deep? | **12,300+ distinct objects, no cap hit**, monotonic, stable cursors. No 10k ceiling on search. |
| 3 | Do data-source rows appear in search? | **Yes** — as plain page objects with `parent.type = database_id`. 87% of this workspace. |
| 4 | What does `comments?block_id={page_id}` return? | **Block-anchored threads too**, plus `display_name.resolved_name`. One call per page. |
| 5 | Does latchkey take a PAT, and what version does it inject? | Injected `2022-06-28`; cannot be overridden per request. **Now re-set to `2026-03-11`, with a PAT.** |
| 6 | Does `GET /v1/pages/{id}/markdown` work, and does the slot rewrite stabilise it? | **Yes to both, measured.** Plus two findings that change the design — see §2. |

Genuinely still open, both blocked on re-setting the credential to
`Notion-Version: 2026-03-11`:

- **Are resolved comment threads reachable at all?** If not, record it
  as an upstream gap.
- **Can we tell a body-less page from one with a body without spending a
  request?** 71% of this workspace is property-only, so the answer is
  worth roughly 8,700 requests on a backfill. Nothing in the search
  result obviously says. If there is no signal, the backfill simply
  costs ~12,300 markdown calls (~1 hour at 3 req/s) and steady state is
  unaffected.
- **What is the full set of unrepresentable `alt` types?** `button`,
  `alias`, `drive` so far. The list decides what the renderer can
  promise.
- **Does a database row's `last_edited_time` move on a property-only
  edit?** If yes, a property change re-fetches a body that did not
  change — wasteful but correct. If no, body edits could be missed —
  which would be a correctness bug, not a cost one.

Two smaller ones worth an answer before the schema is frozen:

- Does `last_edited_time` on a *database row* move when only a property
  changes, or only when body blocks change? The watermark's precision
  depends on it.
- Does the empty-body response seen once in ~130 search requests
  correlate with the 3 req/s limit, or is it unrelated flakiness? The
  retry policy differs.

## 8. What shipped, and what did not

Built:

1. Live spike; every measurement in §2.
2. New `notion_config` — `roots` as an optional allowlist, and the
   "you must name a starting point" rule gone.
3. New raw schema: `pages`, `page_markdown`, `comments`,
   `notion_attachments`. No `blocks` table.
4. Download: root walk over `<page>` / `<database>` tags, page +
   markdown + comments, the slot rewrite, attachment CAS, bounded
   follow-ups for truncated subtrees.
5. Render: markdown passthrough with a properties table for body-less
   pages. `render.rs` went from 1,273 lines to ~490, `grid_rows.rs`
   from 763 to ~490, and `unofficial.rs` is gone with the inbox.
6. Fixture rebuilt on `notion_page_markdown` events. Its comments now
   carry a `page_id`, which they never did — so comment threads reach
   the grid for the first time.

7. `users` — page authors resolved one id at a time and cached, which
   turned every Notion page's author in the grid from `00000001` into
   a name. Comment authors never needed it.
8. `comment_anchors` — one `GET /v1/blocks/{id}` per *commented* block,
   so a thread opens with a quote of what it is about instead of an
   opaque uuid.

Not built, deliberately:

- **Deletion pass.** `filter: {in_trash: true}` is not wired.
- **Data-source rows.** Search returns them, so they arrive as ordinary
  pages; what is missing is the schema and a per-data-source query for
  workspaces past search's reach.
- **The `entity_id_str` port.** `docs/dev/entity_ids.md` still lists
  notion as pending, and it still is.
- **The shared listing-cache helper** — parked as
  [#295](https://github.com/imbue-ai/datalib/issues/295).
