# `grid_rows` — the union table behind the grid

The grid shows one row per displayable thing in the mirror: a
conversation, a message, a content block, a PR, a page, a channel, a
storage measurement. Every source's render writes a denormalized
projection of its records into one table, **`grid_rows`**, and the
`unified_grid_and_search_index` applet reads it with one query and no
per-provider branches.

The same file, the grid index, also holds `problems`, which the grid
does not read: the index step's own rows about a render store it could
not read. The applet serves every step's problems at `/problems`, each
read where its step keeps it, filtered, sorted, grouped
(`/problems/groups`) and paged the way `/search` is, through the keys
declared on `ProblemRow`.

## Why a union table

1. **One definition of each column.** A new column moves one struct;
   the DDL, the column list and the search keys are derived from it.
2. **One query path.** Adding a provider adds rows, not a query.
3. **No joins at query time.** A message row already carries its
   conversation's name, account and project.

It is not a materialized view: the mapping is not pure SQL (timestamps
are bumped to order blocks, JSON is parsed out of raw payloads), so the
rows are built in Rust with the rest of the render code.

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
the `DDL`, `COLUMNS` and `TABLES` consts from the struct. In a checkout,
the `schema_inventory` golden
(`datalib/backend/schema_inventory/tests/snapshots/inventory__schema_inventory.snap`)
lists every store's tables and columns. On a data root, ask a store for
its `.schema`: the grid index, `db.sqlite`, is plain SQLite, and a
render store opens as [`doltlite.md`](doltlite.md) says. The TypeScript
side (`SearchRow` in `datalib/ui/src/api.ts`) is kept by hand.

## Producer side

A source's render crate (`datalib/backend/etl/providers/<p>_render/`)
builds its rows with `GridRow::builder()`, most of them through a
shared layer: `chat-common` for every chat-shaped source,
`contact-common`, `calendar-common`, `forge-render-common` (GitHub,
GitLab) and `timeseries_render`. It writes them into the source's own
render store,
`<root>/<source_id>/render_markdown/indexed_markdown.doltlite_db`.

The `unified_grid_and_search_index` step (`build_grid_index` in
`datalib/backend/etl/render/src/grid_index.rs`) stacks those stores
into the grid index. It asks each store for the `dolt_diff` between the
commit the index last consumed (`source_cursors`) and the store's HEAD,
applies each changed document's rows, and copies its `markdowns` row
across. It reads and writes them a page of 500 documents at a time
(`LOAD_PAGE`), each table's rows in a few multi-row statements
(`grid_index::apply_page`), so a source read whole costs memory for one
page, not for the source. The cursor keeps that commit's tree too, so when the store's
history has been folded and the commit is gone, the diff starts from
the commit that holds the same tree. Each source is one transaction,
its cursor moving inside it, so a stopped or failed pass keeps every
source it finished.

## Consumer side

The applet's search lists the matching uuids in order (`ordered_uuids`
in `unified_grid_and_search_index/src/dolt_repo.rs`, over the `WHERE`
that `build_where` in `db.rs` makes from the query's structured terms),
then reads the rows for one page (`rows_by_uuids`, `search_row_from`)
into `SearchRow`s, with `preview` as the Contents cell. The order is
newest first: `touched_at_utc` descending, a document row ahead of the
rows inside it at the same moment.

The keys the search bar takes are declared on `GridRow`'s own columns
(`#[col(…, search = "datalib_document_id", uuid)]`, and `search(order =
…, range = …, ranked)` on the struct; `datalib/backend/etl/macros/README.md`
§"Searched tables" in the repo). The derive turns them into a
`SearchTable` (`datalib_query::table`) that the `WHERE`, the order and
the grouping are all built from. The grid's own column ids
(`source_ref`, `snippet`) map onto `grid_rows` columns in
`unified_grid_and_search_index/src/grid_columns.rs`.

Every request reads inside one read transaction on a read-only
connection (`DoltRepo::pinned`), so it sees one state of the index.
Which index answers which key, and how SQLite is held to it, is in
[`search.md`](search.md) §"The indexes".

