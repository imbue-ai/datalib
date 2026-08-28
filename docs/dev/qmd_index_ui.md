# Design: QMD index state in the grid, selective re-indexing, live progress

**Status: partly built, 2026-08-27.** Slice 1 (the read-only `Indexed`
/ `Embedded` columns) has landed — the sections describing it have been
rewritten to describe the tree. Everything from
[Writing it](#writing-it-selective-re-index-is-a-job) onward is still a
proposal. Per
[`AGENTS.md`](../../AGENTS.md), don't cite this file as a description of
the tree — it describes work we intend to do. Every claim about *current*
behavior below carries a `file:line`; those were checked against the tree
on 2026-08-27. When a slice lands, rewrite the section it makes real and
delete the rest.

Three asks, in the order a user meets them:

1. a per-row ✅/❌ saying whether this row's markdown doc is in the qmd
   index yet;
2. select rows → run the indexer on just those documents;
3. watch an indexing run progress in the UI.

All three are possible. **(1) is built.** (3) needs one honest piece of
new work — qmd tells us nothing machine-readable while it runs. (2) is
the one qmd genuinely does not support, and is where owning more of qmd's
data model pays for itself.

Building (1) changed the plan for (2) in one place worth reading even if
you skip the rest: the join key turned out to be the content hash, not
the path, which delivered freshness for free and made the proposed
`handelize` port and `markdowns.md_sha256` column both unnecessary. See
[The join](#the-join).

## What the tree gives us today

**The qmd index is a plain SQLite file in WAL mode** —
`third-party/qmd/src/store.ts:833`, at
`<root>/unified_index/qmd/index.sqlite`
(`unified_index/src/qmd/mod.rs:QMD_INDEX_REL`). Four tables matter:

```
content(hash PK, doc, created_at)                         -- store.ts:842
documents(id, collection, path, title, hash, created_at,
          modified_at, active, UNIQUE(collection, path))   -- store.ts:852
content_vectors(hash, seq, pos, model, embed_fingerprint,
                total_chunks, embedded_at, PK(hash,seq))   -- store.ts:882
documents_fts(filepath, title, body)  -- fts5              -- store.ts:917
```

**`qmd update` autocommits per file.** `reindexCollection`
(`store.ts:1272`) loops over the glob and inserts/updates one document at
a time with no wrapping transaction. Combined with WAL, that means **a
concurrent reader sees an in-flight run advance, document by document.**
This is the single most useful fact in this document; most of the design
falls out of it.

**`documents.path` is `handelize(relative_path)`** (`store.ts:1309`,
`handelize` at `store.ts:1971`): every run of non-alphanumeric characters
becomes `-`, the final extension is preserved, case is **not** folded.
Note this is *not* `qmd::mapping::norm_path` (`unified_index/src/qmd/
mapping.rs:53`), which lowercases and only collapses `[_-]`. The two
serve different keys — `norm_path` matches what qmd reports back in a
*search hit* URI; `handelize` is what's stored in `documents.path`. Don't
reuse one for the other.

**`documents.modified_at` is the file's mtime as of indexing** — set from
`statSync(filepath).mtime` on insert and on hash-change update
(`store.ts:1343`, `store.ts:1352`) — except in the title-only-changed
branch, which stores `now` (`store.ts:1334`).

**"Needs embedding" has an exact definition**, and it isn't "no vectors":
`getHashesNeedingEmbedding` (`store.ts:2118`) counts documents whose hash
has no `content_vectors` rows *for the current `(model,
embed_fingerprint)`*, **or** fewer rows than `MAX(total_chunks)` — i.e. a
partially-embedded document is not embedded.

**qmd's per-file progress is TTY-only.** Both the update loop
(`cli/qmd.ts:731`) and the embed loop (`cli/qmd.ts:2039`) guard their
writes with `if (isTTY)`, and `isTTY` is `process.stderr.isTTY`
(`cli/qmd.ts:289`). The indexer spawns qmd with inherited stdio
(`qmd_indexer/src/lib.rs`, `run_qmd`), which under `datalib-http`'s
worker is a pipe. **So today a UI-triggered index run emits literally no
progress until qmd prints its final `Indexed: N new, …` summary line.**

**`qmd update` cannot be scoped to a file set.** `updateCollections`
(`cli/qmd.ts:660`) iterates every registered collection and reindexes
each one's whole glob. `qmd embed` takes `-c <collection>` and nothing
finer (`cli/qmd.ts:4404`). There is no `--only`, no path argument.

**The step is a black box by construction.** `qmd_index`
(`datalib_step/src/qmd_index.rs`) calls `progress.set_message("qmd
index")` and then blocks in `run_index` — no `progress_length`, no
`progress_inc`. It deliberately reports no output version, because qmd
touches its sqlite on every pass.

**The grid already has the affordances.** AG Grid Enterprise is
registered (`ui/src/cards/GridCard.ce.vue:80`), multi-row selection is
on (`GridCard.ce.vue:903`), and `resolveTargetRows`
(`GridCard.ce.vue:181`) already implements "act on the selection if the
right-clicked row is part of it, else on that row" for the context menu.
A "Re-index these documents" item is a few lines in the existing
`getContextMenuItems`.

**There is already a live progress channel.** `datalib-dag` emits NDJSON
on stderr; the worker's `TaskBoard` (`http/src/worker.rs:86`) folds
`run_plan` / `step_start` / `progress_length` / `progress_inc` /
`progress_message` / `step_finish` / `run_summary` into a per-task board
and fans it out over `GET /api/sync/stream` as SSE
(`http/src/lib.rs:1124`), which the UI already consumes
(`ui/src/api.ts:openJobStream`, `ui/src/sync/progress.ts`).

**Applets cannot stream.** The gateway's `forward`
(`http/src/applets.rs:841`) is a hand-rolled HTTP/1.1 client that
`read_to_end`s the whole response. Its own doc comment names this: "not
enough for streaming, which is the first thing to revisit when an applet
wants server-sent events." **Any live channel has to be
`datalib-http`'s, not the applet's.**

**The job worker is strictly serial.** `worker::run`
(`http/src/worker.rs:343`) claims one job, runs it to completion, then
claims the next. This is load-bearing below.

## ✅/❌ is the wrong shape — so it shipped as two columns

The interesting distinction isn't binary, and the one that matters most
to a user is invisible in a two-state badge: **a document can be indexed
for keyword search and still be missing from semantic search**, because
`update` and `embed` are separate passes and `embed` is the slow one.
That is why the grid carries two columns rather than one:

| column | ✅ | ❌ | — |
|---|---|---|---|
| `Indexed` | this content is in the keyword index | not in it (never indexed, or re-rendered since) | no rendered document / unreadable / no index yet |
| `Embedded` | complete vector set; semantic search reaches it | vectors missing or partial | same as above |

The em dash is not decoration. `null` (unknown) and `false` (positively
absent) are different claims, and rendering a red ❌ for "this row has no
rendered document" or "the index would not open" asserts something we
did not check. The API returns `indexed`/`embedded` as
`boolean | null` for exactly this reason, plus a `note` the cell shows
as its tooltip.

Two states that were in the original design and did **not** ship:

* `stale` — folded into `Indexed: ❌`, because the hash join can't tell
  "indexed under an older hash" from "never indexed". See
  [Freshness](#freshness).
* `ineligible` — a document outside the `*/rendered_md/**/*.md` mask
  (`qmd_indexer/src/lib.rs:41`) will never index as-is, and reads as a ❌
  that can never turn green. No renderer currently writes outside the
  mask, so there was nothing to show; worth adding the moment one does.

## Where the truth lives

**qmd's `index.sqlite` is the only authority, and we read it live.** The
tempting alternative — materialize the state into a `grid_rows` column
during `grid_index` — is wrong twice over: `grid_index` and `qmd_index`
are different steps that run at different times, so the column would be
stale the instant an index run touched anything; and the grid index has
exactly one writer by design
(`AGENTS.md`, "One writer per file"), which is the property that lets
every reader open it at once.

So: **no derived copy, no cache with an invalidation story. One SQL query
against the file qmd itself writes.**

### The join

**As built: the content hash.** `documents.hash` is a plain SHA-256 over
the file's UTF-8 bytes (`store.ts:2365`), which we can compute exactly,
from the same bytes, with no shared code:

```
∃ documents row (collection='mirror', active=1) with
  hash = sha256(bytes of <root>/<markdowns.md_path>)
```

This replaced the original plan, which was to port qmd's `handelize`
path mangling (`store.ts:1971`) to Rust and join
`documents.path == handelize(markdowns.md_path)`. The port would have
had to reproduce Unicode `\p{L}`/`\p{N}` classes and an emoji→hex step
with no regex crate in the workspace, and then stay in step with a
vendored dependency we don't build. The hash join needs none of that.

It is also **stricter, not just simpler**. qmd decides whether to
re-index a file by comparing this very hash (`store.ts:1332`), so "the
index holds a row whose hash equals the file's hash right now" *is*
qmd's own definition of up to date. A path join would report a
re-rendered but not-yet-reindexed document as indexed; the hash join
reports it as not indexed, which is what search will actually do.

Verified against the real fixture index by
`//datalib/backend/unified_index:qmd_index_state_test`, which walks the
rendered tree and asserts two things that would otherwise rot silently:

* every rendered document matches a `documents.hash` (the claim a qmd
  version bump would break — nothing else in the suite would notice);
* editing one rendered file flips **exactly that document** to ❌ and
  restoring it flips it back, with every other document untouched.

The second is the behavior the columns promise and the easy one to get
subtly wrong: the grid is message-level while the index is
document-level, so a bug in either hop — row → document by
`markdown_uuid`, or document → qmd by content hash — shows up as
neighbouring rows flipping together, or as nothing flipping at all.
Both look plausible on screen. The test drives
`resolve_markdown_states`, the same function the HTTP handler calls, so
it cannot pass while the shipped path is broken; the handler is left as
request shaping (dedupe, cap, error mapping).

Cost: one file read + hash per document in the current result set. The
grid's default limit is 200 rows, which collapse to far fewer documents,
and the endpoint caps a request at 2,000. `markdowns.md_sha256` (below)
remains the way to remove the reads if that ever matters.

One consequence worth naming: two rendered files with byte-identical
content share a hash, so if one is indexed both report indexed. They
also produce identical search behavior, so the badge is still telling
the truth about the content — just not about the file.

### Freshness

The hash join *is* the freshness check, so the two options the original
version of this section weighed are both moot for slice 1:

1. ~~`documents.modified_at` vs `markdowns.rendered_at`~~ — comparing a
   filesystem mtime against our own stamp across two clocks and two
   timestamp conventions. Not needed.

2. `md_sha256: Option<String>` on `MarkdownRow` — still worth doing
   eventually, but now as a *performance* change (skip the file reads),
   not a correctness one. The renderer has the digest in hand at write
   time.

What the shipped version does **not** do is distinguish `stale`
(indexed under an older hash) from `absent` (never indexed). Both read
as `Indexed: ❌`, which is correct for "can search find this?" but less
informative than it could be. Telling them apart needs the path after
all — a second lookup by `documents.path` when the hash misses — and is
the natural next increment if users ask "was it ever indexed?".

## Reading it: one applet endpoint

```
POST /applet/unified_index/qmd_state
  { "markdown_uuids": ["…", …] }
→ { "index_present": true,
    "summary": { "documents": 51, "embedded": 51 },
    "docs": { "<markdown_uuid>": { "indexed": true, "embedded": true }, … },
    "errors": [] }
```

POST rather than GET because the uuid list is as long as the grid's
result set. Not folded into `/search` because the two change on
different clocks: search results change when the user types, index state
changes while a run is in flight, and the grid wants to refresh the
badges without re-running the query.

`index_present: false` covers the pre-first-sync root, where there is no
`index.sqlite` at all: every document is reported `indexed: false` with
the note "no qmd index yet" — a fact, not a failure.

The endpoint dedupes uuids (many grid rows share one markdown), caps a
request at 2,000 documents, and reports the truncation through the same
`errors` channel `/search` uses, which the UI raises as a toast.
Everything the caller asked about comes back, so a client can't
misread an omission as a `false`.

Reading plain SQLite from Rust: `sqlx` with the bundled `sqlite` feature
is already the workspace's client (`backend/Cargo.toml:102`). It opens
qmd's file directly — this is not a doltlite store, so the usual "stock
sqlite3 can't open these" warning in `AGENTS.md` does not apply. Opened
`read_only` + `create_if_missing(false)`, so a bug on our side cannot
take a write lock on a file a sync is rebuilding.

**Ordering.** The shipped client guards with a monotonic request
sequence and a single in-flight `AbortController`, discarding any
response superseded while in flight. The original design proposed a
server-side `epoch` (SQLite's `PRAGMA data_version`) instead; that
becomes worth adding when a *run* is pushing updates and two clients can
disagree about which snapshot is newer — i.e. with slice 4, not before.

## Writing it: selective re-index is a job

### Why a job, not an applet endpoint

The applet is a reader. Making it a writer would put a second writer on
`index.sqlite` racing the `qmd_index` step of any concurrent sync.

The job queue already solves this: **the worker runs exactly one job at a
time** (`worker.rs:343`). A `qmd_index` job therefore cannot overlap a
sync run, for free, with no lock file and no new invariant to maintain.
It also inherits cancel (SIGTERM → grace → SIGKILL), the per-job log at
`<root>/system/job-logs/<id>.log`, and the SSE stream — all three of
which a long embed run genuinely needs.

The cost is one schema change and one dispatch branch:

- `sync_jobs` has no payload column (`app_schema/src/sync_jobs.rs`). Add
  `payload: Option<String>` (JSON). The alternative — smuggling a
  request-file token through `source_name` — saves a column and costs a
  concept; not worth it.
- `sync_enqueue` (`http/src/lib.rs:1098`) currently rejects every kind
  but `"all"`. Add `"qmd_index"`, whose payload is
  `{"markdown_uuids": [...]}`.
- `run_job` branches: `"all"` → `datalib-dag` as today; `"qmd_index"` →
  `datalib-qmd-indexer --only-docs <file>`. The NDJSON the worker parses
  is the same either way, so `TaskBoard`, the SSE event, and the UI need
  **zero** changes to render this run's progress.

### What `--only-docs` does

This is the part qmd doesn't support. Two routes, and they're a natural
sequence rather than a fork in the road:

**Now, without touching qmd: write the `documents` rows ourselves.** For
each selected file: read it, SHA-256 it, `INSERT OR REPLACE` into
`content` + `documents`. qmd's schema even anticipates outside writers —
`documents_ai`/`_au`/`_ad` triggers keep `documents_fts` in sync "for
callers that write directly to documents" (`store.ts:923`). Then shell
`qmd embed`, which embeds exactly the hashes that lack vectors — i.e.
precisely the documents we just wrote. **Selective indexing with no qmd
change, because the selection is expressed as state rather than as a
flag.**

Two caveats to state honestly. The trigger comment says production
indexing paths rebuild FTS entries in TypeScript "so CJK text can be
normalized before it reaches the unicode61 tokenizer" — so a doc indexed
by our path gets un-normalized CJK in FTS until the next full `qmd
update` reconciles it. And this makes datalib a writer of qmd's tables,
which needs the same one-writer-at-a-time discipline the job queue
provides (and is the reason the *applet* must not do it).

**Medium term, with the fork:** `qmd update --only <path>…` and `qmd
embed --only <path>…`. Both are small — `reindexCollection` filters
`files` after the glob; `generateEmbeddings` filters the pending-hash
query — both are upstreamable, and both remove the CJK caveat by putting
our selection back on qmd's own code path. That is the version I'd want
to land eventually; the state-based route above is what makes the feature
shippable before the fork exists.

### Row selection → document selection

Grid rows are message-level; the index is document-level. So the action
dedupes `markdown_uuid` across the selection, drops nulls, and *says so*
in the menu item: `Re-index 87 documents (3,412 rows)`. Silently
indexing 87 things when the user selected 3,412 is the kind of surprise
that erodes trust in the button.

## Live progress — the part worth getting right

### Rule 1: push the run, pull the documents

Do **not** try to push per-document state changes to the UI. That's a
fanout of one message per document per run, an ordering problem, a
reconnect-gap problem, and a "which of my 200 visible rows does this
concern" problem — and the applet can't stream anyway
(`applets.rs:841`).

Instead: **the run's progress is pushed** (it's one small object,
changing a few times a second, on a channel that already exists and
already auto-reconnects), and **document state is pulled** from the
authority, re-fetched when the pushed progress says something changed.
The pulled response is a full snapshot for the uuids asked about, so a
missed event costs latency, never correctness. This is the property that
makes the whole thing robust: **there is no incremental state in the UI
that can drift.**

### Rule 2: derive progress from the store, not from qmd's chatter

qmd prints nothing useful into a pipe (`cli/qmd.ts:731,2039`). Three ways
to fix that:

1. Give qmd a pty so `isTTY` is true, then scrape `\rIndexing: N/M`.
   Rejected: parsing a human progress line out of a fake terminal, and a
   qmd patch release can change it silently.
2. Fork qmd, add `--json-progress` emitting NDJSON. Clean, and worth
   doing — but it dates immediately if we ever bump the pin without the
   patch.
3. **Poll the index itself.** The indexer already knows the file list
   (it built it, or it's the `--only-docs` list). While `qmd update`
   runs, a background thread queries the read-only connection every
   ~400ms:

   ```sql
   SELECT COUNT(*) FROM documents
    WHERE collection='mirror' AND active=1 AND hash IN (…the run's hashes…);
   ```

   …and emits `progress_length` once and `progress_inc` as the count
   advances. Same again for the embed pass against `content_vectors`.
   This works **because** `reindexCollection` autocommits per file
   (`store.ts:1307`) under WAL (`store.ts:833`).

Take (3), and take it even after the fork: it measures **what actually
landed in the index**, not what a subprocess claimed on stderr, and it
keeps working across qmd version bumps because the schema is a far more
stable interface than the log format. It also means the *same* query
powers the badge and the progress bar — one definition of "indexed",
used in both places, which is why they can never disagree on screen.

Note the embed pass has a cold-start pause: `qmd embed` may download a
~300 MB model before the first vector appears
(`qmd_indexer/src/lib.rs:105`, `models_present`). Emit a
`progress_message` for that phase so the bar being pinned at 0 reads as
"fetching model", not "hung".

### Rule 3: the UI's reconciliation rules

In `GridCard.ce.vue`:

- Keep index state in a `Map<markdown_uuid, DocState>` held **outside**
  `rowData`, read by a `valueGetter`. Refresh with
  `api.refreshCells({ columns: ['qmd'], force: true })` — never by
  reassigning `rows`, which would blow away selection, scroll position,
  and any in-flight edit.
- **Refetch triggers**, all funnelled through one debounced (≥750ms)
  function with a single in-flight `AbortController`: rows changed; an
  SSE event arrived for a job of kind `qmd_index` or a task named
  `qmd_index`; that job reached a terminal state (one final settle —
  this is the one that must not be debounced away); window regained
  focus; and a slow 30s poll as the SSE-reconnect backstop, mirroring
  what `SyncProgressChrome.vue` already does with `seed()`.
- **Discard stale responses** by `epoch`, not by request order.
- **Optimistic `indexing` state** for the docs the user just submitted,
  keyed by job id, cleared when that job goes terminal — *including*
  `failed` and `canceled`, where the rows revert to whatever the
  authority says and the error goes in the tooltip. An optimistic state
  that can outlive its job is a permanently-spinning row.
- Sorting/filtering by the column works on the client row model as long
  as every row has a value; make the `valueGetter` return `absent` (not
  `undefined`) for uuids the map hasn't heard about yet.

### What this design makes impossible

Worth stating, since these are the failures a naive version ships with:

- A badge that disagrees with search — same query backs both.
- A badge stuck green after a re-render — freshness is hash-compared.
- Two runs writing the index at once — the worker is serial.
- A refresh storm during a run — one debounced fetch, one in flight.
- Lost updates across an SSE reconnect — every fetch is a full snapshot.
- Out-of-order overwrites — `epoch` guard.
- A row spinning forever — optimistic state is job-scoped.

## Slices

1. ~~**Read-only.**~~ **Built.** The `qmd_state` endpoint, the two
   columns, the tooltips, and a "N of M documents searchable" line under
   the grid. No writes, no new job kind. What it cost, in the end:
   `unified_index/src/qmd/index_state.rs` (the reader),
   `IndexRepo::md_paths_for`, one applet route, and ~120 lines of
   GridCard. The `handelize` port the plan called for was not needed.
2. **Freshness detail.** `markdowns.md_sha256` to drop the per-request
   file reads, and a path-fallback lookup so `stale` can be told apart
   from `absent`. Both are refinements now, not prerequisites.
3. **Selective re-index.** `payload` column, `qmd_index` job kind,
   `--only-docs`, the context-menu action.
4. **Live progress.** The store-polling progress thread in the indexer;
   the UI refetch rules. Also lights up the *full* `qmd_index` step
   during an ordinary sync, which today shows as one opaque task for its
   entire duration.
5. **The fork.** `--only` and `--json-progress` upstream; drop the
   direct-write path and its CJK caveat.

## Incidental findings

Both surfaced while verifying the join against the fixture.

1. **Fixed here.** `grid_rows.qmd_path`'s doc comment
   (`schema/src/grid_rows.rs`) described paths as rooted under
   `rendered_md/<provider>/…` and listed a per-provider mapping table
   that no longer matched any provider. The real shape is
   `<source_name>/rendered_md/<tail>`. Rewritten against
   fixture-verified values.

2. **Filed separately.** The `pdf` provider writes `qmd_path` as
   `docs/{sha256}.md` — no source-name prefix — while its
   `markdowns.md_path` is correct. Since `GridIndex` keys rows by
   `norm_path(qmd_path)` to resolve qmd hits
   (`unified_index/src/qmd/mapping.rs:146`), **every free-text search
   hit inside a PDF resolves to zero grid rows and is dropped**, logging
   `qmd hit resolved to no grid rows`. The index-state columns are
   unaffected — they go through `markdowns.md_path`. Worth a regression
   test asserting `grid_rows.qmd_path == markdowns.md_path` for every
   row, which should hold for all providers.
