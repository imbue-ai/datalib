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
`etl/src/grid_index.rs`) and from the `dump.sql` portable-DDL emitter.

## Producer side: per-provider `render/grid_rows.rs`

Each provider crate under `datalib/backend/etl/providers/<p>/`
writes its `GridRow`s into that source's own render store,
`<root>/<stanza>/rendered_md/indexed_markdown.doltlite_db`. The
grid_index step (`datalib-step grid_index`; `build_grid_index` in
`datalib/backend/etl/src/grid_index.rs`) stacks those stores into the
unified index: it asks each one `dolt_diff` between the commit the
index last consumed (`source_cursors`) and that store's HEAD, applies
each changed document's row set, and stamps the corresponding
`markdowns` row with the `row_set_hash` used to skip unchanged
re-renders next time.

## Consumer side: `datalib/backend/core/src/dolt_repo.rs`

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
3. Update `dolt_repo.rs`'s `SELECT`, the destructured row, and
   `SearchRow` in `search.rs` if the column should reach the API.
4. Add it to the column manifest in `datalib/backend/http/src/lib.rs`
   if the grid should display it.
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

Table names below are the **raw-store** tables, which are unprefixed:
each provider writes its own `<name>/raw/entities.doltlite_db`, so there
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

`<source_name>/rendered_md/<renderer-specific tail>`, where `<source_name>`
is the config step's name. Verified against the TNG fixture:

```text
claude   claude-api/rendered_md/{conversation_uuid}/all.md
chatgpt  chatgpt-api/rendered_md/{conversation_id}/all.md
slack    slack/rendered_md/{thread_uuid}/all.md
beeper   beeper/rendered_md/{network}/{chat_uuid}/{YYYY-MM}.md
github   github/rendered_md/{owner}/{repo}/pr-{number}/index.md
gitlab   gitlab/rendered_md/{group}/{project}/mr-{iid}/index.md
notion   notion/rendered_md/pages/{page_uuid}/index.md
pdf      tng_pdfs/rendered_md/docs/{blake3}.md
```

For a given `markdown_uuid`, `grid_rows.qmd_path` must be byte-equal to
that markdown's `markdowns.md_path` — `GridIndex` keys rows by this path to
resolve qmd search hits, and a row whose path doesn't match what qmd
reports is silently dropped from free-text results.
`//tests/fixtures:ingested_tng_test` asserts it across providers.
