# ChatGPT render

Each conversation in a `chatgpt` group's raw store
([`../chatgpt/INGEST.md`](../chatgpt/INGEST.md)) becomes one chat-common
document, `render_markdown/<chat_uuid>/all.md`. The page layout and
`LAYOUT_VERSION` are in [`chat-common/README.md`](../../chat-common/README.md); how render picks the conversations
that moved is [`data_architecture_parse_and_render.md` §5](../../../../../docs/dev/data_architecture_parse_and_render.md#5-incrementality-and-deletion).

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

Bump [`RENDER_VERSION`](src/render/render.rs) when this crate's output
changes.

## Tests

Insta snapshots over `../chatgpt/tests/fixtures/chatgpt_api/` pin the
output: the `chatgpt_render` module of `chatgpt/:chatgpt_tests`,
rewritten by `:chatgpt_tests.update`.
