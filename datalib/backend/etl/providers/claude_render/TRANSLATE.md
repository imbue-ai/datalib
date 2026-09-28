# Claude render

The `render_markdown` step of a `claude` group reads the raw store its
ingest wrote (`<data_root>/<group>/ingest/entities.doltlite_db`, filled
by either ingest method; see [`../claude/INGEST.md`](../claude/INGEST.md))
and hands each conversation and each Project to chat-common, which
writes `<data_root>/<group>/render_markdown/<chat_uuid>/all.md` and the
document's rows in `indexed_markdown.doltlite_db` beside it.

What is shared with every chat source lives elsewhere: the markdown
layout, the collapsed tool asides and `LAYOUT_VERSION` in
[`chat-common/README.md`](../../chat-common/README.md); how render finds
the conversations that moved, in
[`data_architecture_parse_and_render.md` §5](../../../../../docs/dev/data_architecture_parse_and_render.md#5-incrementality-and-deletion).
This file covers what Claude adds.

## Conversations

An API-fetched payload goes through
`normalize::normalize_to_export_shape` in the download crate on its way
out of the store; an export-ingested one is already that shape. Either
way one parser reads it (`src/render/parse.rs`).

Messages are ordered by `(created_at, message_uuid)`. Each message
becomes one item whose `kind` comes from its sender: `User Input` for
`human`, `LLM Response` for `assistant` (authored by the
conversation's `model`), `Tool Call` for anything else. Its body is the
message's `text` blocks, so search prose is not polluted by thinking
or tool traffic, plus each `attachments[]` entry's extracted text as a
quoted block. Downloadable `files[]` are materialized by chat-common
from the ingest's `claude_attachments` edges.

Every `thinking`, `tool_use` and `tool_result` block is an item of its
own, placed just before its message's answer:

| block | `kind` | aside | body |
|---|---|---|---|
| `thinking` | `LLM Thinking` | no | a `<details>` "Thinking" with the thought quoted |
| `tool_use` | `Tool Call` | yes | the tool's name, and its `input` as JSON with sorted keys |
| `tool_result` | `Tool Call` | yes | the tool's name, `(error)` when `is_error`, and its content |

A block's id is keyed on the upstream `tool_use` id where it has one,
else on `(message_uuid, block_index)` (`src/render/ids.rs`).

## Projects

A Project renders as a page of its own through the same renderer, with
the grid `kind` `Project`: its description, its custom instructions,
and one `Project Knowledge` section per knowledge document. A document
is cut at the render step's `max_project_doc_bytes` (default 128 KiB)
with a visible marker; the raw store keeps all of it. A conversation's
`project` grid column carries the project's name.

## Ids and links

Ids are minted in `src/render/ids.rs` under `IdNamespace::Claude`,
scoped to the group, never Anthropic's UUIDs passed through
(`docs/dev/entity_ids.md`). The document keeps the upstream UUID as
`external_id` and links back to `https://claude.ai/chat/<uuid>` or
`https://claude.ai/project/<uuid>`. The account is the account's email.

Bump [`RENDER_VERSION`](src/render/render.rs) when what this crate hands
chat-common changes; the render step then re-renders every document.

## Tests

The renderer is pinned by insta snapshots over the TNG fixture at
`../claude/tests/fixtures/claude_export/`, in the `claude_render` module
of `//datalib/backend/etl/providers/claude:claude_tests`. Update them
with `bazelisk run //datalib/backend/etl/providers/claude:claude_tests.update`.
