# ChatGPT render

The `render_markdown` step of a `chatgpt` group reads the raw store its
ingest wrote (`<data_root>/<group>/ingest/entities.doltlite_db`, described
in [`../chatgpt/INGEST.md`](../chatgpt/INGEST.md)) and hands each
conversation to chat-common, which writes
`<data_root>/<group>/render_markdown/<chat_uuid>/all.md` and the
document's rows in `indexed_markdown.doltlite_db` beside it.

What is shared with every chat source lives elsewhere: the markdown
layout, the collapsed tool asides and `LAYOUT_VERSION` in
[`chat-common/README.md`](../../chat-common/README.md); how render finds
the conversations that moved, in
[`data_architecture_parse_and_render.md` §5](../../../../../docs/dev/data_architecture_parse_and_render.md#5-incrementality-and-deletion).
This file covers what ChatGPT adds.

## One conversation, one document

A conversation's `mapping` is a tree, because an edited prompt or a
regenerated answer starts a branch. Render walks `current_node →
parent` from the leaf to the root, so the page shows the branch the
user last saw; a conversation with no usable chain falls back to
`create_time` order. A message with no `create_time` takes the previous
item's time plus 1 ms, and a null `created_at` when there is none.

Each message is one item, its `kind` decided by `(role, content_type)`
in `render.rs`:

| role | `kind` | aside |
|---|---|---|
| `user` | `User Input` | no |
| `assistant`, `thoughts` / `reasoning_recap` content | `LLM Thinking` | no |
| `assistant`, anything else | `LLM Response` (authored by `model_slug`) | no |
| `tool`, `function` | `Tool Call` | yes |
| anything else (`system`, …) | `Tool Call` | no |

A message's body is its content parts in order: text as prose, `code`
as a fence with its `language`, `execution_output` as a bare fence,
`thoughts` / `reasoning_recap` as a blockquote. Attachments are
materialized by chat-common from the ingest's `chatgpt_attachments`
edges; an image is drawn inline.

## Ids and links

Ids are minted in `src/render/ids.rs` under `IdNamespace::Chatgpt`,
scoped to the group, never ChatGPT's own ids passed through
(`docs/dev/entity_ids.md`). The document keeps ChatGPT's conversation id
as `external_id` and links back to `https://chatgpt.com/c/<id>`. The
account is the login's email, from the `me` table.

Bump [`RENDER_VERSION`](src/render/render.rs) when what this crate hands
chat-common changes; the render step then re-renders every document.

## Tests

The renderer is pinned by insta snapshots over the TNG fixture at
`../chatgpt/tests/fixtures/chatgpt_api/`, in the `chatgpt_render` module
of `//datalib/backend/etl/providers/chatgpt:chatgpt_tests`. Update them
with `bazelisk run //datalib/backend/etl/providers/chatgpt:chatgpt_tests.update`.
