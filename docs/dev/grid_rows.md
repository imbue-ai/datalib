# `grid_rows` — the union table behind the grid

The grid in `datalib/ui` shows one row per "displayable thing" in
the mirror: chat conversations, individual messages, content blocks
(tool_use / tool_result / thinking), Slack threads, Slack messages.
Rather than have the Rust backend dispatch per-provider — five queries
unioned in code, with five distinct row builders — every ingest writes a
denormalized projection into a single Dolt table, **`grid_rows`**, and
the backend reads it with one query.

The index holds one more table the grid does not read: `problems`,
every source's render-store `problems` copied in whole by `grid_index`
and served by the applet at `/problems` — see
[`plans/problem_visibility.md`](plans/problem_visibility.md).

## Why a union table

Three forces pushed us this direction:

1. **One source of truth for column semantics.** When the grid grows a
   new column (`channel`, `source_url`, …), exactly one schema needs to
   move. Codegen propagates the change to Rust (both writer and reader
   sides) and TypeScript (consumer side). Drift has historically been
   a recurring bug source.
2. **One query path on the backend.** `datalib/backend/unified_index/src/dolt_repo.rs`
   is now a single `SELECT … FROM grid_rows WHERE …` plus a row mapper.
   Adding a provider doesn't add a `push_*` function; it adds rows to the
   table at ingest time. The query/filter/sort logic stays put.
3. **No "discover at query time" joins.** Per-message rows already carry
   the parent conversation's name, account, project — so the grid renders
   straight off the projection without join cost.

## Source of truth

The hand-written `GridRow` struct in
`datalib/backend/schema/src/grid_rows.rs` defines the row shape — it
is the single source of truth, with no codegen step. Each field carries:

- `#[col(sql = "…")]` — portable DDL type (the SQL subset shared by Dolt
  and MySQL). Nullability is inferred from `Option<T>`.
- `#[derived(name = "…", sql = "…")]` — a column computed at grid-index time
  (e.g. `created_at_utc` / `created_offset`, derived from `created_at`). Present in
  the DDL but absent from the struct.
