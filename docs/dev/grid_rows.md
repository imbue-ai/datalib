# `grid_rows` — the union table behind the grid

The AG Grid in `datalib/ui` shows one row per "displayable thing" in
the mirror: chat conversations, individual messages, content blocks
(tool_use / tool_result / thinking), Slack threads, Slack messages.
Rather than have the Rust backend dispatch per-provider — five queries
unioned in code, with five distinct row builders — every ingest writes a
denormalized projection into a single Dolt table, **`grid_rows`**, and
the backend reads it with one query.

## Why a union table

Three forces pushed us this direction:

1. **One source of truth for column semantics.** When the grid grows a
   new column (`channel`, `slack_link`, …), exactly one schema needs to
   move. Codegen propagates the change to Rust (both writer and reader
   sides) and TypeScript (consumer side). Drift has historically been
   a recurring bug source.
2. **One query path on the backend.** `datalib/backend/core/src/db.rs`
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
  (e.g. `when_ts_utc` / `when_offset`, derived from `when_ts`). Present in
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
each changed document's row set, and stamps the corresponding
`markdowns` row with the `row_set_hash` used to skip unchanged
re-renders next time.

## Consumer side: `datalib/backend/unified_index/src/dolt_repo.rs`

`DoltRepo::search` builds a `WHERE` clause from `ParsedQuery`
(account/project/before/after/free-text) plus a kind clause from
`q.resolved_type` (chat: vs message:), then issues a single SELECT
against `grid_rows` ordered by `when_ts` ASC with chat rows tie-breaking
ahead of their messages. The row mapper translates each row into a
`SearchRow` for the HTTP API.

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
4. If the grid should display it, add it to `default_columns()` in
   `datalib/backend/applets/src/unified_index/mod.rs` (the applet's wire
   contract; a test counts the entries) and to the `SearchRow` type in
   `datalib/ui/src/api.ts`.
5. Re-bake the fixture: `bazelisk build //tests/fixtures:ingested_tng`.

## Adding a provider

1. Land a new crate under `datalib/backend/etl/providers/<p>/`
   with a `render/grid_rows.rs` emitting `GridRow`s with the right
   `provider` / `kind` / `source_label` strings, and a renderer that
   hands each finished document to `ctx.emit_doc`.
2. Wire the new crate into `datalib-step`: add it to the deps of
   `datalib/backend/datalib_step` and to the dispatch table in
   `datalib/backend/datalib_step/src/dispatch.rs`, then declare its
   download/render step pair in the config. The grid_index step picks
   up its render store with no further wiring.
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

Minted by `datalib_id::entity_id` for ported providers; the others pass an
upstream id through directly.

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
| slack.thread | `uuidv5(SLACK_NS, 'slack:{team}:{channel}:{thread_ts}')` |
| slack.message | `uuidv5(SLACK_NS, 'slack:{team}:{channel}:{ts}')` |
| github.pr | `uuidv5(GITHUB_NS, 'github:{repo}:pr:{number}')` |
| github.issue_comment | `uuidv5(GITHUB_NS, 'github:{repo}:issue_comment:{id}')` |
| github.pr_review | `uuidv5(GITHUB_NS, 'github:{repo}:pr_review:{id}')` |
| github.pr_review_comment | `uuidv5(GITHUB_NS, 'github:{repo}:pr_review_comment:{id}')` |
| gitlab.mr | `uuidv5(GITLAB_NS, 'gitlab:{project}:mr:{iid}')` |
| gitlab.note | `uuidv5(GITLAB_NS, 'gitlab:{project}:note:{id}')` |
| notion.page | `page_id` (already a Notion UUID) |
| notion.heading | `uuidv5(NOTION_NS, 'notion:heading:{page_id}:{block_id}')` |
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

`source_label` is the plain product name: `Claude`, `ChatGPT`, `Slack`,
`GitHub`, `GitLab`, `Notion`.

### `when_ts`

| provider.kind | value |
|---|---|
| claude.chat | `IFNULL(created_at, updated_at)` |
| claude.message | `messages.created_at` |
| claude.block | `blocks.start_timestamp`, else `bump_micros(parent.created_at, block_index+1)` |
| chatgpt.chat | `IFNULL(create_time, update_time)` |
| chatgpt.message | `messages.create_time`, else `bump_micros(parent.create_time, msg_idx+1)` |
| slack.message | `messages.ts`, formatted ISO-8601 UTC |
| github.pr | `pull_request.updated_at`, else `created_at` |
| github.comment | `comment.created_at` |
| gitlab.mr | `merge_request.updated_at`, else `created_at` |
| gitlab.note | `note.created_at` |
| notion.page | `block.last_edited_time` (ms epoch → ISO-8601 UTC) |
| notion.heading | the parent page's `last_edited_time` |
| notion.thread | the first comment's `created_time` |
| notion.comment | `comment.created_time` |

