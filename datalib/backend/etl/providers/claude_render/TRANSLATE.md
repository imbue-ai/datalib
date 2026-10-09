# Claude render

Every conversation and every Project in a `claude` group's raw store,
whichever ingest method filled it
([`../claude/INGEST.md`](../claude/INGEST.md)), is rendered as a page
through chat-common. Shared machinery is documented once: the page
layout and `LAYOUT_VERSION` in [`chat-common/README.md`](../../chat-common/README.md), and change detection in
[`data_architecture_parse_and_render.md` §5](../../../../../docs/dev/data_architecture_parse_and_render.md#5-incrementality-and-deletion).

## Conversations

An API-fetched payload goes through
`normalize::normalize_to_export_shape` in the download crate on its way
out of the store; an export-ingested one is already that shape. Either
way one parser reads it (`src/render/parse.rs`).

A conversation's messages are a tree, because an edited prompt or a
retried answer starts a branch: each names its `parent_message_uuid`.
The page reads the branch ending at `current_leaf_message_uuid`, the
one the user last saw, and every other version is folded in, collapsed,
where it forked (chat-common's `branches::reading_order`; see its
README). A conversation with no usable leaf (a bulk export may carry
none) reads every message in `(created_at, message_uuid)` order.

Each message becomes one item whose `kind` comes from its sender: `User
Input` for `human`, `LLM Response` for `assistant` (authored by the
conversation's `model`), `Tool Call` for anything else. Its body is the
message's `text` blocks, so search prose is not polluted by thinking
or tool traffic; then a numbered **Sources** list of every page those
blocks' `citations` name, once each by url; then each `attachments[]`
entry's extracted text as a quoted block, headed by its file name, or
"Pasted text" when the name is empty (a paste is an attachment with
none). Downloadable `files[]` are materialized by chat-common from the
ingest's `claude_attachments` edges.

Every `thinking`, `tool_use` and `tool_result` block is an item of its
own, placed just before its message's answer:

| block | `kind` | aside | body |
|---|---|---|---|
| `thinking` | `LLM Thinking` | no | a `<details>` "Thinking" with the thought quoted |
| `tool_use` | `Tool Call` | yes | the tool's name, and its `input`: readable for `artifacts` (its title, then the code fenced in its language, or an update's old and new text), `create_file` (the path, then the file fenced) and `bash_tool` (the command fenced as bash); JSON with sorted keys for any other tool, or one missing what that needs |
| `tool_result` | `Tool Call` | yes | the tool's name, `(error)` when `is_error`, and its content: a `knowledge` item (a page a search or fetch returned) as a link with its site name unless it carries text of its own, text in a fence, anything else as JSON |

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

[`RENDER_VERSION`](src/render/render.rs) is bumped whenever this
crate's output changes.

## Tests

`claude/:claude_tests` (its `claude_render` module) snapshots the
render of `../claude/tests/fixtures/claude_export/`;
`:claude_tests.update` rewrites the snapshots.
