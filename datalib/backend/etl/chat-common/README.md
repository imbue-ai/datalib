# chat-common — one markdown layout for every chat provider

Ten providers hand this crate a `NormalizedChat` and get back a
rendered `.md` plus the `grid_rows` that go with it. This file covers
the parts of that markdown you have to know about before changing it.

## The message header

Every message starts with a line like:

```markdown
## <span class="msg-author">Picard</span> <time class="msg-ts" datetime="2364-04-11T00:00:00+00:00" title="2364-04-11 00:00:00 UTC">Apr 11th, 2364 at 00:00</time>
```

**Keep the `##`.** It is not there to be a heading on screen — the UI
styles it down to a one-line "name, then a small grey time". It is
there because qmd, the semantic index, cuts a chunk at the best break
point near its size limit and scores an `h2` far above the blank line
it would otherwise settle for. Replace the heading with a plain `<div>`
and every chat's chunk boundaries quietly get worse, with nothing
failing to tell you.

The visible time is short; the full instant is in the `title`
attribute, one hover away. It is deliberately absolute rather than
Slack's "Today at 11:02": this file is written once and read for years,
so a word meaning "the day this was rendered" would be wrong by the
next morning.

## Asides: runs of tool steps fold into one `<details>`

An item with `is_aside` set is machinery rather than conversation — an
assistant's tool call or its result. The renderer wraps each *run* of
adjacent asides in one `<details class="tool-group">`, collapsed by
default, so a turn with five tool steps costs the reader one line.

The `<details>` goes *outside* the per-message `<div>`s, so every
anchor, copy button and grid-row highlight inside it still works. The
frontend opens every enclosing `<details>` before it scrolls to a
selected section (`applySelection` in `ChatBody.ce.vue`) — without
that, clicking a tool-call row in the grid would scroll to something
invisible.

Decide `is_aside` from what the item *is* upstream, not from its
`kind_label`. ChatGPT files `system` messages under the same "Tool
Call" label as real tool traffic, and a system prompt is content
someone may want to read.

## Unread messages

A provider that knows the account has not read a message upstream sets
`unread` on its item. The wrapper then carries an `unread` class, and
the first unread item in the document also carries `first-unread`,
which the UI draws a "New" rule above (`ChatBody.ce.vue`). Nothing
reaches the text: the rule is CSS, so qmd's chunks are the same either
way. `false` means read *or* unknown; a provider that cannot tell leaves
it there.

Who sets it, always only on a message someone else sent:

- Slack: past the conversation's `last_read` for a top-level message,
  past a followed thread's own `last_read` for a reply.
- Email: no `$seen`.
- Apple Messages: `message.is_read = 0`.
- Signal: `IncomingMessageDetails.read` false.
- SMS Backup & Restore: `read="0"`, from the newest backup file that
  has the message.
- WhatsApp: a `_id` past its chat's `last_read_message_row_id`.

LinkedIn, Facebook, Beeper
and Google Takeout carry no read state we store; the assistant
transcripts have none to carry.

