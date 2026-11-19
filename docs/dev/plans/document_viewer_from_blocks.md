# The document viewer, drawn from blocks

*Proposal (2026-11-19). Step 1 landed in #1889; step 2 is #1891.
Nothing else is built. It picks up where
[`documents_to_blocks.md`](documents_to_blocks.md) step 5 stopped
("paging through what is read together comes after"), and chooses a
layout by kind through the registry
[`dispatch_on_kind.md`](dispatch_on_kind.md) proposes. What a person
sees today was checked in the TNG demo (`//datalib:dev_tng`) at
`0ff971d20`; claims about the code were read in the same tree.*

Since #1879 nothing writes a `.md` file: each block's body is a row of
its source's render store. The viewer still asks for a document as one
markdown string. The server builds that string by putting each body
back into what render drew around it, and the UI then cuts the string
apart again, by lengths, so that it can render each message alone. Most
of the viewer's harder code exists to undo that join.

This plan removes the join, one step at a time, **and keeps the viewer
dumb**: it is handed markdown, a block at a time, and draws it. It
learns nothing about any source. Each step deletes code, lands on its
own, and leaves the viewer working.

## 1. What happens today

A **block** is one thing a person or program wrote: a message, an
email, a comment. A **container** holds blocks: a thread, a chat, a
page. Both words are defined in `documents_to_blocks.md` §"Words".