### `author`

| provider.kind | value |
|---|---|
| claude.chat | `''` |
| claude.message.human | `account_uuid` |
| claude.message.assistant | `conversation.raw_json.model`, else `sender` |
| chatgpt.message.user | `account_id` |
| chatgpt.message.assistant | `model_slug`, else `role` |
| slack.message | `users.real_name`, else `users.name` |
| github | `comment.user.login`, else `pull_request.user.login` |
| gitlab | `note.author.username`, else `merge_request.author.username` |
| notion.page | the `notion_user.name` for `block.last_edited_by_id` |
| notion.heading | the `notion_user.name` for the parent page's `last_edited_by_id` |
| notion.thread / notion.comment | the `notion_user.name` for the comment's `created_by_id` |

### `account`, `project`, `channel`

| provider | account | project | channel |
|---|---|---|---|
| claude | `conversations.payload.creator.uuid` | the `projects.name` of the conversation's project (bare UUID when projects aren't mirrored) | — |
| chatgpt | `me.id` | — | — |
| slack | `workspaces.id` | — | `channels.name` |
| github | `self_identity.viewer.login` | `pull_request.base.repo.full_name` | — |
| gitlab | `self_identity.current_user.username` | `merge_request.references.full`, else `project_path` | — |
| notion | `notion_space.name` | — | — |
| whatsapp | — | — | `wa_chat.subject` for groups, JID label for 1:1 |
| signal | — | — | `recipients.display_name`, else phone number |

`org_uuid` / `org_name` are Claude-only, from
`conversations._source`.

### `conversation_name`, `conversation_uuid`, `text`

`conversation_uuid` is the row's own `uuid` for thread-level rows
(claude.chat, chatgpt.chat, slack.thread, github.pr, gitlab.mr, notion.page,
notion.thread) and the parent's for everything below them.

| provider.kind | conversation_name | text |
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

### `qmd_path`

`<source_name>/render_markdown/<renderer-specific tail>`, where `<source_name>`
is the config step's name. Verified against the TNG fixture:

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

Two nullable measurements, both NULL on most rows.

| provider.kind | `byte_size` | `item_count` |
|---|---|---|
| datalib.Source Size | bytes under `<name>/ingest` | files under it |
| datalib.Store | the `.doltlite_db` file's size | — |
| datalib.Table | — (see below) | rows in the table |
| pdf.document | — | pages in the document |

On a `datalib.*` row, `byte_size` is bytes on disk **as of the last
render that rewrote the row** — see "Storage rows" below for why that
is not "now". Everywhere else it is bytes on disk, and never a logical
sum of field lengths.
The two disagree, and a column that quietly mixes them is worse than one
that is absent — a producer that can only compute a logical size leaves
it NULL and says so in `text`.

`item_count` is deliberately unitless. What is being counted is `kind`'s
job to say: a Table counts rows, a Source Size counts files, a PDF
document counts pages.

## Storage rows: what a source weighs

Every source's render wave ends by measuring its own raw store and
emitting a handful of rows tagged `provider = "datalib"`, `source_label
= "Storage"`. That is what gives a download-only source — `fsindex`,
`media` — a place in the grid at all: they render no documents, so
without this they appear nowhere. `source:Storage` is "show me what
everything weighs"; `source_name:<name>` narrows to one source, since
the rows live under that source's `render_markdown/`.

The code is `datalib/backend/datalib_step/src/introspect.rs`, and three
of its decisions are worth knowing before changing it.

**The grid holds the current value; the history lives elsewhere.** Each
measurement is one row, keyed on `(source, kind, measured path)` and
nothing else, so a re-render overwrites it and `dolt_diff` over the
store reads as "these numbers moved". The series behind it accumulates
in `source_measurements`, a table in the same per-source
`indexed_markdown.doltlite_db` that `render_problems` lives in, keyed
`(subject, measured_at)`.

Putting the series in `grid_rows` instead was considered and rejected
for four reasons, each specific to that table: `when_ts` is the global
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
different number — and it is why `byte_size` is kept out of *both*
hashes that decide staleness: the storage report's own fingerprint and
`compute_row_set_hash`, the markdown cache key. Hashing it re-renders
documents nothing touched and churns every golden carrying a
`row_set_hash` on any backend change. `fixture_db_snapshot.rs` scrubs
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
`last_attempt_at` mutation rewrites chunks with no row added), and
`sync_runs` gains a row per run. So:

- byte sizes are **reported but not fingerprinted**; and
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
