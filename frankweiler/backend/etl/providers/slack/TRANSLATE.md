# Slack Translate

The slack translate step is an in-process library (called from
`frankweiler-sync`, no standalone bin) that reads the doltlite db at
`<out>/raw/<name>/entities.doltlite_db` (written by `slack-download`) and
emits, per Slack thread, a `.md` plus a `.grid_rows.json` sidecar
under `<out>/rendered_md/slack/<team>/<channel>/threads/`.

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

`document_uuid` is the thread's UUID, derived deterministically from
`(team_id, channel_id, thread_ts)` via the shared
`SLACK_UUID_NS` v5 namespace. The Python translator uses the same
namespace, so a Rust-translated row and a Python-translated row for
the same Slack thread collide on UUID — the cutover is a write-through
swap, not a re-keying.

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
YAML frontmatter, then emits the sidecar.

## Incrementality

Each `.md` carries `source_fingerprint` in its frontmatter, hashed
from the canonical Slack JSON of every message in the thread. On the
next run, if the existing `.md` already matches, the write is skipped
and the sidecar is not regenerated.

Bump [`RENDER_VERSION`](translate/render.rs) when the on-disk render
layout changes in a way that should invalidate stale `.md` files even
though their `source_fingerprint` would otherwise still match.

## Goldens

The translator + renderer are pinned by insta snapshots against the
TNG-themed fixture co-located at `tests/fixtures/slack_api/`. Run them
with:

```sh
bazelisk test //frankweiler/backend/etl/providers/slack:slack_translate
bazelisk test //frankweiler/backend/etl/providers/slack:slack_render
```

Both are tagged `manual` in Bazel because the fixture tree isn't in
the bazel sandbox runfiles.
