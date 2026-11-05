# Render: a document goes only when its rows did

**Status: partly built. What is built is in
[`data_architecture_parse_and_render.md`](../data_architecture_parse_and_render.md)
§"The sweep" (#1066, #1073, #1087); the `DocDraft` reshape was dropped.
This page is what is left.** It is the render half of
[`audits/2026-10-05_loose_ends.md`](../audits/2026-10-05_loose_ends.md)
§3.2 and §4, read against the tree at `6188afdd7`, so check a line number
before relying on it. git has the full plan at `479b6562d`.

## Problems

- Fetch problems reach no document for email, facebook, sms, beeper,
  signal or notion: they should come through `render_inputs`.
- Parse problems should be reported by stage and by entity. Chatgpt and
  slack report `Whole` scopes on narrowed runs, and parse problems are
  swept with no stage filter.
- A lint over `*_render/src` for `warn!` beside a `continue` or a skip
  would keep this class of bug out.

## Records that fail

- A record that fails the first time it is seen has no document, and only
  its renderer knows which rows it tried, so nothing general reports it.
- Codex still fails its whole render on one bad line.

## Smaller bugs

Linkedin posts keyed by row position (`posts.rs:133-151`), email_render's
`"references"` column (`parse.rs:452`), signal's `decode_chat_item` turning
a decode failure into an empty item, garmin's `unwrap_or(Value::Null)`,
chat-common's blob materialize failure.