- doc comment — one or two lines saying what the column *means*. How each
  provider fills it in is in [Per-provider mappings](#per-provider-mappings)
  below; the authority is always the provider's own `render/grid_rows.rs`.

`#[derive(PortableTable)]` (in `datalib/backend/etl/macros`) produces
from the struct the `DDL`, `COLUMNS`, and `TABLES` module consts. The
`DDL` constant is used at grid-index time (`init_schema` in
`etl/render/src/grid_index.rs`) and from the `dump.sql` portable-DDL emitter.

## Producer side: per-provider `render/grid_rows.rs`

Each provider crate under `datalib/backend/etl/providers/<p>/`
writes its `GridRow`s into that source's own render store,
`<root>/<stanza>/render_markdown/indexed_markdown.doltlite_db`. The
grid_index step (the `grid_index` function of `datalib-step`; `build_grid_index` in
`datalib/backend/etl/render/src/grid_index.rs`) stacks those stores into the
unified index: it asks each one `dolt_diff` between the commit the
index last consumed (`source_cursors`) and that store's HEAD, applies
each changed document's row set, and copies the corresponding
`markdowns` row across.

## Consumer side: `datalib/backend/unified_index/src/dolt_repo.rs`

`DoltRepo::search` builds a `WHERE` clause from `ParsedQuery`'s
structured terms (account/project/before/after/…) plus `is_document = 1`
or `= 0` when the query said `is:document` or `-is:document`, then
issues a single SELECT against `grid_rows`, newest first: by
`touched_at` descending, a document row ahead of the rows inside it at
the same moment. The row mapper translates each row into a `SearchRow`
for the HTTP API, with `preview` as its Contents cell.

Every read happens inside one read transaction on a read-only
connection (`DoltRepo::pinned`), so a request sees one commit and the
plain table's indexes serve it. `grid_rows` carries one index for the
newest-first order and one per key the search bar filters on, each
`(key, touched_at_utc, is_document, uuid)`; a key without one walks
the whole table in order. They are declared on the struct
(`#[portable_table(index = …)]`) and created only in the unified
index, not in the render stores that also hold a `grid_rows`.
`every_filter_key_is_served_by_an_index` fails when a key has none.

Free text never reaches SQL: the applet sends it to qmd, maps the hits
to rows by `qmd_path` (below), fetches them with `search_by_uuids`, and
shows each hit's own matched lines as its Contents cell. With no qmd
index, a free-text search answers with an error, not a weaker search.

## Adding a column

1. Add the field to the `GridRow` struct in
   `datalib/backend/schema/src/grid_rows.rs`, with a `#[col(sql = "…")]`
   portable type and a doc comment carrying the per-provider mapping so
   future-you knows where the value comes from.
2. Add the column to each per-provider `render/grid_rows.rs`
   `GridRow` builder.
3. Update `unified_index/src/dolt_repo.rs` — both the
   `SEARCH_ROW_COLUMNS` constant and `search_row_from` — and `SearchRow`
   in `unified_index/src/search.rs` if the column should reach the API.
4. If it should be a grid column, add it to the `SearchRow` type in
   `datalib/ui/src/api.ts` and declare it in `columns()` in
   `datalib/backend/applets/src/unified_index/columns.rs`, with its type
   from `datalib_columns`. The applet declares the columns and the grid
   draws them by type (`cards/typedColumns.ts`, over the renderers in
   `cards/cellRenderers.ts`); a width or a hover the type cannot know
   goes in `GridCard`'s `columnOverrides`.
5. Re-bake the fixture: `bazelisk build //tests/fixtures:ingested_tng`.

Nothing to bump for an existing root: the render store's DDL hash is
one of the render params (`_store_schema`), so a new column re-renders
every source on the next run rather than sitting `NULL` on every row
rendered before it, and the grid index rebuilds itself on any drift.

## Adding a provider

1. Land a new crate under `datalib/backend/etl/providers/<p>/`
   with a `render/grid_rows.rs` emitting `GridRow`s with the right
   `provider` / `kind` / `source_label` strings, and a renderer that
   hands each finished document to `ctx.emit_doc`.
2. Wire the new crate into `datalib-step`: add it to the deps of
   `datalib/backend/datalib_step` and to the dispatch table in
   `datalib/backend/datalib_step/src/dispatch.rs`, then declare its
   ingest/render step pair in the config and name the render step in
   the two fan-ins' `inputs` (the wizard does this for a source it
   adds); `grid_index` and `qmd_index` read exactly the stores their
   inputs name.
3. Add the source label to the consuming bits as needed (icon
   resolution, etc.) — but the query path itself does not change.

## Why this isn't a materialized view

We considered Dolt-side triggers / views. The mapping logic isn't
always pure SQL — timestamps get bumped to synthesize per-block
ordering, JSON fields get parsed out of raw payloads — so a generated
table built next to the rest of the translator code keeps the mapping
legible and avoids depending on Dolt-specific features.

## Per-provider mappings

How each provider derives each column. This is a reading aid, not a
contract — when it disagrees with a provider's `render/grid_rows.rs`, the
code is right and this table is stale.

### `uuid`

Minted by `datalib_id::entity_id` for every provider: the record's
`created_at` in the leading bits where it has one of its own, then a
hash of `(provider, source_id, upstream_account, upstream_entity_kind,
upstream_id)`. The recipe, the accounts and which rows carry a stamp are in
[`entity_ids.md`](entity_ids.md); the natural keys below are what
`upstream_id` holds.

The authoritative list of tables and columns is the `schema_inventory`
golden at
`datalib/backend/schema_inventory/tests/snapshots/inventory__schema_inventory.snap`,
generated from the DDL this build actually declares. Check the names
below against it; that file cannot go stale, and this one can.

Table names below are the **raw-store** tables, which are unprefixed:
each provider writes its own `<name>/ingest/entities.doltlite_db`, so there
is no `claude_`/`slack_` prefix to disambiguate. Only the CAS edge
tables carry one (`claude_attachments`, `slack_attachments`), because
they sit beside the shared blob store.