Free text never reaches `grid_rows`' SQL: the applet works out what the
query's filters keep first, as `grid_rows` row ids read from the
indexes alone (`db::matching_sql`), ranks the words among those in the
search index, by their words and by their meaning, maps the hits to rows
by their document's id (`markdown_uuid`), and shows each hit's own
matched lines as its Contents cell. Before the first sync builds the
search index, a search still answers its filters, a pasted id or
address among them, finds no words, and says `search_index_missing`,
which the grid shows as a note rather than an error.

## Adding a column

From a checkout:

1. Add the field to the `GridRow` struct in
   `datalib/backend/schema/src/grid_rows.rs`, with a `#[col(sql = "…")]`
   portable type and a doc comment saying what it means, and a setter
   on `GridRowBuilder` (`schema/src/grid_rows_builder.rs`).
2. Fill it where rows are built: the shared layers named under
   "Producer side", and the render crates that build rows themselves
   (notion, pdf, perseus, garmin; pdf writes the struct literally).
3. Update `unified_grid_and_search_index/src/dolt_repo.rs` — both the
   `SEARCH_ROW_COLUMNS` list and `search_row_from`, which name columns
   by `GridRowColumn` — and `SearchRow` in `unified_grid_and_search_index/src/search.rs`
   if the column should reach the API.
4. If it should be a grid column, add it to the `SearchRow` type in
   `datalib/ui/src/api.ts` and declare it in `columns()` in
   `datalib/backend/applets/src/unified_grid_and_search_index/columns.rs`, with its type
   from `datalib_columns`. The applet declares the columns and the grid
   draws them by type (`cards/typedColumns.ts`, over the renderers in
   `cards/cellRenderers.ts`); a width or a hover the type cannot know
   goes in `GridCard`'s `columnOverrides`. Map its id to the
   `grid_rows` column it sorts and filters by in `GridColumn::backing`
   (`unified_grid_and_search_index/src/grid_columns.rs`), and give that column `search`
   on its `#[col]` so Keep only, Exclude and dropping it on the search
   bar work; `every_filter_key_is_served_by_an_index` then asks for an
   index or a place in its `SCANS` list.
5. If it is a string, say whether a person reads it as a name or a
   label: clean it in `clean_row` (`etl/render/src/names.rs`), which
   strips its bidi controls as the document is stored, or add it to
   that file's `KEPT` list as an id, a path, a stamp or a body.
   `every_grid_row_string_is_cleaned_or_kept` fails until it is one or
   the other.
6. Re-bake the fixture: `bazelisk build //tests/fixtures:ingested_tng`.

