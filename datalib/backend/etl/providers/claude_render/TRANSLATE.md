# Claude Translate

`claude-translate` reads a directory of conversations in
export-shape JSON (written by `claude-ingest` or by an
Anthropic bulk export) and emits, per conversation, a `.md` at
`<out>/render_markdown/claude/<account>/llm_chats/<conv>__<slug>.md` plus
that document's rows in the source's render store
(`<out>/render_markdown/indexed_markdown.doltlite_db`).

The Load step is provider-agnostic and lives in
`datalib_etl_render::grid_index`.

## What is a "document"?

**One Claude conversation is one document.** Messages are walked in
`(created_at, message_uuid)` order. Each assistant message can
contain a mix of `text`, `thinking`, `tool_use`, and `tool_result`
blocks; all of them surface in the rendered prose, with the
thinking/tool blocks each emitting their own grid row in addition to
the parent message row.

For each conversation we emit:

  * **One Chat row** (`kind = "Chat"`) — points at the rendered
    `.md` and carries the conversation name/summary for snippets.
  * **One message row per chat message** — `kind` is
    `User Input` / `LLM Response` / `Tool Call`, decided by sender.
    `text` is reconstructed from the message's `type=text` blocks so
    search prose isn't polluted by raw thinking transcripts.
  * **One block row per `tool_use` / `tool_result` / `thinking`** —
    `kind` is `LLM Thinking` for thinking blocks, `Tool Call`
    otherwise. `uuid` is `<message_uuid>:<block_index>`.

`document_uuid` is the upstream conversation UUID directly — Claude's
UUIDs are already globally unique, so no namespacing is needed.

## Markdown rendering

`render.rs` builds CommonMark with YAML frontmatter (`provider`,
`uuid`, `name`, `summary`, `account_uuid`, `project_uuid`, `model`,
`created_at`, `updated_at`). Per message it emits:

  * A `<div id="m-…" data-msg-index="N" class="msg msg--claude">`
    wrapper for anchor stability.
  * `## <Role>` heading + italic `*timestamp · model*` line.
  * Per content block, a `<a id="b-…">` anchor and type-specific
    rendering: `text` as prose, `thinking` as a `> blockquote` with
    a leading `<!-- thinking -->` HTML comment, `tool_use` /
    `tool_result` as fenced JSON with sorted keys for diff stability.

The body is byte-stable against the Python `_render_one_claude`.

## Incrementality

Render asks the raw store `dolt_diff` from the commit the render
cursor names and renders only the conversations that moved. Every document
it renders is written; an unchanged one writes identical rows, which
doltlite's content-addressed tables store as no change, so the index
never sees it.

Bump [`RENDER_VERSION`](src/render/render.rs) when the on-disk render
layout changes: the driver then re-renders every document. The shared
chat layout has its own number, `LAYOUT_VERSION` in chat-common, which
every chat provider declares through `render_params`.

## Goldens

The renderer + grid_rows emitter are pinned by insta snapshots
against the TNG-themed fixture at `tests/fixtures/claude_export/`.

```sh
bazelisk test //datalib/backend/etl/providers/claude:claude_render
```

Tagged `manual` in Bazel — the fixture lives in `CARGO_MANIFEST_DIR`
which the bazel sandbox doesn't surface in runfiles.