| provider.kind | value |
|---|---|
| claude.chat | `conversations.id` |
| claude.message | `conversations.payload.chat_messages[*].uuid` |
| claude.block | `{message_uuid}:{block_index}` |
| chatgpt.chat | `conversations.id` |
| chatgpt.message | `conversations.payload.mapping[*]` |
| slack.thread | `{channel}#{thread_ts}`, with `team_id` in `upstream_account` |
| slack.message | `{channel}#{ts}`, with `team_id` in `upstream_account` |
| github.pr | `{repo}#{number}` |
| github.issue_comment, pr_review, pr_review_comment | `{repo}#{id}`, told apart by `upstream_entity_kind` |
| gitlab.mr | `{project}#{iid}` |
| gitlab.note | `{project}#{id}` |
| notion.page | `page_id` (a Notion UUID) |
| notion.thread | `discussion_id` |
| notion.comment | `comment_id` |

### `kind` — the display label

| provider.kind | label |
|---|---|
| claude.chat, chatgpt.chat | `Chat` |
| claude.message.human, chatgpt.message.user | `User Input` |
| claude.message.assistant | `LLM Response` |
| claude.block.thinking | `LLM Thinking` |
| claude.block.tool_* | `Tool Call` |
| chatgpt.message.assistant.thoughts / reasoning_recap | `LLM Thinking` |
| chatgpt.message.assistant.* | `LLM Response` |
| chatgpt.message.system / other | `Tool Call` |
| slack.thread / slack.message | `Slack Thread` / `Slack Message` |
| github.pr | `GitHub PR` |
| github.issue_comment | `GitHub PR Comment` |
| github.pr_review | `GitHub Review` |
| github.pr_review_comment | `GitHub Review Comment` |
| gitlab.mr | `GitLab MR` |
| gitlab.note | `GitLab Discussion Note` |
| notion.page | `Notion Page` (`Notion Database` for a collection_view_page) |
| notion.heading.h1/h2/h3 | `Notion Heading 1` / `2` / `3` |
| notion.thread / notion.comment | `Notion Comment Thread` / `Notion Comment` |
| facebook.post / its one item | `Facebook Post` / `Facebook Post Message` |
| facebook.album / description / photo | `Facebook Album` / `Facebook Album Message` / `Facebook Photo` |
| facebook.comments / comment | `Facebook Comments` / `Facebook Comment` (one chat per source, a document per month) |
| facebook.reactions / reaction | `Facebook Reactions` / `Facebook Reaction` (likewise) |
| facebook.friend | `Contact` |

`source_label` is the plain product name: `Claude`, `ChatGPT`, `Slack`,
`GitHub`, `GitLab`, `Notion`.

### `is_document`

True on exactly one row per rendered markdown document — the row
whose `uuid` is the document's `markdown_uuid` — and false on every
row inside it. Every row carries a `markdown_uuid`, so this is not
"has a document"; it is "opening this row opens a whole document
rather than a place in one". Each renderer says so through
`GridRowBuilder::is_document`, and the render store refuses a document
with any number of them other than one (`document_row` in
`etl/render/src/grid_index.rs`), so the flag is declared, never
inferred. A Browse of a source opens on these rows (`is:document`).
Several sources have more than one document kind: Claude's `Chat` and
`Project`, Notion's `Notion Page` and `Notion Comment Thread`,
LinkedIn's `Contact` and `LinkedIn Chat`.

### `created_at`, `modified_at` and `touched_at`

All three are the record's own stamps, kept as the source wrote them
(see the timestamp convention in AGENTS.md); each gets a `_utc` twin
and an offset column at index time. `touched_at_utc` is what the grid
sorts on, newest first; `created_at_utc` is what `before:`/`after:`
filter on.

