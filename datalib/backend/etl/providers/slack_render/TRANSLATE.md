# Slack render

The `render_markdown` step of a `slack` group reads the raw store its
ingest wrote (`<data_root>/<group>/ingest/entities.doltlite_db`, described
in [`../slack/INGEST.md`](../slack/INGEST.md)) and hands each thread to
chat-common, which writes
`<data_root>/<group>/render_markdown/<thread_uuid>/all.md` and the
document's rows in `indexed_markdown.doltlite_db` beside it.

What is shared with every chat source lives elsewhere: the markdown
layout, the unread marker and `LAYOUT_VERSION` in
[`chat-common/README.md`](../../chat-common/README.md); how render finds
the threads that moved, in
[`data_architecture_parse_and_render.md` §5](../../../../../docs/dev/data_architecture_parse_and_render.md#5-incrementality-and-deletion).
This file covers what Slack adds.

## One thread, one document

A thread's root and all of its replies are one document; reactions,
files and edits fold into its items. A message that starts no thread is
a thread of one. The grid rows are one `Slack Thread` row per document
and one `Slack Message` row per message. The document is titled
`#channel: <root snippet>`; a DM is named after the people in it
(`@Jean-Luc Picard`), less the account itself.

`document_uuid` is `render::ids::thread` over the root's
`(team_id, channel_id, ts)` under the configured source; a message's id
carries its `ts` in its leading bits (`docs/dev/entity_ids.md`). The raw
store keys messages and threads by `{team}#{channel}#{ts}`, the
upstream's own key, never an entity id. The document's `external_id`
is `{channel_id}#{thread_ts}` and its link is the thread's permalink.

## mrkdwn

`src/render/mrkdwn.rs` converts Slack's mrkdwn dialect to CommonMark:

- bold, italic, strike, code and blockquote, with Slack's boundary rules;
- `<@U…>`, `<#C…|name>`, `<!subteam^…>`, `<!here>` / `<!channel>` /
  `<!everyone>`, resolved against the workspace's users and channels;
- `<https://…|label>` → `[label](url)`;
- `:shortcode:` → unicode, through the `emojis` crate;
- the three entities Slack escapes (`&amp;`, `&lt;`, `&gt;`).

## Unread messages

A top-level message is unread past its conversation's `last_read`; a
reply, past its followed thread's own `last_read`. The account's own
messages are never unread.

Those marks are volatile, so a moved mark touches only a `_bookkeeping`
table and the content diff never sees it. The scan in
`src/render/parse.rs` therefore also reads
`dolt_diff_channel_read_states_bookkeeping` and
`dolt_diff_messages_bookkeeping`, and re-renders the threads whose root
lies between a conversation's old and new `last_read`, or whose own
thread mark moved. A thread that is unread on both sides of a move is
left alone.

Bump [`RENDER_VERSION`](src/render/render.rs) when what this crate hands
chat-common changes; the render step then re-renders every document.

## Tests

The renderer is pinned by insta snapshots over the TNG fixture at
`../slack/tests/fixtures/slack_api/`, in the `slack_render` and
`slack_translate` modules of
`//datalib/backend/etl/providers/slack:slack_tests`. Update them with
`bazelisk run //datalib/backend/etl/providers/slack:slack_tests.update`.
