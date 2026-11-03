# Slack render

Slack threads from the raw store the ingest writes
([`../slack/INGEST.md`](../slack/INGEST.md)) render through chat-common,
one document per thread, filed by its id like every document
([`grid_rows.md`](../../../../../docs/dev/grid_rows.md) §"`qmd_path` and `source_id`"). See
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

A permalink is built the way Slack's `chat.getPermalink` answers, on
the workspace's own host from `auth.test`'s `url` (the `workspaces`
row): `https://<workspace>.slack.com/archives/<channel>/p<ts without
its dot>`, and for a reply `?thread_ts=<root ts>&cid=<channel>`. A
link on bare `slack.com` opens in whichever workspace the browser is
signed into, so that form, with `?team=<T…>`, is only for a store with
no workspace URL. `render::slack_link` is the one place either is built.

## Replies not mirrored yet

A thread's root says how many replies it has, `2 replies`, and while
the store holds fewer than that, how many it does not: `2 replies · 2
not mirrored yet`. A first sync reads the newest of the whole
workspace first, and a stopped run leaves threads owed, so a root can
sit in the store for hours before its replies do
([`../slack/INGEST.md`](../slack/INGEST.md) §"What a run still owes").
Without the line it reads like a message nobody answered.

The count is the root payload's `reply_count`. What is held is the
replies stored under the thread, counted, a broadcast reply among
them: what the page shows. The thread's `held_version` would say only
whether the last read of it was whole, not how many replies are
missing, and lives in a sidecar every attempt rewrites. A
`reply_count` that is not a number is a problem on the root, and the
root shows no line.

The line needs nothing extra to stay true. A reply's row carries its
root's key, which is the thread's bucket key, so a reply that arrives
renders its thread again, and the root's own row changes when Slack's
count does. `slack_tests`'s `reply_count` module holds a thread whose
read failed, then reads it, and checks the incremental render.

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
- `:shortcode:` → unicode by Slack's own names (`:thinking_face:`,
  `:flag-ca:`), from the table in `third-party/emoji-data`, and so is a
  reaction's name; a skin tone, `:+1::skin-tone-2:`, joins its emoji.
  Code keeps its shortcodes as typed. A workspace's custom emoji are
  images Slack hosts, which nothing downloads yet, so they stay
  `:name:`;
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

Both marks are rows of the raw store's `read_marks` table
(`datalib_etl::read_marks`): a conversation's under its conversation id,
a thread's under its thread key. A mark is content, so a moved one
moves the store's version and wakes the render, but no thread declares
one as an input: that would re-render every thread of a conversation
whose mark moved. The scan in `src/render/parse.rs` asks
`read_marks::moved` instead, and re-renders the threads whose root lies
between a conversation's old and new mark, or whose own mark moved. A
thread that is unread on both sides of a move is left alone.

A change to this crate's output needs a
[`RENDER_VERSION`](src/render/render.rs) bump.

## Tests

The `slack_render` and `slack_translate` modules of
`slack/:slack_tests` snapshot `../slack/tests/fixtures/slack_api/`;
`:slack_tests.update` rewrites them.