`touched_at` is when the record last changed at its source. The
builder sets it to `modified_at`, else `created_at`, so a provider sets
it only when the record's last change is neither. Calendar is the one
that does: an event's `created_at` is when it happens, often years
ahead, so its `touched_at` is its edit stamp (Google's `updated`, the
feed's `LAST-MODIFIED`, else `DTSTAMP`), else when it was added. The rule for a document
row is the same everywhere: `created_at` is the earliest moment in
the document and `modified_at` the latest. For a row inside a
document, `created_at` is its own stamp and `modified_at` is the edit
stamp where the source keeps one — **null** otherwise, never a copy
of `created_at`: null means "not known to have changed since it was
created". `markdowns.created_at` / `modified_at` are copies of the
document row's, taken by the render store when it writes the
document.

| provider.kind | `created_at` | `modified_at` |
|---|---|---|
| every chat provider through `chat-common` (chat row) | `min(item.date_ms)` over the bucket | `max` over the items and their reactions |
| chat-common message / reaction | the item's `date_ms`, else `bump_micros` off the parent | null |
| claude.project | through chat-common: its sections are anchored to `projects.created_at` (else `updated_at`), so the min/max rule above applies | likewise, the latest section or knowledge doc |
| github.pr / gitlab.mr | `created_at` | `updated_at` |
| github.comment / gitlab.note | `created_at` | `updated_at` when it differs from `created_at`, else null |
| notion.page | `created_time` | `last_edited_time` |
| notion.thread | the first comment's `created_time` | the latest comment's `last_edited_time` (or `created_time`) |
| notion.comment | `created_time` | `last_edited_time` when it differs, else null |
| pdf.document | PDF `CreationDate`, else `ModDate` (the file existed by then) | `ModDate` |
| pdf.page | the document's `created_at` | null |
| calendar (event, series, changed occurrence) | when it happens: the start (a series' first occurrence), an all-day date as midnight UTC | null — an event is edited before it happens, which would break created ≤ modified; the edit stamp is on the page |
| contacts (vCard) | null — a person has no creation event | `REV:` |
| linkedin.contact | "Connected On", as midnight UTC | null |
| yolink / airvisual timeseries, garmin weight | the first sample | the last sample |
| datalib storage rows | the run's `--now` | the same instant |

### `author`

| provider.kind | value |
|---|---|
| claude.chat | `''` |
| claude.project | `projects.payload.creator.full_name` — who made the project, which in a Team workspace is often not the account that downloaded it |
| claude.message.human | the capitalized `chat_messages[].sender` ("Human") |
| claude.message.assistant | `conversation.raw_json.model`, else `sender` |
| chatgpt.message.user | `User` |
| chatgpt.message.assistant | `model_slug`, else `Assistant` |
| slack.message | `users.real_name`, else `users.name` |
| github | `comment.user.login`, else `pull_request.user.login` |
| gitlab | `note.author.username`, else `merge_request.author.username` |
| notion.page | the `notion_user.name` for `block.last_edited_by_id` |
| notion.heading | the `notion_user.name` for the parent page's `last_edited_by_id` |
| notion.thread / notion.comment | the `notion_user.name` for the comment's `created_by_id` |

### `account`, `project`, `channel`

`account` says whose mirror the row came from, resolved the same way
everywhere (`datalib_etl_chat_common::account_label`): the login's
email where the raw store has one, else its name, else the provider's
own id — so grouping by Account groups one person's data across
sources, and a raw id in the column means "this login has no row to
resolve against". A source with no login at all (a PDF folder, a
`.vcf` file, YoLink) leaves it null; the source's id is on
`source_id`, not here.

| provider | account | project | channel |
|---|---|---|---|
| claude | the `users` row's `email_address` (else `full_name`, else the bare UUID) for `conversations.payload.account.uuid`; a project page carries the downloading account, not its creator | the `projects.name` of the conversation's project (bare UUID when projects aren't mirrored) | — |
| chatgpt | the `me` row's `email` (else `name`, else `me.id`) | — | — |
| slack | the `users` row for `workspaces.self_user_id`: `profile.email`, else real name, else handle, else the bare `U…` id | — | `channels.name` |
| email | the `accounts` row: `emailAddress` (mbox), `email` (Gmail), else JMAP's `name` — which RFC 8620 defines as the owner's address — else the account id | — | — |
| linkedin | the `Primary` row of `email_addresses` (else the first, else `profile`'s first + last name); every row of the export, connections included | — | `Connections` for a contact |
| facebook | `profile_v2.emails.emails[0]` (else `profile_v2.name.full_name`) from `…profile_information`; every row of the export, friends included | — | `Friends` for a contact |
| calendar | the `accounts` row's `login`: the CalDAV principal's address, the Google account's primary calendar id; null for `.ics` files | — | the calendar's name |
| github | — | `pull_request.base.repo.full_name` | — |
| gitlab | — | `merge_request.references.full`, else `project_path` | — |
| notion | — | — | — |
| beeper | `rooms.account_id`, Beeper's bridge-account id (`local-signal_ba_…`) — still opaque | — | — |
| whatsapp | — | — | `chat.subject` for groups; for 1:1, `lid_display_name`, else the phone behind `jid_map`, else the JID label (msgstore's own tables, mirrored) |
| signal | — | — | `recipients.display_name`, else phone number |
| apple_messages | — | — | `chat.display_name`, else the chat's handles joined by `, `, else `chat_identifier` (`chat.db`'s own tables, mirrored; contact names are not in it) |

`org_uuid` / `org_name` are the organization a login lives inside:
Claude's Anthropic org (from `conversations._source`) and Slack's
workspace (`workspaces.id` / `team_name`). Null elsewhere. GitHub,
GitLab and Notion all mirror a self-identity row and could fill
`account` from it; their render diff deliberately does not fan out on
that table, so that is a small design change rather than a one-liner.

### `conversation_name`, `conversation_uuid`, `preview`, `content_hash`

`conversation_uuid` is the row's own `uuid` for thread-level rows
(claude.chat, chatgpt.chat, slack.thread, github.pr, gitlab.mr, notion.page,
notion.thread) and the parent's for everything below them.

A producer hands the builder the row's whole text (`.body(…)`), and the
builder keeps two things from it: `preview`, the first 240 characters on
one line, which is the grid's Contents cell; and `content_hash`, blake3
of the whole body, so a change past the preview still changes the row.
The body itself is not stored — the rendered markdown holds it, and
qmd's index of that markdown is how free text finds it. The table says
what each producer passes as the body.

| provider.kind | conversation_name | body |
|---|---|---|
| claude.chat | `conversations.name` | `summary`, else `name` |
| claude.message | (parent's) | `messages.text` |
| claude.block | (parent's) | `blocks.text`, else `raw_json.thinking`, else `type` |
| chatgpt.chat | `conversations.title` | `title` |
| chatgpt.message | (parent's) | `messages.text` |
| slack.thread | `channel_name` + root snippet | root message text |
| slack.message | (parent's) | `messages.text`, mentions and emoji rendered |
| github.pr | `pull_request.title` | `title` + `body` |
| github.comment | (the PR's title) | `comment.body` |
| gitlab.mr | `merge_request.title` | `title` + `description` |
| gitlab.note | (the MR's title) | `note.body` |
| notion.page | `block.properties.title` | title + recursive plain text of all child blocks |
| notion.heading | (the page's title) | the heading's plain text |
| notion.thread | (the page's title) | every comment in the discussion, concatenated |
| notion.comment | (the page's title) | `comment.text` as plain text |

### `source_url`, `git_sha`

| provider.kind | source_url | git_sha |
|---|---|---|
| github.pr | `pull_request.html_url` | `pull_request.head.sha` |
| github.comment | `comment.html_url` | — |
| github.pr_review | — | `review.commit_id` |
| github.pr_review_comment | — | `comment.commit_id`, else `original_commit_id` |
| gitlab.mr | `merge_request.web_url` | `merge_request.sha` |
| gitlab.note | `merge_request.web_url#note_{id}` | `note.position.head_sha` for a diff note |

### `upstream_id`

| provider.kind | value |
|---|---|
| github.pr | `pull_request.number` |
| github.issue_comment / pr_review / pr_review_comment | the id |
| gitlab.mr | `merge_request.iid` |
| gitlab.note | `note.id` |
| pdf.document / pdf.page | `blake3` / `{blake3}#{page_number}` |
| email.thread | `thread_id` |
| perseus | the locator path (`1`, `1.2`, `1.2.3`) |

### `qmd_path` and `source_id`

`source_id` is derived from `qmd_path` at index time: its first
segment, except a storage row (provider `datalib`), which sits under the
source it measures and is filed under `datalib`
(`GridRow::derived_source_id`). The `source_id:` filter compares it.

`<source_id>/render_markdown/<renderer-specific tail>`, where `<source_id>`
is the group's id — its directory under the data root, never the
display name the config may also give it. Verified against the TNG
fixture:

```text
claude   claude-api/render_markdown/{conversation_uuid}/all.md
chatgpt  chatgpt-api/render_markdown/{conversation_id}/all.md
slack    slack/render_markdown/{thread_uuid}/all.md
beeper   beeper/render_markdown/{network}/{chat_uuid}/{YYYY-MM}.md
github   github/render_markdown/{owner}/{repo}/pr-{number}/index.md
gitlab   gitlab/render_markdown/{group}/{project}/mr-{iid}/index.md
notion   notion/render_markdown/pages/{page_uuid}/index.md
pdf      tng_pdfs/render_markdown/docs/{blake3}.md
```

For a given `markdown_uuid`, `grid_rows.qmd_path` must be byte-equal to
that markdown's `markdowns.md_path` — `GridIndex` keys rows by this path to
resolve qmd search hits, and a row whose path doesn't match what qmd
reports is silently dropped from free-text results.
`//tests/fixtures:ingested_tng_test` asserts it across providers.

### `byte_size`, `item_count`

Two nullable measurements. What each one measures is decided per
`kind`, and this table is the list:

| provider.kind | `byte_size` | `item_count` |
|---|---|---|
| datalib.Source Size | bytes under `<name>/ingest` | files under it |
| datalib.Store | the `.doltlite_db` file's size | — |
| datalib.Table | — (see below) | rows in the table |
| pdf.document | — | pages in the document |
| any chat-common conversation row | the sum of its messages' `byte_size` | messages in the document |
| any chat-common message row | the body's UTF-8 length | 1 |
| any chat-common reaction row | — | — |

On a `datalib.*` row, `byte_size` is bytes on disk **as of the last
render that rewrote the row** — see "Storage rows" below for why that
is not "now". On a chat-common row it is the message body — the same
string it passes as the body — and nothing else: not the attachments,
whose sizes only some providers know, and not the raw payload, which
the renderer never sees. So a conversation's `byte_size` is exactly the
sum of its message rows', and its `item_count` is exactly how many of
them there are. Reactions have their own rows but are not messages, so
they carry neither.

Bytes on disk and a byte length of content are different measurements,
and one kind must never mix them: a producer that measures a file
reports the file, and a producer that can only compute a logical size
for something that *has* an on-disk size leaves the column NULL. Adding
a kind here means adding a row to this table.

`item_count` is deliberately unitless. What is being counted is `kind`'s
job to say: a Table counts rows, a Source Size counts files, a PDF
document counts pages, a conversation counts messages.

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
= "Storage"`. That is what gives a download-only source — `fsindex`,
`media` — a place in the grid at all: they render no documents, so
without this they appear nowhere. `source:Storage` is "show me what
everything weighs".

**They are filed under `datalib`, not under the source they measure.**
The report's markdown does sit in the measured source's
`render_markdown/`, because that is the one tree the render step is
allowed to write, and a source id is normally just the first segment
of `qmd_path`. Reading it that way here would put "what claude weighs"
in the same bucket as the Claude conversations — which is precisely
what the `provider` tag already refuses to do. So the derivation asks
`provider` first: a `datalib` row is datalib's, whatever directory it
came out of. Two halves, and they have to agree:

- `source_id_for` in `unified_index/src/dolt_repo.rs` decides what the
  grid's Source column shows (`Datalib`, spelled out by the UI);
- the `Field::SourceId` arm of `build_where` in
  `unified_index/src/db.rs` decides what `source_id:` matches —
  `source_id:datalib` selects on the provider tag, and every other id
  excludes the datalib rows despite the path prefix.

Which source a measurement describes is still on the row: `account` is
the source name and `conversation_name` is `<name> storage`. One
consequence worth knowing: a group configured with the literal id
`datalib` would collide with this, and its own rows would become
unfilterable by name.

The code is `datalib/backend/datalib_step/src/introspect.rs`, and three
of its decisions are worth knowing before changing it.

**The grid holds the current value; the history lives elsewhere.** Each
measurement is one row, keyed on `(source, kind, measured path)` and
nothing else, so a re-render overwrites it and `dolt_diff` over the
store reads as "these numbers moved". The series behind it accumulates
in `source_measurements`, a table in the same per-source
`indexed_markdown.doltlite_db` that `problems` lives in, keyed
`(subject, measured_at_utc)`.

Putting the series in `grid_rows` instead was considered and rejected
for four reasons, each specific to that table: `created_at` is the global
sort key, so every run would bury the user's real data under a few
hundred fresh measurement rows; `grid_rows.uuid` is contracted to be
deterministic from the entity, and a series row's id must carry a
timestamp; the query path has no notion of "latest only", so search and
filters would return N historical copies of every file; and pruning the
grid's hot table is a bad trade for a sparkline.

**A `Table` row carries no byte size.** doltlite is a content-addressed
chunk store with no page layout — `dbstat` refuses outright, with
"content-addressed chunk store has no page layout" — and chunks are
shared between tables and between commits, so no honest per-table number
exists. Row counts are exact and cheap; file sizes are exact and free.
Those are what this emits. The internals to do better are in the
amalgamation (`doltlite_chunk_walk.c` enumerates a catalog's per-table
prolly roots, and `ChunkIndexEntry` carries each chunk's size), but none
of it is exposed to SQL or declared in the public header.

**A doltlite store's size is not reproducible.** Rebuilding the TNG
fixture from byte-identical inputs moves six of its sixteen sources by
1-22 bytes, in a different direction each time. That is a stronger
property than "changes on every fetch" — it is the same input giving a
different number — and it is why `byte_size` is kept out of the
comparison that decides whether the storage report is rewritten
(`introspect::counts_unchanged`). Comparing it would rewrite the
report on every run. `fixture_db_snapshot.rs` scrubs
the byte figure out of the text it digests for the same reason.

This is the download side's *volatile field* idea arriving somewhere
else. The mechanism does not apply — `split_volatile` operates on JSONB
wire payloads and nothing here writes one — but the shape is identical,
with one difference worth naming. For an unordered bag the rule is
"sort; don't declare it volatile", because the contents are signal.
Here the bytes are signal too — a user wants to see how big a source
is — so we neither sort nor drop them. We **report but don't hash**.

**Scope is `<name>/ingest`, not the whole tree.** `<name>/render_markdown` is
datalib's own output, `system/usage.doltlite_db` already tracks it per
step, and measuring it from inside the thing that writes it is a
ratchet: every run finds a bigger tree, writes a bigger number, and
commits — growing the store it just measured, forever, on a pipeline
where nothing upstream changed.

**What re-renders the report is a count, never a byte.** This is the
same hazard one level down, and it is the one that actually shipped
broken: `ingested_tng_test` asserts that a second run over unchanged
data leaves `grid_index` with nothing to read, and the first version of
this failed it. Two numbers move without the data moving — a doltlite
store grows on any run that touches it (a bookkeeping
`last_attempt_at_utc` mutation rewrites chunks with no row added), and
`sync_runs` gains a row per run. So:

- byte sizes are **reported but never compared**; and
- `sync_runs`, `sync_scope_state`, `sync_scope_config` and every
  `<table>_bookkeeping` sidecar are left out of the report entirely.
  They are not the source's data — `doltlite_raw`'s own words for the
  first two are "audit log and resume cursor, not content" — and a
  sidecar holds one row per row of the table it shadows, so counting it
  doubles every number for nothing.

So read `byte_size` on a storage row as **how big the raw store was
the last time this source's contents changed**, not as how big it is
now. The two only coincide on a source that just changed. On one that
has gone quiet the number goes quiet with it, no matter how many times
the pipeline runs afterwards — which is the honest thing for a
content-addressed row to say, since that is the last moment the row was
written. For bytes on their own cadence, `system/usage.doltlite_db`
keeps a per-step series and commits nothing, which is exactly what lets
it sample freely.
