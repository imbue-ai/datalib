# ChatGPT Translate

The chatgpt translate step is an in-process library (called from
`datalib-sync`, no standalone bin) that reads the doltlite db at
`<out>/ingest/<name>/entities.doltlite_db` (written by `chatgpt-ingest`) and
emits, per ChatGPT conversation, a `.md` at
`<out>/render_markdown/chatgpt/<account>/llm_chats/<conv>__<slug>.md` plus
that document's rows in the source's render store
(`<out>/render_markdown/indexed_markdown.doltlite_db`).

The Load step is provider-agnostic and lives in
`datalib_etl_render::grid_index`.

## What is a "document"?

**One ChatGPT conversation is one document.** The conversation's
`current_node → parent_id` chain is walked from the leaf to the root
to recover the canonical reading order; orphans fall back to
`create_time` sort. System messages and `model_editable_context`
parts are filtered out — the rendered prose matches what a user sees
in the web app.

For each conversation we emit:

  * **One Chat row** (`kind = "Chat"`) — points at the rendered
    `.md` and carries the conversation title for snippet display.
  * **N message rows** — one per surfaced message. `kind` is
    `User Input` / `LLM Response` / `LLM Thinking` / `Tool Call`,
    decided by `(role, content_type)`.

`document_uuid` is the upstream conversation UUID directly (no v5
namespacing — ChatGPT's UUIDs are already globally unique).

## Markdown rendering

`render.rs` builds CommonMark with YAML frontmatter (`provider`,
`id`, `title`, `account_id`, `create_time`, `update_time`,
`default_model_slug`). Per message it emits:

  * A `<div id="m-…" data-msg-index="N" class="msg msg--chatgpt">`
    wrapper for anchor stability.
  * `## <Role>` heading + italic `*timestamp · model_slug*` line.
  * Per content part, a `<a id="b-…">` anchor and content-type-specific
    rendering: text as prose, code as fenced blocks with `language`,
    `execution_output` as bare fences, `thoughts`/`reasoning_recap`
    as blockquotes with a leading `<!-- kind -->` HTML comment.

The body is byte-stable against the Python `_render_one_openai`.

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
against the TNG-themed fixture at `tests/fixtures/chatgpt_api/`.

```sh
bazelisk test //datalib/backend/etl/providers/chatgpt:chatgpt_render
```

Tagged `manual` in Bazel — the fixture lives in `CARGO_MANIFEST_DIR`
which the bazel sandbox doesn't surface in runfiles.