A read mark is state that moves while the message does not, so a
provider that renders it must re-render when it moves, and only the
documents it moved across. Email's `$seen` is a row of its own
(`email_keywords`), whose diff already names the email. Slack keeps its
marks in volatile sidecars, and its diff scan reads the sidecar's diff
for the threads a moved mark crossed (`slack_render`'s `parse.rs`).

## Long messages are the frontend's problem, not this crate's

A hundred-line message is one the reader has to scroll past, and none of
the handling for that touches the markdown: `src/samples.rs` has a
sample that is one, and `datalib/ui/src/cards/chatSections.js` clamps it
to a screenful with a "Show more" pill, pins its `##` header to the top
of the pane while you are inside it, and puts ▲ / ▼ in that pinned
header for "start of this message" and "start of the next one".

Two constraints, both learned the hard way and both easy to undo by
accident:

- **A clamped card cannot have a sticky header.** The clamp is
  `overflow: hidden`, and an `overflow: hidden` ancestor disables
  `position: sticky` inside it. That is why the header only pins once
  the message is expanded — which is fine, because a clamped card is
  short.
- **Selecting into a clamped or collapsed section has to open it
  first.** `applySelection` removes the clamp and opens every enclosing
  `<details>` before it scrolls, or a grid-row click highlights
  something nobody can see. Two e2e specs assert the selected message is
  actually visible; they are what catches this.
- **A height measured before the pane has a width is nonsense.** In a
  column that has not been laid out — a hidden tab, the frame before
  first paint — every line wraps into a zero-width box, so a one-line
  message measures taller than the clamp and every message gets a "Show
  more" that does nothing. `decorateLongMessages` bails when
  `root.clientWidth` is 0 and waits on a `ResizeObserver` instead.

## What a provider parameterizes

Most of a provider's shape reaches the renderer through
`RenderProfile` — its `grid_rows` taxonomy, its `source_label`, its
`created_at` precision. Three knobs exist for one source each, and are
worth knowing about before you invent a fourth:

- **`RenderProfile` is per *call*, not per source.** Beeper bridges
  many upstreams and its taxonomy is per-network ("Signal Chat",
  "Google Chat Message"), so it groups its chats by network and calls
  `render_all` once per group.
- **`NormalizedChat::path_prefix`** puts a segment between
  `render_markdown/` and the chat's directory. Beeper's `<network>/`, so
  two upstreams bridged into one stanza stay apart on disk.
- **`NormalizedDoc::orphan_reactions`** carries reactions to a message
  the mirror does not have. A period-bucketed provider is expected to
  file a reaction under its *target's* period rather than its own — a
  reaction to a March message belongs in the March document however
  late it arrived, and beeper's parse resolves that against every event
  in the store. What is left is the case nothing can place: the target
  was never downloaded. Only beeper produces these today, and the TNG
  fixture has none, so this is the one path here that nothing
  exercises.

One `NormalizedChat` per *bucket* rather than per chat is the idiom for
a period-bucketed source (beeper, signal): attachment bundles are keyed
by `NormalizedChat::id`, and those sources load a bundle per bucket.
Nothing downstream notices — the path is `<chat_uuid>/<period>.md`
either way, and the chat-level grid row was already one per document.

## `LAYOUT_VERSION`

**Bump `LAYOUT_VERSION` whenever you change what `render_markdown`
writes.** Every chat provider declares it through
`RenderProcessor::render_params` (`layout_params()`), so one edit
changes every provider's render params and the driver re-renders all
of them. Bumping the eight by hand is the alternative, and the one
you forget is the one that keeps serving the old layout forever.

It stays out of the *stored* `render_version`, which is the provider's
alone: `datalib_step`'s render step checks that every version on disk
is one its processors declare.

## Looking at the output

```sh
bazelisk run //datalib/ui:render_preview     # rewrite the golden
open datalib/ui/tests/goldens/render_preview.html
```

`src/samples.rs` holds a corpus that hits every layout this crate can
produce — a tool run, a group chat with reactions and an attachment, a
system event, a hundred-line message, an undated item, an author whose
name is markup.

The page is not an imitation of the app: the CSS is read out of the
`.vue` files, the markdown goes through markdown-it with the same
options `ChatBody` uses, and `chatSections.js` — the module the
component itself imports — is inlined verbatim, so the clamp, the jump
controls and the copy buttons you are clicking are the app's own code.
That is also why that file is plain JavaScript: the preview can inline
it without a bundler.

`//datalib/ui:render_preview_test` regenerates the page and diffs it, so
the checked-in copy cannot drift from the sources it was built from.

## The UI sanitizes what you emit

A message body reaches the markdown as the sender wrote it, and the app
renders the markdown with HTML enabled because the section wrappers are
HTML. So before the page shows a document, `ui/src/cards/sanitize.ts`
runs it through DOMPurify: scripts, event handlers, `javascript:` URLs,
form controls and foreign iframes are dropped, and only the tags and
attributes the renderers actually use survive. **A renderer that starts
emitting a new tag or attribute has to add it there**, or the page will
silently strip it; `ui/tests/sanitize.test.ts` is where the vocabulary
is pinned.