| Step | Where | What it does |
|---|---|---|
| Draw | chat-common `render_item` | each message: a `<div>` wrapper, a `##` header with the author as a link and a `<time>`, a recipients line, a `🏷` labels line, the body, reactions as a list, a replies line; between messages, `<details>` openers and closers around tool steps and other versions |
| Keep | render store | the body in `sections`, everything else in `drawings` (#1868) |
| Join | `stored_text::spell` | joins the pieces into one string |
| Measure | `/chat/{uuid}` | trims the front matter off the joined string as text, then recounts each piece's length in UTF-16 units and checks the lengths add up to the string, a check left from when the string was a file |
| Cut | `renderDocument.ts` | cuts the string by those lengths; renders the pieces between messages together, with a placeholder per message, so a fold opened in one closes in a later one; renders each message alone; checks the wrapper with two regexes; strips any copy of a message's id from inside a body; falls back piece by piece if a fold swallowed a placeholder |
| Decorate | `ChatBody.ce.vue`, `chatSections.js` | writes the HTML into a frame, then redraws each `<time>` from its `datetime` and each author link from its `href`, guesses where each header is (`metaHost`'s three guesses) to put a 🆔 button in it, clamps long messages and folds long recipient lines by measuring them |
| Refresh | `DocCard.ce.vue` | on every `index_changed`, fetches the whole document again, compares it as JSON, and redraws all of it if anything differs |

What the demo shows (seen, not read):

- **An email thread** ("Mission Briefing — Sector 001", three emails)
  shows every email fully open, each with To and Cc, a
  `🏷 Starred · Important · Inbox` line at the top of the body, and 🆔
  and ↗ in its header.
- **A Signal group chat** of ten one-line messages is taller than the
  pane: each message is a bordered box with a chip, a full date and 🆔.
- **Slack reactions** are a bulleted list, one line per person.
- **A GitHub pull request's comments** have a different header: 🆔 on a
  line of its own above it, then `(COMMENTED)` in italics. **A Notion
  discussion** is 🆔 alone on a line, then the comment. Every renderer
  but chat-common draws its own header, and the 🆔 button's placement
  is a guess at it.
- **A search hit** opens the thread scrolled to its email, outlined.
  That works well and must not change.

`GET /children/{uuid}` (step 2 of the blocks plan) already lists a
container's blocks around an anchor, a page each way, with each block's
author, time and a one-line preview. Nothing in the UI calls it yet.

## 2. Keep the viewer dumb

Drawing headers in Vue components, from facts, was this plan's first
idea. Markdown per block is the better one for now:

- **The viewer decorates markup anyway.** A body holds person links and
  `<time>` elements, and the viewer turns them into chips. A header
  written in the same markup gets the same chips for nothing; a header
  component would be a second way of drawing the same chip.
- **One drawer serves every reader.** The markdown an agent reads from
  `/chat`, the export `datalib-step markdown` writes, and what the
  viewer shows come from one Rust function per thing drawn, tested by
  snapshots.
- **A new source needs no UI code.** It draws markdown, as today.

So the smell is not markdown. It is four things markdown was made to
carry, each removable while every block stays markdown:

1. **The join.** Pieces are joined on the server and cut apart in the
   viewer.
2. **Folds as text.** A fold is two pieces of text, an opener and a
   closer, with messages between them, so the viewer has to render text
   that spans messages.
3. **The box inside the text.** Each message's wrapper `<div>` is part
   of its markdown, so the viewer has to check it with a regex and
   strip copies of it from the body.
4. **A header per renderer.** Each renderer draws its own header, so
   the viewer has to guess where a header is.

What would justify components instead: a header that needs state the
markup cannot hold. Nothing today does; the folded recipients line and
the long-message clamp are a class and a button.

## 3. The path

Each step says what it deletes, what it adds, whether the next sync
re-renders, what a person notices, and how it is checked.

### Step 1: pieces on the wire

`/chat/{uuid}?pieces=true` answers with `pieces`, each message's
markdown as its own string with its uuid, in place of `body`. Without
the parameter the answer is `body` alone, as before.

- **Deletes:** the viewer's cut by lengths; on the server, recounting
  lengths in UTF-16 units and checking them against the joined string
  (`body_sections.rs`), and the warning when front matter cut into a
  message. The front matter comes off the first piece only.
- **Adds:** a query parameter.
- **Re-render:** none.
- **Notices:** nothing. A script that reads `sections` no longer gets
  them, and renders `body` whole, as the docs already told it to for a
  document that came without them. `rendered_documents.md` and
  `applets.md` tell it to ask for `pieces` instead, which needs no
  arithmetic.
- **Checked by:** `section-isolation.spec.ts`, whose control now serves
  the same payload as one piece and shows the next message hidden.

### Step 2: the box is the viewer's

Every renderer that wraps a block draws the wrapper the same way: an
opening `<div id="m-{uuid}" data-section-uuid="{uuid}" class="…">`, a
blank line, what is inside, `</div>`, a blank line. One constructor,
`Section::boxed(uuid, class, inside)`, draws it for all of them, and the
render store keeps the box's class in a column of `sections`. `/chat`
sends a boxed piece as its class and what is inside the box; the viewer
draws the box itself. Joined, with each box drawn around its inside, the
pieces are still `body` byte for byte.

What is stored and indexed does not change: the drawing and the text
keep the box, so no search text moves and nothing re-embeds. The read
side takes the box off knowing exactly what was drawn, from the uuid and
the class. Taking it out of storage too waits for step 5, when drawings
give way to facts.

- **Deletes:** both wrapper regexes in `renderDocument.ts`, and the
  sanitizer pass over the wrapper alone; each renderer's own wrapper
  code (`msg_div_open`, `MSG_DIV_CLOSE`, Perseus's, Garmin's, the time
  series', Notion's); stripping copied ids out of bodies
  (`reservedSections`).
- **Adds:** one constructor, one nullable column, a `class` on a piece.
- **Re-render:** once, as every change to the render store's shape
  does.
- **Notices:** nothing.
- **Checked by:** every render snapshot, unchanged; the isolation tests,
  whose hostile bodies still cannot close a box early or answer to
  another message's id.

Stripping copies of a message's id out of a body (`reservedSections` in
the sanitizer) went with it: a lookup by id now takes the outermost
element naming it, which is the box the viewer drew, so a copy inside a
message is never found first and can be left as written.

`close_open_blocks` in chat-common stays, for a reason inside one
message: the body is followed, in the same markdown, by the
attachments, the reactions and the replies line render draws after it,
and a code fence the body leaves open would turn those into code. It
goes in step 5, when those are drawn from facts and the body is
rendered alone.

### Step 3: folds are structure

Render says, per block, whether it is a tool step and which other
version it is on (two columns of `sections`). One Rust function groups
a run of blocks into folds: the export draws them as `<details>`, and
`/chat` sends them as a node holding its pieces. The viewer draws the
`<details>` around them.

- **Deletes:** the placeholder frame, rendering the pieces between
  messages together, and the fallback when a fold swallows a
  placeholder. What is left of `renderDocument.ts` renders one piece.
  The `<details>` openers and closers go from `drawings`.
- **Adds:** two columns, one grouping function, a nested shape on the
  wire.
- **Re-render:** once.
- **Notices:** nothing.
- **Checked by:** the grouping drawn from the columns matches the folds
  each fixture document draws today, before those are taken out of the
  drawings. A test renders every fixture document's pieces each alone
  and finds no piece that leaves an element open for the next, so no
  renderer but chat-common hides a fold in its pieces.

Paging (step 6) lists blocks, not pieces; folds drawn from facts on the
blocks are what let a page carry its own folds. A page never ends
inside a fold.

### Step 4: one header for every renderer

Every renderer that draws a block's header draws it with
`datalib_etl_render::message::MessageHeader`, as chat-common, Drive
comments and Todoist already do. Still to move: GitHub and GitLab
(`forge-render-common`), calendar events and Notion discussions. A
word such as `COMMENTED` becomes a part of the header the function
draws.

- **Deletes:** `metaHost`'s three guesses, which become one selector;
  each renderer's own header code.
- **Re-render:** each of those sources, once, in its own change.
- **Notices:** comments on a pull request and a Notion page, and a
  calendar event, look like every other message.

### Step 5: what is drawn into a header becomes a fact

The header, recipients, labels and replies line move out of `drawings`:
`/chat` draws them when it is asked, with the same Rust function, from
what is stored on the block. Each moves on its own, by the rule step 5a
of the blocks plan set: draw it from the fact, check that against the
stored drawing, then take it out of the drawing.

| What the header or fold shows | Where it is today | Becomes |
|---|---|---|
| author, handle, time, source link | the block's grid row | (already a fact) |
| who it was addressed to, labels | `sections.addressed`, `sections.labels` | (already a fact) |
| unread, and the first unread | a class in the drawing | a column |
| which message a reaction is on | nothing: a reaction's row names its chat, not its message | its parent, or an edge |
| "12 replies · 9 not mirrored yet" | a line in the drawing | two columns on the thread's first block |
| a system event | an italic line in the drawing | the row's kind |

- **Deletes:** most of `drawings`. When the last of these moves, the
  export draws from facts too, and the table goes. With nothing drawn
  after a body in its own markdown, chat-common's `close_open_blocks`
  goes from the viewer's path; only a join (the export, `body`) closes
  what a body leaves open.
- **Notices:** nothing. A read mark that moves today re-renders every
  document it crossed (chat-common README §"Unread messages"); once
  unread is a column, that render rewrites rows, not drawings.

### Step 6: a window that grows both ways

`/children/{uuid}?with=pieces` answers a page of blocks as step 3's
pieces and folds. The viewer keeps a window of them.

- **Where it opens:** at a block for a search hit or a link to a
  message; at the first unread, or else the newest, for a chat; at the
  start for anything else.
- **Paging:** a cursor at each end, as `/children` already answers.
  `pagedWindow.ts` grows one way only (the grid's list), so the viewer
  needs a two-ended version in the same pure-core shape. When a page
  lands above, it holds the reader still by measuring the block on
  screen before and after.
- **Small containers load whole**, so ⌘F and select-all work as today.
  Past a page or two, find-in-page sees only what is loaded, as in
  Slack and Gmail.
- **Live updates** ask for what is past the newest cursor and again for
  the window on screen; only the blocks whose content changed redraw,
  so the reader keeps their place.
- **Deletes:** the whole-document fetch and JSON compare in `DocCard`'s
  `refresh`, and checking a document's remote images all at once.
- **Notices:** a long chat opens fast and the reader's place holds
  through a sync.

### Step 7: layouts by kind

The last step changes what a person sees, and each part of it is a
decision of its own (§4). The container's kind picks a layout through
the registry in `dispatch_on_kind.md`, conversation by default. The
server draws the hints a layout needs, so the viewer stays dumb: a
block that continues the one before it (same author, a few minutes
later), a block shown as its one-line preview.

- **Conversation** (an email thread, a pull request's discussion):
  read emails as one line each, who, preview and when, as Gmail and
  Apple Mail do; the unread ones and the last open.
- **Chat** (Signal, WhatsApp, Messages, SMS, Beeper, Slack, Messenger,
  Google Chat, LinkedIn): no box per message; a divider per day; one
  header for a run of one person's messages, as Slack and iMessage do;
  reactions as one pill per emoji with a count.
- **Transcript** (Claude, ChatGPT, Claude Code, Codex): today's look.
- **Page** (Notion, Drive, PDF, calendar, contacts, time series): one
  big block, no header box.

Header actions that mail and chat clients show on hover (🆔, ↗), and
labels as chips beside the header rather than a `🏷` line in the body,
belong here too.

## 4. Open questions

- **Should read emails start collapsed?** Every mail client does it,
  but it changes what a person sees first in a thread.
- **How close is "a few minutes"** for one header over a run of one
  person's chat messages?
- **Reactions**: an item of their own or an attribute of the message
  (`documents_to_blocks.md` §"Open questions")? A pill needs only to
  know which message a reaction is on.
- **Can a person choose a layout**, say read a Slack thread as a
  conversation? Not proposed; the registry would make it one setting.
