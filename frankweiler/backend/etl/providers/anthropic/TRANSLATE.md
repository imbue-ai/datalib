# Anthropic Translate

`anthropic-translate` reads a directory of conversations in
export-shape JSON (written by `anthropic-download` or by an
Anthropic bulk export) and emits, per conversation, a `.md` plus a
co-located `.grid_rows.json` sidecar under
`<out>/rendered_md/anthropic/<account>/llm_chats/<conv>__<slug>.md`.

The Load step is provider-agnostic and lives in `frankweiler_etl::load`.

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

  * A `<div id="m-…" data-msg-index="N" class="msg msg--anthropic">`
    wrapper for anchor stability.
  * `## <Role>` heading + italic `*timestamp · model*` line.
  * Per content block, a `<a id="b-…">` anchor and type-specific
    rendering: `text` as prose, `thinking` as a `> blockquote` with
    a leading `<!-- thinking -->` HTML comment, `tool_use` /
    `tool_result` as fenced JSON with sorted keys for diff stability.

The body is byte-stable against the Python `_render_one_anthropic`.

## Incrementality

The sidecar header carries `source_fingerprint`, a 64-bit hash over
the canonical JSON of the conversation, every message, every content
block, and every attachment (sorted by `(message_uuid,
block_index/attachment_index)`). The Load step uses this to dedup
the sidecar against prior runs.

Bump [`RENDER_VERSION`](src/translate/grid_rows.rs) when the on-disk
render layout changes in a way that should invalidate stale `.md`
files even though their `source_fingerprint` would otherwise still
match.

## Goldens

The renderer + grid_rows emitter are pinned by insta snapshots
against the TNG-themed fixture at `tests/fixtures/anthropic_api/`.

```sh
cd frankweiler/backend
cargo test -p frankweiler-etl-anthropic --test anthropic_render
```

Tagged `manual` in Bazel — the fixture lives in `CARGO_MANIFEST_DIR`
which the bazel sandbox doesn't surface in runfiles.