On an existing root, a render store and the grid index change shape
only when the step that writes them runs. Each start of the loop asks
each step (`--prepare`) whether its store's recorded shape is this
build's; the app then offers to re-render them all, downloading
nothing, and until then a source's own sync re-renders that source
([the runner](../../datalib/backend/dag/README.md#upgrading-a-root)).
The render step sees its own DDL hash moved (`_store_schema`, one of
its params) and re-renders every document into the new shape, and the
grid index rebuilds itself from the stores.

## Adding a provider

From a checkout:

1. Add the provider's crates under `datalib/backend/etl/providers/`
   (`<p>`, `<p>_render`, `<p>_config`; `AGENTS.md` §"Ingest and render
   are separate crates" in the repo), a `Provider` variant
   (`schema/src/providers.rs`) and an `IdNamespace` variant
   (`datalib/backend/id/src/lib.rs`). The render crate emits `GridRow`s
   with the right `provider` / `kind` / `source_label` — through a
   shared layer where one fits — and hands each finished document to
   `ctx.emit_doc`.
2. Wire it into `datalib-step`: add it to the deps of
   `datalib/backend/datalib_step` and to the dispatch table in
   `datalib/backend/datalib_step/src/dispatch.rs`, then declare its
   ingest/render step pair in the config. `unified_grid_and_search_index`
   reads the stores its `inputs` name, which is normally the pattern
   `*/render_markdown`, so a new source's render is read with no edit
   there ([`config_model.md`](config_model.md) §"The fan-ins read exactly
   their `inputs`").
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
upstream_id)`, so re-rendering a record gives it the same uuid. A
document's own row carries no stamp. The recipe, which rows carry a
stamp, and the account each provider names are in
`docs/dev/entity_ids.md` in the repo.

### `kind` and `source_label`

`kind` is the display label for the Kind column ("LLM Thinking",
"GitHub PR", "Notion Comment Thread") and may be reworded freely; it is
not `upstream_entity_kind`, which the id depends on. `source_label` is
the plain product name ("Claude", "Slack").

### `is_document`

True on exactly one row per rendered document — the row
whose `uuid` is the document's `markdown_uuid` — and false on every
row inside it. Every row a document carries has a `markdown_uuid`, so
this is not "has a document"; it is "opening this row opens a whole
document rather than a place in one". A container's own row, below, has
none and is not a document. Each renderer says so through
`GridRowBuilder::is_document`, and a document with any number of them
other than one fails the render (`document_row` in
`etl/render/src/grid_index.rs`), so the flag is declared, never
inferred. A Browse of a source opens on these rows (`is:document`).
Several sources have more than one document kind: Claude's `Chat` and
`Project`, Notion's `Notion Page`, `Notion Comment Thread` and `Notion
Table of Contents`, LinkedIn's `Contact`, `LinkedIn Chat`, `LinkedIn
Post` and `LinkedIn Skills`.

### `created_at`, `modified_at` and `touched_at`

All three are the record's own stamps, kept as the source wrote them,
offset included; each gets a `_utc` twin and an offset column at index
time. `touched_at_utc` is what the grid
sorts on, newest first, and what `before:`/`after:` filter on; Touched
is the one date column the grid shows by default (a calendar's Browse
shows Created instead).

- **A document row:** `created_at` is the earliest moment in the
  document and `modified_at` the latest. Through `chat-common` that is
  the min and max over the items' stamps, reactions counting towards
  the max.
- **A row inside a document:** `created_at` is its own stamp (for a
  chat item with none, bumped a microsecond off its parent), and
  `modified_at` is the edit stamp where the source keeps one —
  **null** otherwise, never a copy of `created_at`: null means "not
  known to have changed since it was created".
- **`touched_at`** is when the record was last active: when it last
  changed at its source, or for an event, when it happens. The builder
  sets it to `modified_at`, else `created_at`, so a provider sets it
  only when the answer is neither. Calendar is the one that does: an
  event's `touched_at` is its start, though it was last edited before
  then, so `after:` last Monday finds last week's meetings. A meeting to
  come is ahead of now, and the grid opens at now rather than on it.
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

### `contact`, `email`, `phone`

**Null on every row but a contact's own**: an address-book card, a
LinkedIn connection, a Facebook friend, all written by
`contact-common`. `contact` is who the row is about, the person or the
group, and `email` and `phone` are the card's first address and number
as the source wrote them, a number with or without its country code.
The grid's Contact column, and the Subject cell on such a row, is the
contact's chip, drawn from the three, else from the contact's handles
in `contact_record_handles`, else by its own page ([`chips.md`](chips.md) §"In a grid"). A
card is its own subject, so `conversation_name` is the contact's name
too, and `channel` holds the address book or list it is filed in.
`author` stays null: nobody wrote a contact.

### `conversation_name` and `channel`: the subject, and where it is filed

`conversation_name` is the row's **subject**: the conversation,
thread, page, event or card the row belongs to, carried onto every row
in it, and on a document's own row its own title. The grid shows it as
the Subject column (`subject_ref`: on a contact's row the card's chip,
in a conversation named after its people a chip for each). `subject:`
(or `conversation:`) finds it by its whole name; `datalib_document_id:`
finds the conversation by its `conversation_uuid`, and is what the
cell's Keep only writes. `id:` is the row's `upstream_id`.

`channel` is where a row is filed when that is *not* its subject: a
card's address book, an event's calendar, a sensor's device, LinkedIn's
connections list. A chat leaves it null — chat-common writes the
conversation's name once, as its subject — so the two never repeat
each other. Where a row sits among places nested in places (a Slack
workspace and its channel) is its tree, below.

### `parent_uuid`, `sort_key`, `read_uuid`, `place_uuid`: where a row sits

Every row sits in a tree of **containers**: the chat, thread, channel,
page, calendar or label it lives in, each one row of the grid index's
`nodes` table, by its uuid. `nodes` is settled from what the documents
say: each carries a `data_nodes` row for every container it sits in, the
way it carries its contacts, so a channel has one there under every
thread. The document that holds a container's own grid row tells of it,
else the newest that names it does. Every reader reads `nodes`;
`data_nodes` is there for when the document that told of one goes, and
the next one tells.

`parent_uuid` is the container that holds the row. `sort_key` is how
it sorts among that container's children: its `created_at` in UTC
unless the source orders children without times, as a page orders its
blocks, never a position counted by us, so a row that arrives late
moves none of its siblings. A row with neither has `""`, which sorts
first; the column is never null, because a page of places is read
past a `(sort_key, uuid)` pair and a null in that comparison would put
the place on no page. A container that is also a row of its document (a
thread, a chat) takes that row's sort key unless its renderer gave it
one. The applet's `GET /places/{uuid}` lists the places in a place in
that order ([the route](../user/applets.md#places)), off `nodes_places`,
which only the grid index has (`nodes::TREE_INDEXES`). No index finds the
rows whose parent a container is: a search finds them by their place or
their conversation. A container is **read together** (what a
search groups by and the viewer opens: a thread, a chat, a page) or a
**place** (what a person browses: a channel, a mailbox, a calendar), or
both; `read_uuid` and `place_uuid` are the nearest container above the
row of each kind, so grouping by either is one column. A place lives in a
place, and in one only. Besides its parent, a document or a container
read together may be filed under others, an email thread under each of
its labels: those are `filings` rows, and a place has none.

The search's `at:<node>` (`unified_grid_and_search_index/src/at_node.rs`)
reads these: the rows whose `place_uuid` the node is, those in what is
filed under it, and its own document when it is one, off
`grid_rows_by_place` and `grid_rows_by_conversation` alone.

**Every container is a row.** Most are already a document's row: a
thread or a chat is the document that holds its messages, under the
same uuid. The rest (a Slack channel or workspace, an email label or
account, a project, a calendar, an address book) are written by the
`unified_grid_and_search_index` step from `nodes`, one row per
container: its `kind`, its name as `conversation_name`, its place in the
tree and its upstream id, and no author, time or text
(`etl/render/src/grid_index/nodes.rs`).
Such a row has no `markdown_uuid`, because no document owns it: it is
kept while any document carries its container and deleted with the
last. So a container's uuid opens a row like any other.

### `conversation_uuid`, `preview`, `content_hash`

`conversation_uuid` is the conversation's id on every row in it, the
conversation's own row included. A document that is part of a
conversation, as a sub-agent's transcript is of its session, carries
the session's.

A producer hands the builder the row's whole text (`.body(…)`), and the
builder keeps two things from it: `preview`, the first 240 characters
of it as plain text on one line (`PREVIEW_CHARS`), which is the grid's
Contents cell; and
`content_hash`, blake3 of the whole body, so a change past the preview
still changes the row. The body itself is not stored — the rendered
markdown holds it, and the search index of that markdown is how free
text finds it.

The body is markdown, and `datalib_schema::plain_text` turns it into
what a person reads: tags, images, link targets, heading and quote
marks, emphasis and code fences go, and a `<details>` block reads as
its summary unless it is all the body there is. Text inside a code
fence or a code span is kept as written, backslashes and `*` included,
since that is what the page shows. A search hit's snippet
goes through the same pass. So what a producer puts in the body is
what the row *says*, in the order that matters: a calendar event's
description comes before its guest list, a chat's document row leaves
out its asides, and an email's mailbox labels are not in it at all.
Changing what a producer puts there changes every row it wrote, so it
takes a `RENDER_VERSION` bump to reach a store already rendered.

### `source_id`

`source_id` is the id of the source a row is filed under: its group's
id, the directory under the data root, never the display name the
config may also give it. A renderer does not set it. The one writer
every document passes through (`grid_index::apply_page`, which
`apply_one` calls with a page of one) files each row
under the group whose render store holds its document, and a storage
row (provider `datalib`) under `datalib`
(`grid_rows::filed_under`). A document that names no source
(`RenderedMarkdown::source_id` empty) is refused, and its render fails
rather than filing rows under nothing. The `source:` filter compares it.

A row names its document by `markdown_uuid`. The document's markdown is
kept in its source's render store under that id, and nowhere else; the
store refuses a document holding a row of another document (`misfiled`
in `grid_index.rs`). A search hit names its document by that id and
finds the document's rows through `grid_rows_by_markdown`.

### `byte_size`, `item_count`

Two nullable measurements. What each one measures is decided per
`kind`, and this table is the list:

| provider.kind | `byte_size` | `item_count` |
|---|---|---|
| datalib.Source Size | bytes under `<name>/ingest` | files under it |
| datalib.Store | the `.doltlite_db` file's size | — |
| datalib.Table | — (see below) | rows in the table |
| pdf.document | — | 1 |
| markdown.Note | — | 1 |
| any chat-common conversation row | the sum of its items' `byte_size` | its messages: what a person or an assistant said |
| any chat-common message row | the body's UTF-8 length | 1; — for a tool call, a tool result or a system note |
| any chat-common reaction row | — | — |
| a sensor or Garmin page | — | its readings: rows of the raw store, or for Garmin every day of a metric with data, every weigh-in and every activity |
| garmin.Garmin Metrics, garmin.Garmin Daily Log | — | the days with a value on the page |
| garmin.Garmin Records | — | its entries |
| garmin.Garmin Activity | — | 1 |
| a GitHub PR, a GitLab MR, a Notion comment thread | — | itself and its comments; each comment row 1 |
| todoist.Todoist Project | — | its tasks, open and completed, and its comments; each task or comment row 1 |
| baby_connect.Child (a child's month) | — | its entries; each entry row 1 |
| a calendar event, a contact, a Notion page, a Perseus book or chapter | — | 1 |
| lightroom.Photo, lightroom.Video | — | 1 |
| notion.Notion Table of Contents | — | 0: a list of the source's pages adds nothing to its count |

On a `datalib.*` row, `byte_size` is bytes on disk **as of the last
render that rewrote the row** (see "Storage rows" below). On a
chat-common row it is the message body and nothing else: not the
attachments, whose sizes only some providers know, and not the raw
payload. So a conversation's `byte_size` is exactly the sum of its item
rows', and its `item_count` is how many of them were said: a tool call,
its result, a system note and a reaction are not messages, so their
rows carry no count.

**Every document row carries an `item_count`**; the render store
refuses one that does not. It is what the Manage screen's Items column
sums, through `markdowns.item_count` — a copy, like the document's
stamps — so a new renderer has to decide what its documents count.

Bytes on disk and a byte length of content are different measurements,
and one kind never mixes them: a producer that can only compute a
logical size for something that *has* an on-disk size leaves the column
NULL. `item_count` has no unit; what it counts is `kind`'s to say.
Adding a kind here means adding a row to this table.

### `diff_status`, `diff_changed_columns`

**NULL on every row a real source renders.** Set only by a diff group,
whose rows are two renders of the same source subtracted: `diff_status` is `added`, `removed`, `modified`
or `unchanged` (`datalib_schema::diff_status::DiffStatus`), and for a
modified row `diff_changed_columns` names the columns that differ,
sorted and `|`-joined. A non-NULL `diff_status` is the one thing that
says a row came from a diff tree; nothing else about the row changes —
`provider` is still the source's, so its icon and CSS apply. A
`removed` row is the earlier render's row as it was. The search key
`change:` reads `diff_status`; its negation keeps NULL, so
`-change:unchanged` over the whole grid keeps every real source's row,
and a diff group's Browse opens on `source:<group> -change:unchanged`
(`ui/src/config/browsePresets.ts`). How the rows are made:
[parse and render § "Rendering the delta itself"](data_architecture_parse_and_render.md#rendering-the-delta-itself).

### `image`, `latitude`, `longitude`

`image` names the picture a row shows, as a `datalib:` name
(`datalib:photo/<uuid>`), never an address: the UI asks which applet
serves the name's kind and draws the thumbnail from it, so moving the
bytes moves no row. The search grid's Picture column (column type
`image`) is hidden until a row in the results has one. `latitude` and
`longitude` are where the thing was, in degrees. All three are null on
every row but a Lightroom photo's.

### `h3`

The finest [H3](https://h3geo.org) cell holding `latitude` and
`longitude`, as an integer, which the builder sets from `.geo(…)`: a
render gives a row a location and nothing else. `place:<cell>` and
`near:<cell>` are ranges of this one indexed column, at any resolution
(`datalib-step help search`).

**A photo's document has no text.** Its render store keeps its rows and
no markdown: the `photos` applet draws the photo from the
`NormalizedPhoto` its document carries in `source_photos`
(`datalib_photo_schema`), which the index copies whole. `/chat/<uuid>` answers such a document with its
`image` and no text, and the document card hands it to the applet's
card. The search index holds nothing for it, which is why a photo's
caption is a `caption:` term.

**A document can be in the grid and not in the search index.** One its
renderer hands over with `RenderedMarkdown::searchable` false has its rows
and opens like any other, and is stored with `markdowns.searchable` false,
which the keyword index and the source's `embed` read. Use it for a document that only repeats what others say, so
a free-text search does not return it and nothing embeds it: Notion's
table of contents is the one. Its row is still found by its own
columns.

### `supplied_terms`

What a row answers to in search that its other columns do not say:
who an email went to, who a message mentions, which labels it is filed
under, who took part in a conversation (on the document's own row),
who a contact's card is about. A JSON array of `[kind, value]` pairs,
sorted and each once, so an unchanged row writes the same bytes; NULL
when there are none. A renderer sets it with
`GridRow::set_supplied_terms` and reads it back with
`GridRow::supplied_terms`.

**Nothing searches this column.** SQLite cannot index the elements of
a JSON array. The transaction that writes a row to the grid index
expands it into the index's search terms, one indexed row per term,
together with the terms the row's other columns give
(`search_terms_of`); a row written again unchanged is not touched. A
row inside a document has no term its document's own row holds for it:
the document's id and title are on that row once, and the rows inside
answer to them through it (`FROM_THE_DOCUMENT`). An id that is a uuid
is a term in one spelling, lowercase with dashes, however its source
wrote it.

The terms live on the row so that they go wherever the row goes: a row
that moves to another document takes its terms with it, in one write.

What may be a term:

- **A mention only where the source marks it up**: Slack's `<@U02>`, a
  GitHub `@login`, each an exact id. Never an address found in running
  text: quoted replies repeat every earlier message, and signatures add
  noise.
- **Every recipient, with no cap.** Dropping some would make the record
  lie. If the size of long reply-all threads ever matters, store each
  distinct recipient list once and point the emails at it.
- **Never a message's text, nor its preview.** Words in the body are the
  keyword index's.

## Storage rows: what a source weighs

Every source's render ends by measuring its own raw store and emitting
a handful of rows tagged `provider = "datalib"`, `source_label =
"Storage"`, kinds `Source Size`, `Store` and `Table`. That gives a
download-only source (`file_inventory`, `media`) a place in the grid at
all. `source:datalib` is "show me what everything weighs". The code,
with the reasons behind these rules in its header, is
`datalib/backend/datalib_step/src/introspect.rs`.

- **They are filed under `datalib`, not under the source they
  measure**, so "what claude weighs" never lands beside the Claude
  conversations. The report is a document in the measured source's
  render store, the one store its render may write; which source it
  describes is `markdowns.source_id`, the row's `conversation_name`
  (`<source_id> storage`) and its `upstream_id` (the measured path). A
  group configured with the literal id `datalib` would collide with
  this.
- **The grid holds the current value.** Each measurement is one row,
  keyed on `(source, kind, measured path)`, which a re-render
  overwrites. The series accumulates in `source_measurements`, a table
  in the same render store, keyed `(subject, measured_at_utc)`.
- **A `Table` row carries no byte size.** Doltlite shares chunks
  between tables and between commits, so no honest per-table number
  exists ([query plans](doltlite.md#query-plans-and-indexes)).
- **Scope is `<name>/ingest`.** The render's own output is Datalib's,
  and `system/disk_stats.sqlite` already tracks it per step.
- **What re-renders the report is a count, never a byte.** A doltlite
  store's size is not reproducible from identical inputs, and
  bookkeeping rewrites chunks with no row added, so byte sizes are
  reported but never compared (`introspect::counts_unchanged`), and
  Datalib's own tables (`sync_runs`, `sync_scope_state`, every
  `<table>_bookkeeping` sidecar) are left out of the report. Otherwise
  every run would rewrite it.

So read `byte_size` on a storage row as **how big the raw store was
the last time this source's contents changed**, not as how big it is
now. For bytes on their own cadence, `system/disk_stats.sqlite` keeps a
per-step series in plain SQLite.
