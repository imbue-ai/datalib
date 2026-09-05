# Anthropic Translate

`claude-translate` reads a directory of conversations in
export-shape JSON (written by `claude-download` or by an
Anthropic bulk export) and emits, per conversation, a `.md` at
`<out>/rendered_md/claude/<account>/llm_chats/<conv>__<slug>.md` plus
that document's rows in the source's render store
(`<out>/rendered_md/indexed_markdown.doltlite_db`).

The Load step is provider-agnostic and lives in
`datalib_etl::grid_index`.

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

The document's `markdowns` row carries `source_fingerprint`, a 64-bit
hash over the canonical JSON of the conversation, every message, every
content block, and every attachment (sorted by `(message_uuid,
block_index/attachment_index)`). Render skips a document whose
fingerprint already matches; the Load step uses the same value to
dedup against prior runs.

Bump [`RENDER_VERSION`](src/render/render.rs) when the on-disk
render layout changes in a way that should invalidate stale `.md`
files even though their `source_fingerprint` would otherwise still
match.

## Goldens

The renderer + grid_rows emitter are pinned by insta snapshots
against the TNG-themed fixture at `tests/fixtures/claude_export/`.

```sh
bazelisk test //datalib/backend/etl/providers/claude:claude_render
```

Tagged `manual` in Bazel — the fixture lives in `CARGO_MANIFEST_DIR`
which the bazel sandbox doesn't surface in runfiles.
