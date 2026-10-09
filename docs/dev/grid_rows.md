# `grid_rows` — the union table behind the grid

The grid in `datalib/ui` shows one row per displayable thing in the
mirror: a conversation, a message, a content block, a PR, a page, a
storage measurement. Every source's render writes a denormalized
projection of its records into one table, **`grid_rows`**, and the
`unified_index` applet reads it with one query and no per-provider
branches.

The index holds one more table the grid does not read: `problems`,
every source's render-store `problems` copied in whole by `grid_index`
and served by the applet at `/problems`, which filters, sorts, groups
(`/problems/groups`) and pages the way `/search` does, through the keys
declared on `ProblemRow` — see
[`plans/problem_visibility.md`](plans/problem_visibility.md).

## Why a union table

1. **One definition of each column.** When the grid grows a column
   (`channel`, `source_url`, …), one struct moves; the DDL, the column
   list and the search keys are derived from it.
2. **One query path.** Adding a provider adds rows, not a query: the
   query, filter and sort code in `datalib/backend/unified_index/`
   stays put.
3. **No joins at query time.** A message row already carries its
   conversation's name, account and project, so the grid renders
   straight off the projection.

**Not a materialized view.** The mapping is not pure SQL — timestamps
are bumped to order blocks, JSON fields are parsed out of raw payloads
— so the rows are built in Rust beside the rest of the render code,
with no dependence on Dolt-specific features.

## Source of truth

The hand-written `GridRow` struct in
`datalib/backend/schema/src/grid_rows.rs` defines the row shape; there
is no codegen step. Each field carries:

- `#[col(sql = "…")]` — the portable DDL type. Nullability is inferred
  from `Option<T>`.
- `#[derived(name = "…", sql = "…")]` — a column computed at index time
  (e.g. `created_at_utc` / `created_offset`, from `created_at`). Present
  in the DDL but absent from the struct.
- a doc comment saying what the column *means*.

`#[derive(PortableTable)]` (in `datalib/backend/etl/macros`) produces
the `DDL`, `COLUMNS` and `TABLES` consts from the struct; `init_schema`
in `etl/render/src/grid_index.rs` applies the DDL. The authoritative
list of every store's tables and columns is the `schema_inventory`
golden,
`datalib/backend/schema_inventory/tests/snapshots/inventory__schema_inventory.snap`.
The TypeScript side (`SearchRow` in `datalib/ui/src/api.ts`) is kept
by hand.

## Producer side

A source's render crate (`datalib/backend/etl/providers/<p>_render/`)
builds its rows with `GridRow::builder()` — most of them through a
shared layer rather than directly: `chat-common` for every chat-shaped
source, `contact-common`, `calendar-common`, `forge-render-common`
(github, gitlab) and `timeseries_render`. It writes them into the
source's own render store,
`<root>/<source_id>/render_markdown/indexed_markdown.doltlite_db`. The
`grid_index` step (`build_grid_index` in
`datalib/backend/etl/render/src/grid_index.rs`) stacks those stores
into the unified index: it asks each one for the `dolt_diff` between
the commit the index last consumed (`source_cursors`) and that store's
HEAD, applies each changed document's row set, and copies the
corresponding `markdowns` row across. Each source is one transaction
and one commit, its cursor moving inside it, so a stopped or failed
pass keeps every source it finished.

## Consumer side

