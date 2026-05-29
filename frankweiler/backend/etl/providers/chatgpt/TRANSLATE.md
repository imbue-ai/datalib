# ChatGPT Translate

The chatgpt translate step is an in-process library (called from
`frankweiler-sync`, no standalone bin) that reads the doltlite db at
`<out>/raw/<name>.doltlite_db` (written by `chatgpt-download`) and
emits, per ChatGPT conversation, a `.md` plus a co-located
`.grid_rows.json` sidecar under
`<out>/rendered_md/openai/<account>/llm_chats/<conv>__<slug>.md`.

The Load step is provider-agnostic and lives in `frankweiler_etl::load`.

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

  * A `<div id="m-…" data-msg-index="N" class="msg msg--openai">`
    wrapper for anchor stability.
  * `## <Role>` heading + italic `*timestamp · model_slug*` line.
  * Per content part, a `<a id="b-…">` anchor and content-type-specific
    rendering: text as prose, code as fenced blocks with `language`,
    `execution_output` as bare fences, `thoughts`/`reasoning_recap`
    as blockquotes with a leading `<!-- kind -->` HTML comment.

The body is byte-stable against the Python `_render_one_openai`.

## Incrementality

The sidecar header carries `source_fingerprint`, a 64-bit hash over
the canonical JSON of the conversation row, every message row, and
every content part (sorted by `(message_id, part_index)`). The Load
step uses this to dedup the sidecar against prior runs.

Bump [`RENDER_VERSION`](src/translate/grid_rows.rs) when the on-disk
render layout changes in a way that should invalidate stale `.md`
files even though their `source_fingerprint` would otherwise still
match.

## Goldens

The renderer + grid_rows emitter are pinned by insta snapshots
against the TNG-themed fixture at `tests/fixtures/chatgpt_api/`.

```sh
cd frankweiler/backend
cargo test -p frankweiler-etl-chatgpt --test chatgpt_render
```

Tagged `manual` in Bazel — the fixture lives in `CARGO_MANIFEST_DIR`
which the bazel sandbox doesn't surface in runfiles.
