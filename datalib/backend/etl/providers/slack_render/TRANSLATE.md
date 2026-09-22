# Slack Translate

The slack translate step is an in-process library (called from
`datalib-sync`, no standalone bin) that reads the doltlite db at
`<out>/ingest/<name>/entities.doltlite_db` (written by `slack-ingest`) and
emits, per Slack thread, a `.md` under
`<out>/render_markdown/slack/<team>/<channel>/threads/` plus that
document's rows in the source's render store.

## What is a "document"?

**A Slack thread is one document.** The thread root and all of its
replies are grouped together; reactions, files, and edits are folded
into that document's rows. This matches the ergonomic unit a human
reader thinks of as "a conversation."

For each thread we emit:

  * **One thread row** (`kind = "slack_thread"`) — `entire_chat` holds
    the whole conversation rendered as CommonMark; `text` holds the
    first message's text for search snippets.
  * **N message rows** (`kind = "slack_message"`) — one per message,
    with `message_index` set so the thread can be reassembled in order.

`document_uuid` is the thread's UUID, `datalib_etl_slack::ids::thread`
over `(channel_id, thread_ts)` under `Upstream(team_id)`; a message's
carries its `ts` in its leading bits (`docs/dev/entity_ids.md`). The
same ids key the raw store's messages and threads.

## Markdown rendering

`mrkdwn.rs` converts Slack's mrkdwn dialect to CommonMark:

  * Bold/italic/strike/code/blockquote with Slack's quirky boundary
    rules.
  * `<@U…>` / `<#C…|name>` / `<!subteam^…>` / `<!here>` mention
    resolution against the workspace user map.
  * `<https://…|label>` link syntax → `[label](url)`.
  * `:shortcode:` → unicode via the `emojis` crate.
  * HTML entity decoding.

`render.rs` composes those primitives into per-thread markdown with
YAML frontmatter, then emits the document's rows.

## Incrementality

Render asks the raw store `dolt_diff` from the commit the render
cursor names and renders only the threads that moved. Every document
it renders is written; an unchanged one writes identical rows, which
doltlite's content-addressed tables store as no change, so the index
never sees it.

Bump [`RENDER_VERSION`](src/render/render.rs) when the on-disk render
layout changes: the driver then re-renders every document. The shared
chat layout has its own number, `LAYOUT_VERSION` in chat-common, which
every chat provider declares through `render_params`.

## Goldens

The translator + renderer are pinned by insta snapshots against the
TNG-themed fixture co-located at `tests/fixtures/slack_api/`. Run them
with:

```sh
bazelisk test //datalib/backend/etl/providers/slack:slack_translate
bazelisk test //datalib/backend/etl/providers/slack:slack_render
```

Both are tagged `manual` in Bazel because the fixture tree isn't in
the bazel sandbox runfiles.