The applet's search lists the matching uuids in order
(`ordered_uuids` in `datalib/backend/unified_index/src/dolt_repo.rs`,
over the `WHERE` that `build_where` in `unified_index/src/db.rs` makes
from the query's structured terms), then reads the rows for one page
(`rows_by_uuids`, `search_row_from`) into `SearchRow`s with `preview`
as the Contents cell. The order is newest first: `touched_at_utc`
descending, a document row ahead of the rows inside it at the same
moment.

The keys the search bar takes are declared on `GridRow`'s own columns
(`#[col(…, search = "convo", uuid)]`, and `search(order = …, range = …,
qmd)` on the struct), and the derive turns them into a `SearchTable`
(`datalib_query::table`) that the `WHERE`, the order and the grouping
are all built from. The grid's own column ids (`source_ref`, `snippet`)
map onto `grid_rows` columns in `unified_index/src/grid_columns.rs`.

Every read happens inside one read transaction on a read-only
connection (`DoltRepo::pinned`), so a request sees one commit and the
plain table's indexes serve it. `grid_rows` carries one index for the
newest-first order and one per key the search bar filters on, each
`(key, touched_at_utc, is_document, uuid)`; a filter on a key without
one reads and sorts the whole table
([query plans](doltlite.md#query-plans-and-indexes)). They are declared on the struct
(`#[portable_table(index = …)]`) and created only in the unified
index, not in the render stores that also hold a `grid_rows`.
`every_filter_key_is_served_by_an_index` fails when a key has none.

Free text never reaches SQL: the applet sends it to qmd, maps the hits
to rows by `qmd_path` (below), keeps the ones the query's structured
terms also match (`filter_uuids`), and shows each hit's own matched
lines as its Contents cell. There is no weaker search in its place:
before the first sync builds a qmd index, free text finds no rows and
the answer says `qmd_index_missing`, which the grid shows as a note
rather than an error.

## Adding a column

1. Add the field to the `GridRow` struct in
   `datalib/backend/schema/src/grid_rows.rs`, with a `#[col(sql = "…")]`
   portable type and a doc comment saying what it means, and a setter
   on `GridRowBuilder` (`schema/src/grid_rows_builder.rs`).
2. Fill it where rows are built: the shared layers named under
   "Producer side", and the render crates that build rows themselves
   (notion, pdf, perseus, garmin; pdf writes the struct literally).
3. Update `unified_index/src/dolt_repo.rs` — both the
   `SEARCH_ROW_COLUMNS` list and `search_row_from`, which name columns
   by `GridRowColumn` — and `SearchRow` in `unified_index/src/search.rs`
   if the column should reach the API.
4. If it should be a grid column, add it to the `SearchRow` type in
   `datalib/ui/src/api.ts` and declare it in `columns()` in
   `datalib/backend/applets/src/unified_index/columns.rs`, with its type
   from `datalib_columns`. The applet declares the columns and the grid
   draws them by type (`cards/typedColumns.ts`, over the renderers in
   `cards/cellRenderers.ts`); a width or a hover the type cannot know
   goes in `GridCard`'s `columnOverrides`. Map its id to the
   `grid_rows` column it sorts and filters by in `GridColumn::backing`
   (`unified_index/src/grid_columns.rs`), and give that column `search`
   on its `#[col]` so Keep only, Exclude and dropping it on the search
   bar work; `every_filter_key_is_served_by_an_index` then asks for an
   index or a place in its `SCANS` list.
5. Re-bake the fixture: `bazelisk build //tests/fixtures:ingested_tng`.

On an existing root, a render store and the grid index change shape only
when the step that writes them runs. The first launch of the new build
asks each of them (`--migrate`); each compares its store's recorded
shape with this build's and answers that it needs to run again, and the
app offers to re-render them all, downloading nothing; until then a
source's own sync re-renders that source
([dag README](../../datalib/backend/dag/README.md) § "Upgrading a
root"). The render step then sees its own DDL hash moved (it is one of
the render params, `_store_schema`) and re-renders every document into
the new shape, and the grid index rebuilds itself from the stores.

## Adding a provider

1. Add the provider's crates under `datalib/backend/etl/providers/`
   (`<p>`, `<p>_render`, `<p>_config`; see AGENTS.md § "Ingest and
   render are separate crates"), a `Provider` variant
   (`schema/src/providers.rs`) and an `IdNamespace` variant
   (`datalib/backend/id/src/lib.rs`). The render crate emits `GridRow`s
   with the right `provider` / `kind` / `source_label` — through a
   shared layer where one fits — and hands each finished document to
   `ctx.emit_doc`.
2. Wire it into `datalib-step`: add it to the deps of
   `datalib/backend/datalib_step` and to the dispatch table in
   `datalib/backend/datalib_step/src/dispatch.rs`, then declare its
   ingest/render step pair in the config and name the render step in
   `grid_index`'s `inputs` (the wizard does this for a source it adds,
   with the source's qmd steps); `grid_index` reads exactly the stores
   its inputs name.
3. Give it an icon and a catalog entry (`datalib/ui/src/assets/README.md`)
   — the query path itself does not change.

## Column conventions

The rules every provider follows. What a given provider puts in a given
column is in the code that builds its rows (see "Producer side"), and
the TNG fixture's goldens show the result per provider.

### `uuid` and the backpointer

Minted by `datalib_id` for every provider: the record's `created_at` in
the leading bits where it has one of its own, then a hash of
`(provider, source_id, upstream_account, upstream_entity_kind,
upstream_id)`. The recipe, which rows carry a stamp, and the account
each provider names are in [`entity_ids.md`](entity_ids.md).

### `kind` and `source_label`

`kind` is the display label for the Kind column ("LLM Thinking",
"GitHub PR", "Notion Comment Thread") and may be reworded freely; it is
not `upstream_entity_kind`, which the id depends on. `source_label` is
the plain product name ("Claude", "Slack").

### `is_document`

True on exactly one row per rendered markdown document — the row
whose `uuid` is the document's `markdown_uuid` — and false on every
row inside it. Every row carries a `markdown_uuid`, so this is not
"has a document"; it is "opening this row opens a whole document
rather than a place in one". Each renderer says so through
`GridRowBuilder::is_document`, and a document with any number of them
other than one fails the render (`document_row` in
`etl/render/src/grid_index.rs`), so the flag is declared, never
inferred. A Browse of a source opens on these rows (`is:document`).
Several sources have more than one document kind: Claude's `Chat` and
`Project`, Notion's `Notion Page` and `Notion Comment Thread`,
LinkedIn's `Contact` and `LinkedIn Chat`.

### `created_at`, `modified_at` and `touched_at`

All three are the record's own stamps, kept as the source wrote them
(see the timestamp convention in AGENTS.md); each gets a `_utc` twin
and an offset column at index time. `touched_at_utc` is what the grid
sorts on, newest first, and Touched is the one date column the grid
shows by default (a calendar's Browse shows Created instead);
`created_at_utc` is what `before:`/`after:` filter on.

- **A document row:** `created_at` is the earliest moment in the
  document and `modified_at` the latest. Through `chat-common` that is
  the min and max over the items' stamps, reactions counting towards
  the max.
- **A row inside a document:** `created_at` is its own stamp (for a
  chat item with none, bumped a microsecond off its parent), and
  `modified_at` is the edit stamp where the source keeps one —
  **null** otherwise, never a copy of `created_at`: null means "not
  known to have changed since it was created".
- **`touched_at`** is when the record last changed at its source. The
  builder sets it to `modified_at`, else `created_at`, so a provider
  sets it only when the last change is neither. Calendar is the one
  that does: an event's `created_at` is when it happens, often years
  ahead, so its `touched_at` is its edit stamp, else when it was added.
- A record with no stamp of its own (a vCard has no creation event)
  leaves `created_at` null.

`markdowns.created_at` / `modified_at` are copies of the document
row's, taken by the render store when it writes the document.

### `account`, `org_uuid`, `org_name`

`account` says whose mirror the row came from, resolved the same way
everywhere (`datalib_etl_chat_common::account_label`): the login's
email where the raw store has one, else its name, else the provider's
own id — so grouping by Account groups one person's data across
sources, and a raw id in the column means "this login has no row to
resolve against". A source with no login at all (a PDF folder, a
`.vcf` file, YoLink) leaves it null; the source's id is on
`source_id`, not here. `org_uuid` / `org_name` are the organization a
login lives inside, where the provider has one (Claude's org, Slack's
workspace).

### `conversation_uuid`, `preview`, `content_hash`

`conversation_uuid` is the row's own `uuid` for a document row and the
document's for everything inside it.

A producer hands the builder the row's whole text (`.body(…)`), and the
builder keeps two things from it: `preview`, the first 240 characters
of it as plain text on one line (`PREVIEW_CHARS`), which is the grid's
Contents cell; and
`content_hash`, blake3 of the whole body, so a change past the preview
still changes the row. The body itself is not stored — the rendered
markdown holds it, and qmd's index of that markdown is how free text
finds it.

The body is markdown, and `datalib_schema::plain_text` turns it into
what a person reads: tags, images, link targets, heading and quote
marks, emphasis and code fences go, and a `<details>` block reads as
its summary unless it is all the body there is. Text inside a code
fence or a code span is kept as written, backslashes and `*` included,
since that is what the page shows. A qmd hit's snippet
goes through the same pass. So what a producer puts in the body is
what the row *says*, in the order that matters: a calendar event's
description comes before its guest list, a chat's document row leaves
out its asides, and an email's mailbox labels are not in it at all.
Changing what a producer puts there changes every row it wrote, so it
takes a `RENDER_VERSION` bump to reach a store already rendered.

### `qmd_path` and `source_id`

`qmd_path` is `<source_id>/render_markdown/<renderer-specific tail>`,
where `<source_id>` is the group's id — its directory under the data
root, never the display name the config may also give it.

For a given `markdown_uuid`, `grid_rows.qmd_path` must be byte-equal to
that markdown's `markdowns.md_path` — the applet's qmd mapping
(`GridIndex` in `unified_index/src/qmd/mapping.rs`) keys rows by this
path to resolve search hits, and a row whose path doesn't match what
qmd reports is silently dropped from free-text results.
`//tests/fixtures:ingested_tng_test` asserts it across providers.

`source_id` is derived from `qmd_path` at index time: its first
segment, except a storage row (provider `datalib`), which sits under
the source it measures and is filed under `datalib`
(`GridRow::derived_source_id`). The `source_id:` filter compares it.

### `byte_size`, `item_count`

Two nullable measurements. What each one measures is decided per
`kind`, and this table is the list:

| provider.kind | `byte_size` | `item_count` |
|---|---|---|
| datalib.Source Size | bytes under `<name>/ingest` | files under it |
| datalib.Store | the `.doltlite_db` file's size | — |
| datalib.Table | — (see below) | rows in the table |
| pdf.document | — | 1 |
| any chat-common conversation row | the sum of its items' `byte_size` | its messages: what a person or an assistant said |
| any chat-common message row | the body's UTF-8 length | 1; — for a tool call, a tool result or a system note |
| any chat-common reaction row | — | — |
| a sensor or Garmin page | — | its readings: rows of the raw store, or for Garmin every day of a metric with data, every weigh-in and every activity |
| a GitHub PR, a GitLab MR, a Notion comment thread | — | itself and its comments; each comment row 1 |
| a calendar event, a contact, a Notion page, a Perseus book or chapter | — | 1 |

On a `datalib.*` row, `byte_size` is bytes on disk **as of the last
render that rewrote the row** — see "Storage rows" below for why that
is not "now". On a chat-common row it is the message body — the same
string it passes as the body — and nothing else: not the attachments,
whose sizes only some providers know, and not the raw payload, which
the renderer never sees. So a conversation's `byte_size` is exactly the
sum of its item rows', and its `item_count` is how many of them were
said: a tool call, its result and a system note are in the transcript
but are not messages, so their rows carry no count. Reactions have
their own rows but are not messages, so they carry neither.

**Every document row carries an `item_count`**; the render store
refuses one that does not. It is what the Manage screen's Items column
sums, through `markdowns.item_count` — a copy, like the document's
stamps — so a new renderer has to decide what its documents count.

Bytes on disk and a byte length of content are different measurements,
and one kind must never mix them: a producer that measures a file
reports the file, and a producer that can only compute a logical size
for something that *has* an on-disk size leaves the column NULL. Adding
a kind here means adding a row to this table.

`item_count` is deliberately unitless. What is being counted is `kind`'s
job to say: a Table counts rows, a Source Size counts files, a
conversation counts messages, a sensor page counts readings.

### `diff_status`, `diff_changed_columns`

**NULL on every row a real source renders.** Set only by a diff group
(`docs/dev/plans/completed/diff_renderer.md`), whose rows are two renders of the
same source subtracted: `diff_status` is `added`, `removed`, `modified`
or `unchanged` (`datalib_schema::diff_status::DiffStatus`), and for a
modified row `diff_changed_columns` names the columns that differ,
sorted and `|`-joined. A non-NULL `diff_status` is the one thing that
says a row came from a diff tree; nothing else about the row changes —
`provider` is still the source's, so its icon and CSS apply.

## Storage rows: what a source weighs

Every source's render wave ends by measuring its own raw store and
emitting a handful of rows tagged `provider = "datalib"`, `source_label
= "Storage"`, kinds `Source Size`, `Store` and `Table`. That is what
gives a download-only source — `fsindex`, `media` — a place in the grid
at all: they render no documents, so without this they appear nowhere.
`source_id:datalib` is "show me what everything weighs". The code is
`datalib/backend/datalib_step/src/introspect.rs`; its header has the
reasons behind the rules below.

**They are filed under `datalib`, not under the source they measure.**
The report's markdown sits in the measured source's
`render_markdown/`, the one tree the render step may write, so its
`qmd_path` starts with that source's id. `GridRow::derived_source_id`
asks `provider` first, so a `datalib` row gets `source_id = datalib`
whatever directory it came out of, and "what claude weighs" never lands
in the same bucket as the Claude conversations. Which source a
measurement describes is `markdowns.source_id`, the row's
`conversation_name` (`<source_id> storage`), and its `upstream_id` (the
measured path). A group configured with the literal id `datalib` would
collide with this.

**The grid holds the current value; the history lives elsewhere.** Each
measurement is one row, keyed on `(source, kind, measured path)` and
nothing else, so a re-render overwrites it. The series behind it
accumulates in `source_measurements`, a table in the same per-source
`indexed_markdown.doltlite_db` that `problems` lives in, keyed
`(subject, measured_at_utc)`. It stays out of `grid_rows` because a
series row's id must carry its time, and the grid would then return a
copy of every file per run and bury real data under measurements.

**A `Table` row carries no byte size.** Doltlite has no `dbstat`
([query plans](doltlite.md#query-plans-and-indexes)), and its chunks
are shared between tables and between commits, so no honest per-table
number exists. The amalgamation could walk a table's chunks
(`doltlite_chunk_walk.c`), but none of that is exposed to SQL.

**Scope is `<name>/ingest`, not the whole tree.** `render_markdown` is
datalib's own output, `system/usage.doltlite_db` already tracks it per
step, and measuring it from inside the thing that writes it would grow
the store it just measured on every run.

**What re-renders the report is a count, never a byte.** A doltlite
store's size is not reproducible — rebuilding the TNG fixture from
byte-identical inputs moves six of its sixteen sources by 1-22 bytes,
in a different direction each time — and a bookkeeping mutation
rewrites chunks with no row added. So byte sizes are reported but
never compared (`introspect::counts_unchanged`), and datalib's own
tables (`doltlite_raw::SHARED_TABLES` — `sync_runs`, `sync_scope_state`
and the like — and every `<table>_bookkeeping` sidecar) are left out of
the report. Otherwise every run would rewrite the report and hand
`grid_index` a diff forever. This is the download side's *volatile
field* idea in another shape: the bytes are signal, so they are
**reported but not hashed**, neither sorted nor dropped.

So read `byte_size` on a storage row as **how big the raw store was
the last time this source's contents changed**, not as how big it is
now. For bytes on their own cadence, `system/usage.doltlite_db` keeps a
per-step series and commits nothing, which is what lets it sample
freely.
