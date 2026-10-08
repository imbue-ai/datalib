# ChatGPT render

Each conversation in a `chatgpt` group's raw store
([`../chatgpt/INGEST.md`](../chatgpt/INGEST.md)) becomes one chat-common
document, `render_markdown/<chat_uuid>/all.md`. The page layout and
`LAYOUT_VERSION` are in [`chat-common/README.md`](../../chat-common/README.md); how render picks the conversations
that moved is [`data_architecture_parse_and_render.md` §5](../../../../../docs/dev/data_architecture_parse_and_render.md#5-incrementality-and-deletion).

## One conversation, one document

A conversation's `mapping` is a tree, because an edited prompt or a
regenerated answer starts a branch. The page reads the branch ending at
`current_node`, the one the user last saw, and every other version is
folded in, collapsed, where it forked (chat-common's
`branches::reading_order`; see its README). A conversation with no
usable `current_node` reads every message in `create_time` order. A message with no `create_time` takes the previous
item's time plus 1 ms, and a null `created_at` when there is none.

Each message is one item, its `kind` decided by `(role, content_type)`
in `render.rs`:

| role | `kind` | aside |
|---|---|---|
| `user` | `User Input` | no |
| `assistant`, `thoughts` / `reasoning_recap` content | `LLM Thinking` | no |
| `assistant`, anything else | `LLM Response` (authored by `model_slug`) | no |
| `tool`, `function` | `Tool Call` | yes, unless it carries a file (a generated image) |
| anything else (`system`, …) | `Tool Call` | no |

A message's body is its content parts in order: text as prose, `code`
as a fence with its `language`, `execution_output` as a bare fence,
`thoughts` / `reasoning_recap` as a blockquote. A `multimodal_text`
message keeps the words beside its images; a `tether_quote` (the
model quoting an uploaded file) is the file's title over its text,
quoted; a `tether_browsing_display` is what it showed. A content type
none of these covers renders nothing and is an `uncovered_type`
problem on its message, and a message left with no body and no
attachment (an empty thought, say) is not drawn at all. Attachments are
materialized by chat-common from the ingest's `chatgpt_attachments`
edges; an image is drawn inline.

ChatGPT marks up its text with private-use characters (`U+E200`–`U+E2FF`,
`src/render/sentinels.rs`): an inline `url` becomes a markdown link, an
`entity` its name, and every other kind (file and web citations, image
groups) is dropped.

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
