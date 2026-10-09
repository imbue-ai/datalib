# Slack render

Slack threads from the raw store the ingest writes
([`../slack/INGEST.md`](../slack/INGEST.md)) render through chat-common,
one `render_markdown/<thread_uuid>/all.md` per thread. See
[`chat-common/README.md`](../../chat-common/README.md) for the page layout, the unread marker and `LAYOUT_VERSION`,
and [`data_architecture_parse_and_render.md` §5](../../../../../docs/dev/data_architecture_parse_and_render.md#5-incrementality-and-deletion) for the diff scan this file extends.

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

- `*bold*` → `**bold**` and `~strike~` → `~~strike~~`, with Slack's
  word-boundary rules; a `>` quote is ended with a blank line. `_italic_`
  and backticks are already CommonMark;
- `<@U…>` is a chip link to the user, `[@Name](slack://user?team=T&id=U
  "@Name (slack:T/U)")`, through `chip_link`, so datalib draws who that
  is; inside code it is plain `@Name`, which code shows literally, and a
  thread's title, plain text, keeps `@Name` too;
- `<#C…|name>`, `<!subteam^…>`, `<!here>` / `<!channel>` /
  `<!everyone>`, resolved against the workspace's users and channels;
- a `!` typed straight before any of these, or before a labelled link,
  is escaped, so markdown does not read the link after it as an image;
- `<https://…|label>` → `[label](url)`;
- `:shortcode:` → unicode, through the `emojis` crate;
- the three entities Slack escapes (`&amp;`, `&lt;`, `&gt;`) stay
  entities in running text, so a `<b>` someone typed shows as typed;
  they are decoded inside code, which markdown shows literally, and in
  the `&gt;` that opens a Slack quote. A thread's title decodes them all:
  it is plain text, and `Title` escapes it;
- markdown's own syntax, which Slack shows as typed, is escaped outside
  code and outside the `<…>` constructs: `[`, `]`, `|`, a backslash
  before punctuation, and a line's opening `#`, `- `, `1. `, `---` or
  four-space indent. A name from the users or channels table, or a
  mention's own label, is escaped as text on a markdown line.

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

A change to this crate's output needs a
[`RENDER_VERSION`](src/render/render.rs) bump.

## Tests

The `slack_render` and `slack_translate` modules of
`slack/:slack_tests` snapshot `../slack/tests/fixtures/slack_api/`;
`:slack_tests.update` rewrites them.
