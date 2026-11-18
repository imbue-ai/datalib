# Moving from documents to blocks: grouping chosen when a search runs

**Status: proposal (2026-11-17). Steps 1, 2, 3a, 3b and 3c have
landed (#1765, #1770, #1775, #1782, #1818); 4a and 5 are under way;
nothing else has started.** Facts about the tree were checked against
the code on 2026-11-16; file references are to that tree.

## Words

| word | means |
|---|---|
| **block** | one piece of content: one thing a person or program wrote, with one author, one time and a body (a message, an email, a comment, an event, a tool call, a page's own text). What search ranks and a hit lands on. A `grid_rows` row; the text below and the search index's code call it an **item**. |
| **data node** | a node of the tree that holds blocks and other data nodes: a chat, a thread, a channel, a label, a page, a session. It often has a name and other metadata (its kind, its upstream id), and most of what it says is in the blocks it holds. A `data_nodes` row. Many data nodes are **places**: ones a person browses or filters by, such as a channel, a mailbox or a workspace. |
| **document** | a group of blocks that are often read together: a thread, a chat, a page. Today it is also the markdown file render writes for it, which step 5 stops writing. |

The text below also calls a container **read together**. When that holds
is not settled; see §"Open questions".

## The problem

Every record datalib renders goes into a markdown file, and the file
is the unit everything else is built on. qmd forced that: it could
only index files. Now that search reads its own index
([`search_index_from_ram.md`](search_index_from_ram.md)), the file is
a choice, and it is costing us in three ways.

- **A chat is cut into months.** A Signal, WhatsApp, Beeper, Messages
  or SMS conversation becomes one document per calendar month (Google
  Chat and Voice the same, Messenger one per year). A conversation that
  runs across midnight on the 31st is two documents. A search ranks the
  two halves separately, and no embedded chunk sees both, so a reply
  sent the next month has lost what it replies to. Changing a source's
  `period` setting re-keys every document, because the month is part of
  each document's id.
- **A search has to say what it wants rows to be.** The grid holds two
  kinds of row in one table: one row per document, which has no single
  sender and no single time, and one row per message, which does.
  `is:document` picks between them. The grid opens with it on, Browse
  adds it, and a person who leaves it off sees every message instead.
  Free text is ranked per document and then landed on one message.
- **Parent links are mostly thrown away.** Each source knows what a
  message belongs to and what it replies to, but `grid_rows` has no
  column for either. Slack's thread survives only as which file a
  message was written into. Email's `in_reply_to` is loaded and never
  read. A Beeper reply becomes the text `↪ in reply to <id>`, and a
  Signal quote is counted as "not shown".

The rendered text, meanwhile, lives only in those files. The render
store holds no text, so everything that needs it (the search index,
the viewer, landing a hit on a message) reads the file and cuts it by
byte offsets.

**So this plan also ends the markdown files.** Each item's rendered
text is stored in the database with its row, and nothing is written to
disk but the stores and the blobs. That removes a whole class of
machinery:

- `section_layout`, the byte spans that cut a file into messages, and
  the check that fails a render when they do not add up to the file's
  size;
- `markdowns.md_path`, the `unindexed/` directory convention, and
  unlinking an old file when its path moves or its document empties
  (`grid_rows.qmd_path`, the copy of that path on every row, is
  already gone);
- reading a file and converting bytes to lines to land a hit, and
  re-quoting the snippet from it;
- a file and its database row disagreeing after a crash between the
  two writes. The text commits with its row, so a reader pinned to a
  commit sees the text that commit had.

Render writes each item's text on its own, and whatever shows a thread
or a conversation joins its items in order when it is read.
Nothing needs to know where one message starts inside a larger text,
so the backpointers that answer that go too: a section's byte span
and index within its document, and a chunk's `first_section` and
`last_section` counted in that index. A chunk still packs several
short items; it records them by uuid, one row per item in a table of
its own, and "which messages does this vector speak for" is then a
lookup, not arithmetic over a file's layout. Not a range of positions:
a message that arrives late (a backfill, a slow sync) can land inside
the range, and the chunk would then claim an item its vector never
saw. The one offset left is inside a single item too long for one
chunk, which is cut in pieces as it is today.

## What a document is today

Measured in the code, not the docs. One document is:

| Source | One document is | Grouping key |
|---|---|---|
| Slack | one thread (a top-level message with no replies is its own) | `{team}#{channel}#{thread_ts or ts}` |
| Email | one thread | `(account, thread_id)`; mbox: `X-GM-THRID`, else the message itself |
| Claude, ChatGPT, LinkedIn | one conversation | conversation id |
| Claude Code, Codex | one session transcript; each sub-agent's is its own | session or thread id |
| Beeper, Signal | room or chat × `period` (setting, default month) | chat id + period key |
| WhatsApp, Messages, SMS | chat × month | chat id + month |
| Google Takeout | Chat space or Voice conversation × month; activity × year | space or conversation + period |
| Facebook | Messenger thread × year; a post or album whole | thread + year |
| Notion | one page; each discussion its own | page id |
| Google Drive | one file, its comment threads folded in | file id |
| GitHub, GitLab | one pull or merge request | PR id |

The time buckets come from `datalib_etl::periodize::Period`
(`etl/src/periodize.rs`); `sms_backup_restore` and `google_takeout`
format their own months. A bucketed source mints its document's uuid
from the chat id **and the period key** (`beeper_render/src/render/ids.rs`
and its siblings), while each message's uuid comes from its own
upstream key and does not move.

What a row carries about where it belongs:

- `markdown_uuid`: the document the row was written into. For a
  bucketed chat that is the month, not the chat.
- `conversation_uuid`: in `chat-common`, the chat's id on every row, so
  `convo:` already spans every month of one chat. (`grid_rows.md` and
  the field's doc comment say it equals the row's own uuid on a
  document row; for a bucketed chat it does not, and no row has the
  chat's id as its own.)
- `message_index`: position within the **document**, not the
  conversation.
- No thread, parent or reply-to column. The `edges` table has no kind
  column and only Perseus writes it.

Where upstream's own parent links are, and what happens to them:

| Source | Upstream has | Today |
|---|---|---|
| Slack | `thread_ts`, `thread_root_uuid`, `is_thread_root` in raw | becomes the document |
| Email | `in_reply_to`, `references_header` in raw | loaded by render, never read |
| Beeper | reply target (`inReplyToID`) | body text only |
| Google Takeout Chat | `topic_id` (thread), quoted message | body text only |
| GitHub, GitLab | `in_reply_to_id` on review comments | groups comments in the markdown |
| ChatGPT | `parent_id` (branches) | reading order of the current branch |
| Codex, Claude Code | parent session of a sub-agent | display string only |
| Notion comments | page and block | `notion_page_uuid`, `notion_block_uuid` columns |
| Google Drive comments | thread id | only inside `source_url` |
| Signal | a quote on the message in raw | counted as "not shown" (`signal/src/ingest/not_shown.rs`) |
| WhatsApp, Messages, LinkedIn, Messenger | not checked in raw; nothing in render | — |

## The model

**An item is the unit.** An item is one thing a person or program
wrote, with one author and one time: a message, an email, a comment, a
page, an event, a tool call. It is one row, and its body is stored with
the row (§"Store the body, format on read"). Everything is keyed by the item's uuid, which already exists
and does not move.

**Containment is a tree of any depth.** Each source's things nest
differently, and no fixed set of levels fits them all:

| Source | From the top down |
|---|---|
| Slack | workspace › channel › thread › reply |
| Email | account › thread › message; and account › label › label › … filing threads |
| WhatsApp, Signal, Messenger | chat › message |
| Notion | workspace › page › page › … › block; a discussion on a block |
| Google Drive | folder › … › file › comment thread › reply |
| Claude Code | project › session › sub-agent session › message |

So every item, and every **data node** (a container that holds items;
"node" below, never a DAG step, and a row of the `data_nodes` table),
names **one parent and how it sorts among its siblings**, and that is
all the tree stores. A container has a uuid a person can open. What
some containers say is an item inside them (a Notion page's own text,
a Slack thread's first message); some say nothing of their own (an
email thread). "The children of
X, in order" is one indexed query on `(parent, sort key, uuid)` and needs no
stored list.

**Siblings sort by a key that never renumbers**, never by a dense
position we count. An item with a time sorts by `created_at_utc`, then
uuid; item uuids are already time-prefixed, so the tie-break agrees. A
message that arrives late (a backfill, a slow sync) lands among its
siblings without moving any of them. Where upstream orders children
without times, as Notion orders a page's blocks, the key is upstream's
own order, and a block that moves is a change to the page that
re-renders it anyway.

**A container keeps its upstream record too**: a Slack channel's id, an
email thread's id, a Notion page's id, as `(upstream_entity_kind,
upstream_id)`. A download problem about a container ("could not list
channel C01") then reaches the container's row the same way an item's
reaches the item. A container's uuid is minted by the one rule in
[`entity_ids.md`](../entity_ids.md) from its kind and upstream key, with
no stamp, as documents are minted today. Its upstream values are spelled
by the same per-provider recipe in the ingest crate that the download's
problems and the render's rows both call
([`problems_as_step_events.md`](problems_as_step_events.md)), so a
container's key is upstream's real id, not one composed at render. A kind that is both a place
and read together (a WhatsApp chat) is one row with both roles, not
two rows with one uuid each.

**What each kind of container is for is declared by its kind**, not
discovered from depth. A container can be:

- **read together**: a Slack thread, an email thread, a WhatsApp chat,
  a Notion page, a Drive file, a session. This is what a search groups
  by when it groups, what the viewer opens, and what embedding cuts its
  windows from. Nested ones stay separate: a Notion subpage is read on
  its own, not as part of its parent.
- **a place**: a Slack channel, a mailbox, a Notion workspace, a Drive
  folder, a Claude Code project. What a person browses or filters by,
  not what they read through.

A kind may be both: a WhatsApp chat is the place and the thing read
together. Each item stores the uuid of its nearest ancestor of each
role, worked out once at render, so grouping by either is one column
and never a walk up the tree. A source with no threads loses nothing:
its chat is read together and is its own place.

**A node can also be filed under other nodes.** An email thread lives
in its account, and is also filed under every label or folder its
messages carry; labels nest (`Work › Clients › Acme`). A photo is
filed under its keywords and its collections, which nest too, and the
search already treats those as a tree (`keyword:` matches the keywords
above a photo's; a collection answers to each set above it). So the
tree has two kinds of parent:

- **contained by**: exactly one, where the node lives. Removing it
  removes the node.
- **filed under**: any number, each a container itself (a label, a
  folder, a keyword, a collection) with its own parent.

**Everything else is an edge, not a parent.** Reply-to (a Signal
quote, an email's `In-Reply-To`, a GitHub review reply), a mention, a
link: each is a relation between two nodes, so a message that quotes
one from last year stays where it was said.

## Browsing the tree

The model makes possible something the app does not have: **a tree a
person can browse, and a search held to any branch of it.**

- **The tree card.** The library is the root; under it, the person's
  folders of sources, then each source, then its accounts, channels,
  labels, folders, pages, as deep as the source goes.
  Each node shows how many items are under it and expands to its
  children, both those it contains and those filed under it, loaded a
  level at a time. Opening a leaf container opens what is read
  together.
- **Held to a branch.** Choosing a node holds the grid and every search
  to the items under it: `under:<node>` for everything beneath it,
  `at:<node>` for its children alone. A node is a chip like a person, so
  it is clicked or typed, never spelled out. `source:` becomes
  `under:` the source's node, which it already means.

**Above the sources: the library and its folders.** A source's
top containers (an email account, a Slack workspace, a chat with no
account above it) have no parent today. They get one:

- **A source is a node.** It is the source's group: its uuid is minted
  from the group id, and its children are the source's top containers.
  Renderers do not mint it; they keep naming no parent at their top,
  and `unified_grid_and_search_index`, which knows each row's source, reads that as the
  source node. A step is not a node: it is how a source's data was
  made, and it lives in the DAG. A problem's `stage` says which step
  raised it.
- **A folder of sources is a group too**, one with no type and no
  steps, and a group names the folder it is in with one optional
  config field (`parent = "<group id>"`). A person arranges their
  sources into folders on the Manage screen. Moving a source never
  changes its id, which is its directory on disk; only its parent
  moves.
- **The library is the one root**, above every folder and source.

The arrangement belongs to the person, not to any source, so a move
costs one row: the source-to-folder links are read from the config
into a small table of their own, and no row of any source is rewritten,
re-rendered or re-indexed when a source moves. Every node but the
library then has a parent, so a problem rolls up through its
containers, its source and the person's folders to the library, and
"how many problems under X" is the same lookup as "how many items
under X".

**How "under" stays fast.** `unified_grid_and_search_index` keeps one table of every
node's ancestors, `(node, ancestor, depth)`, following both kinds of
parent, so "under X" is one indexed lookup however deep X is and
whichever way a node reached it. A message under a nested label is
found from the label and from every label above it, with no walk up
the tree at search time and no special case for labels, keywords or
folders. Its size is the sum of every node's depth: a few million
rows for a root of a million items about four levels deep.

**An item keeps the upstream record it came from**, as
`upstream_entity_kind` and `upstream_id`, the pair `grid_rows` already
carries. A problem names what its step was holding when it arose (that
pair, an item uuid or a path) and never a document or a parent, and is
linked to its item when it is read
([`problems_as_step_events.md`](problems_as_step_events.md)). So the
pair must reach the stored item, and the item's parents must be
reachable from it.

**A group is chosen when a search runs, not when render runs.** A
search finds items. How its results are shown is a separate choice:
one row per item, per thing read together, or per place. A group's row is
derived from its items: who took part, its first and last time, how
many matched. Nothing about a group is fixed at render, so there is no
month to choose and no setting to change.

**What gets embedded is its own choice.** One message embedded alone
has lost its context; that is why chunks span messages today. So
embedding still needs a context: a window of nearby items within the
one thing they are read together in. A Slack thread usually fits in
one window; a ten-year chat is many windows; a Notion page's blocks are
windowed within the page and never mixed with a subpage's. How wide a window is and
whether windows overlap is decided by the eval, and changing it
re-embeds without changing anything a person sees.

**Store the body, format on read.** What render stores for an item is
its content, not text laid out for one reader:

- **The facts** a header is drawn from (author, time, handles, reply-to,
  edited) are columns, most of them on the row already.
- **The body** is stored once, in one normalized form of ours: markdown
  with our link forms for a person, a date and an attachment. Turning
  upstream's own form (Slack's markup, an email's HTML, Notion's
  blocks) into it is the provider's work and stays at render. The form
  is a small written grammar, strict enough that every output below
  parses it, and turning it into HTML keeps chat-common's sanitizer
  allowlist.

Everything around the body (an item's header line, a group's title and
who took part, the separators) is drawn when something reads it, by one
function per output:

- **Markdown**, for the viewer and an export, and **HTML** later. These
  may change as often as anyone likes; a change costs nothing stored.
  The promises in [`rendered_documents.md`](../../user/rendered_documents.md)
  (the link a person is, the `<time>` a date is, `data-section-uuid`)
  become what these functions draw, not bytes on disk. A view per node
  kind is the same function chosen by kind.
- **Index text**, what a chunk is embedded and keyword-indexed from.
  It is kept plain and changes rarely: a chunk is keyed by its text's
  hash, so any change to this function re-embeds every chunk of every
  source. So it carries the body, who wrote it and when (the eval is
  not yet good enough to say a timestamp can go), and nothing for
  show: no URLs, a chip as its name. A date is written with its day of
  the week and its month as words ("Tue 3 Mar 2026 14:05"), so a search
  for "a Tuesday in March" has words to meet. It has a version, and moving it is
  as deliberate as a render-version bump; it costs a re-embed, never a
  re-render or a re-download.

This is possible only now. Chunks were found again by byte spans of one
file, which pinned one text for every reader. A chunk is cut by us and
names the items it covers, so the text it was made from need exist
nowhere but while it is embedded.

**What is read together is read in pages, not as a file.** The viewer
opens it at an item and asks for the items around it in order. A
ten-year group chat opens as fast as a short one.

**A list starts at an anchor and grows both ways.** Every list of a
node's children, or of everything under it, is asked for at an
**anchor**: an item (`at=<uuid>`), or a moment (`at=<stamp>`: the first
item at or after it; "now" is the common one). The answer is a window
around the anchor with a cursor at each end, and scrolling asks for the
next page past either one: older above, newer below, as a chat's "jump
to message" does. A cursor is the edge row's `(sort key, uuid)`, never
a position counted from the start, so an item that arrives late, or
during the scroll, lands in its place and moves nothing on screen. A
moment means nothing among children the source orders without times (a
page's blocks); those are anchored on an item. The UI keeps the window
the way the grid already does, in the pure core of `pagedWindow.ts`,
which holds the reader still when a page lands above them
([`paged_grids.md`](paged_grids.md)); the two are one design.

**A view per kind of node.** Opening a node shows it the way its kind
is read: a message in its context, a thread's items a page at a time,
a channel's threads to browse. A shared layer supplies the views for
everything that goes through it (chat-common for every chat source's
message, thread and channel), and a provider or an applet adds one for
a kind of its own. That is a registry keyed on the node's kind, which
is what [`dispatch_on_kind.md`](dispatch_on_kind.md) proposes for
chips and cards. So the kinds become a small closed set (channel,
thread, chat, mailbox, label, folder, page, calendar, project,
collection, …) designed with that registry, beside the free-text
`kind` a person reads, and `/chat` gives way to it.

## The same thing in two sources

The same email in Gmail and in Fastmail is two records in two sources,
each in its own tree, and for now nothing says they are the same: two
copies. Not one node with two parents. If it is ever worth knowing, it
is an edge between the two, matched on a key the sources share (an
email's Message-ID), and neither tree changes.

## Contacts in the tree

A card sits in its address book. The groups a source keeps (vCard
`CATEGORIES`, a CardDAV group card, a Google contact group) are filings,
as an email's labels are. A label a person makes in the contacts app is
one too, with one difference: its writer is the applet, whose store is
under `datalib_curated/`, not a render step. So the applet owns its
labels' data nodes and filings, and the ancestors table reads them
beside the render stores'. A person the app merged from several cards
is the same: two records, not a parent.

## What changes, and for whom

Every place that assumes a document, from the read side of the tree:

| Where | Today | After |
|---|---|---|
| `grid_rows.is_document`, `is:document` (grid default, Browse, dashboard, eval) | one row per document, or per message | `group:read` (what is read together), `group:place`, `group:none`; `is:document` kept as an alias for `group:read`, since people type it |
| Search index (`search_documents`, `search_sections`, `search_chunks`) | keyed by document; sections are byte spans of the file | keyed by item; chunks name the items they cover and the group they were cut from |
| Ranking (`documents_of_hits`, `fuse`, `land`) | ranks documents, then lands each on a message by re-reading the file | ranks items, then groups them; a hit is an item, so there is nothing to land |
| `Keyed` / `Kept::Sections` | what the filters keep, per document | what the filters keep is a set of items |
| `WithoutVectors` | documents without vectors | items not covered by any embedded chunk |
| Viewer (`/chat/{markdown_uuid}`, `DocCard`) | the whole file, cut into sections by byte length | the items of what is read together, a page at a time, opened at one |
| The route's name, `/chat/{markdown_uuid}` | serves any document (a page, a PDF, an event), named for the first sources, which were chats | `/data_node/{uuid}`, named for what it opens; `/chat` kept as an alias, since agents use it (`agent_user.md`) |
| Links (`datalib:doc/<page>#<section>`), copied 🆔 | a document and a message in it | an item, with what holds it looked up |
| Embedding map | one point per document (the mean of its chunks) | one point per group at the grouping chosen |
| Problems banner | rows scoped to a document | problems name an item; a group shows its items' problems ([`problems_as_step_events.md`](problems_as_step_events.md)) |
| Feedback anchors (`data-section-uuid`) | a message inside a document | unchanged: already per item |
| Eval targets (`uuid`, `markdown_uuid`) | a hit anywhere in the document counts | a target keeps its `uuid` (an item, which does not move); its `markdown_uuid` is replaced by the uuid of what holds it, read together, by a script run against the root. The set stays private; "found" is scored per item and per group |
| Agents and Minds workspaces reading files (`docs/agent_user.md` §"Markdown", `rendered_documents.md`) | read `<group>/render_markdown/*.md` directly | read through the applet: `/chat/{uuid}` already returns a page's text, and a route listing a container's items takes over from the files; the docs say so in the step that stops writing them |
| `docs/user/searching.md`, `rendered_documents.md`, `agent_user.md` | describe documents | rewritten with the step that changes each |

There are few enough users that links people have saved need no forward
path. A query a person types does, which is why `is:document` stays as
an alias.

## Forward path for existing data

**The markdown files go, and something reads them today.**
`docs/agent_user.md` tells an agent to read `<group>/render_markdown/`
directly, and a Minds workspace may. That is a break a person using an
agent notices. The step that stops writing the files (step 5) first
makes the applet serve everything they held, rewrites both docs, and
says it in the release notes. The `.md` trees already on disk are
derived, so the render step deletes its own tree on its first run after
that step, as the index step deletes the tree qmd's index was in.

Everything this changes is derived: render stores, the grid, the search
index. They are rebuilt from the raw stores, which already hold every
parent link listed above. The vectors in the inference cache are keyed
by their prompt's hash, so a chunk whose text and context are unchanged
keeps its vector; chunks cut differently are embedded again.

The `period` setting on Beeper and Signal stops meaning anything. The
config upgrade drops it with a warning at its line, as it did for
`embed_search_index`.

## Order of work

Each step lands on its own, leaves search working, and is checked
against the eval before the next.

1. **The tree.** Render gives every container a row of its own and
   fills each node's parent, what it is filed under, its sort key among
   its siblings, and its
   nearest ancestor that is read together and that is a place; reply-to
   and the other relations go in as edges. Containers' upstream values
   come from the ingest crate's recipe. The few sections that are not
   their row today become rows or lose their anchor: GitHub's and
   GitLab's description (keyed by the document row), Notion's table of
   contents and Perseus's sentence spans (no rows); everywhere else a
   section's uuid is already its row's
   ([`problems_as_step_events.md`](problems_as_step_events.md)
   §"What the tree does today"). `unified_grid_and_search_index`
   carries the columns. A re-render
   and a re-index; nothing a person sees changes. Fixes the
   `conversation_uuid` docs on the way.
2. **Children in order.** The index on `(parent, sort key, uuid)`, made
   with the first query that uses it, and one route that lists a
   container's children around an anchor, a page each way at a time.
   The viewer can use it beside the file.
3. **The body in the render store.** Each item's body is stored with
   its row, and the index text is drawn from it on read
   (§"Store the body, format on read"). The search index reads items from the store instead of
   cutting files, and `keyword_index` folds into the index step, which
   already reads every render store. Leaving a source out of search
   survives the fold: before it, a source was out of search by having
   no `keyword_index` step, and `embed` stays the per-source switch for
   meaning. After it, a source is out of search by being named in the
   index step's `params.not_searched`; the step searches every source
   it reads but those. The config upgrade lists there every source that
   had no `keyword_index` step. In three parts:
   - **3a. Bodies.** A `sections` table in the render store: every
     piece of a document in order, keyed or not. chat-common hands each
     item's body and who it was addressed to, and marks its layout as
     frame, which is not kept; every other renderer's pieces are kept
     whole and marked `framed`, unkeyed ones too (a PDF's text is
     unkeyed), and each splits its own bodies off later. Every renderer
     also hands its front matter as facts (`Front`), drawn by one
     function and kept on `markdowns.front`, and its document as
     sections, those that handed none (Notion, Perseus, Garmin, the
     time-series pages, the storage report) included. A re-render, and
     nothing a person sees.
   - **3b. Index text from the store.** The search index draws each
     item's index text from its body and its row, and stops reading
     files. A framed body is drawn as it is, so its chunks' text and
     hashes do not move; only chat-common's sources re-embed. An
     unframed item is drawn as one plain envelope line, its labels, then
     its body: `Will Riker <riker@enterprise.starfleet> to Jean-Luc
     Picard — Tue 3 Mar 2026 14:05`, then `Labels: Away missions`. A
     person is named, with the address one would type for them where the
     handle is an email or a phone number, so a pasted address or a word
     of a label finds the item by its words as well as through the search
     terms; a Slack id and the like stay out. Recipients only where the
     item has its own, as an email's To and Cc, not a chat's members on
     every line; no URLs. A chunk that starts inside an item repeats its
     envelope line, so every chunk says who and when. The title stays in
     the prompt's frame.

     The text is never stored: a chunk keeps its hash, and its vector is
     filed under it, so a chunk re-embeds exactly when its text changes.
     The text is drawn again whenever it is needed, so the function must
     be deterministic: a date in the item's own offset, never the
     machine's clock, and a test draws one item under two `TZ` settings
     and gets the same bytes. Which items to draw is the render store's
     diff since the commit the index last read. `INDEX_TEXT_VERSION`
     beside the function says when the function itself changed: the
     index keeps the version it drew with, and a mismatch redraws every
     item, re-embedding only the chunks whose hash moved. A golden test
     draws chat-common's and the hostile samples and fails when the
     output changes and the version does not.
   - **3c. The fold.** `keyword_index` into the index step, with the
     config upgrade above (`migrate_config/src/keyword_fold.rs`,
     #1818), which runs after the rewrite that renamed the index group
     to `unified_grid_and_search_index` (`index_name.rs`, #1796). The
     index step took over the aggregator's work too: it keeps the
     inference cache to the vectors a chunk still names and reports what
     search holds of each source, so `qmd_aggregator` is gone and the
     embedding map reads every `*/embed` (`aggregator_drop.rs`, #1821,
     merged into #1818). That left the one-slot `qmd_keyword` lock
     (#1795) with one holder, the index step, which the runner never
     runs beside itself, so the lock can go (#1845). The step has since
     taken its group's name: its function is
     `unified_grid_and_search_index`, where it was `grid_index`
     (`index_step_name.rs`).
4. **Grouping when a search runs.** In two parts:
   - **4a. Every data node is a row.** Nearly every container already is
     a document's row under its own uuid: in one real library, 60,395
     of 63,976 data nodes, every thread, chat, event and session. The
     590 that were not are nearly all places: Slack channels, DMs and
     the workspace, email labels and accounts, Claude Code projects,
     calendars, repositories, an address book. The index step writes
     each of those a row from the `data_nodes` its documents carry, kept
     while any document carries it, with no `markdown_uuid`
     (`grid_rows.md` §"`parent_uuid`, …"). Still to come in 4a:
     - **A path on each data node.** The ancestors' ids from the top of
       its source, so everything under a node is a prefix range, as
       Lightroom keeps a `genealogy` on its keywords and collections and
       none on its photos. A block reaches the tree through
       `parent_uuid`. Counts are worked out when asked, as a node's own
       children or as everything under it, never stored.
     - **`is:<kind>`** (`is:channel`, `is:label`), a word each renderer
       declares for its data nodes, written as a search term. There is no
       word for every place: `folder` is what a Drive, Lightroom or
       Messenger folder is called, and `place:` is already where a thing
       is on the map.
   - **4b. Ranking blocks, grouping when asked.** Search ranks blocks,
     keeping every block a ranking finds where it now keeps the best
     one per document, and rolls them up to the grouping asked for:
     `group:document` (today's answer, ranked as today, which
     `is:document` stays an alias for), `group:place` (each hit's
     `place_uuid`, ranked first by its best block), or `group:none`, a
     row per block and nothing to land. The eval scores a target per
     block and per document, and compares the grouped answer with
     today's.
5. **The viewer reads items, and the `.md` files go.** Only two things
   read the files' bytes: the viewer's `/chat/{markdown_uuid}` and search
   landing (`hits/mapping.rs`), which quotes a hit and picks its row from
   the file. In order:
   - **5a. The store spells every document.** `sections` keeps a
     chat-common item's body alone and no frame piece, so chat-common's
     fourteen sources could not be read back from the store. A
     `drawn_pieces` table keeps the rest of each piece as drawn: an
     item's drawing with its body cut out and where it goes back, and
     each frame piece whole. The store then spells every document byte
     for byte, which a step test checks against every fixture document's
     file. Storing the drawing, rather than drawing it again from facts,
     is the choice that lets the files go first: what the frame and a
     message's header show (unread, a branch, reply counts, which
     message a reaction is on) is not all on the rows yet. Drawing from
     facts can follow one fact at a time, each checked against the
     stored drawing.
   - **5b.** "Unindexed" (Notion's table of contents) is a `markdowns`
     column, `searchable`, which every reader reads. It is still set from
     the folder the renderer files the document under, until 5e takes
     the paths away and the renderer says so itself.
   - **5c.** The viewer and search landing read the render store, at the
     commit the grid index read.
   - **5d.** A command prints or exports a document's markdown: the way
     out the README and the first-time guide promise as the `.md` files.
     Tests and fixtures read drawn text.
   - **5e.** Render stops writing the files, and the existing ones are
     deleted, each render store kept; the rule that rendered markdown is
     write-only goes with them. Paging through what is read together, a
     page at a time, comes after.
6. **No time buckets.** A bucketed source renders one container per
   chat. The `period` setting goes.
7. **Embedding context.** Chunks are cut from windows of what is read
   together, and the eval picks the window.
8. **Browsing the tree.** The ancestors table in `unified_grid_and_search_index`, with
   each row's sort key copied in so everything under a node is one
   index range around an anchor; `under:` and `at:` as search keys; and
   the tree card. Whether a large subtree pages off that index or by
   the grid's list-then-slice (measured at 74k rows, not at today's
   941k) is measured then. `keyword:` and
   collections inside sets become `under:` their node, with the old keys
   kept as aliases. The library root, the source nodes and the folders
   above them come here too, with the `parent` config field and moving a
   source on the Manage screen.
9. **The rest of the table above:** the map, links, the problems banner
   with the problems plan, the docs.

## Open questions

- **What is read together.** Today each kind of container says so once,
  at render: a thread, a chat, a page, and in a chat cut into months,
  each month's document. Whether a Slack channel's messages are read
  together depends on how the channel is used: a thready channel's
  threads are, and a flat channel's run of messages may not be. Steps
  4 (grouping), 6 (no time buckets) and 7 (embedding windows) all lean
  on the answer. Until it is settled, a search can group by document or
  by place.
- **How wide an embedding window is.** How many items or bytes of a
  thread, and does it overlap the next? The eval decides.
  [`multimodal_retrieval.md`](multimodal_retrieval.md) §5.1 argues for
  embedding whole threads; that is one input, not settled.
- **How a group ranks.** Reciprocal rank fusion per document today.
  Per group it could be the best item's score, or a sum over items;
  a long chat must not win by being long.
- **What a group row shows** in the grid when it spans years: first and
  last time, the last few participants, the number of items matched.
- **How deep each source's tree goes.** A Notion page is one row today,
  with none for its blocks. Making blocks items lets a hit land on a
  paragraph and a page be windowed for embedding, at the cost of many
  more rows; Drive's paragraphs are the same question. Which kinds are
  read together and which are places is each source's to declare, and
  this plan's tables are a first guess to check source by source.
- **Where a label lives.** Gmail labels a message, and a thread
  carries the union of its messages' labels; JMAP mailboxes likewise.
  Is a thread filed under a label when any of its messages is, and does
  the tree card show the thread or the message there?
- **Counts on the tree card.** "How many items under X" for a node with
  half a million under it is a count over the ancestors table; it may
  need keeping per node at `unified_grid_and_search_index` time rather than counting when
  a level opens.
- **Which upstream hierarchies to bring in.** Email labels and
  mailboxes and Lightroom's keywords and collections already reach the
  search (`label:`, `keyword:`, `collection:`); Slack channels and Notion
  pages are in their raw stores. Drive folders, Apple Mail's mailboxes
  and a Claude Code project are still to check. Each source
  declares its tree in step 1 and the card shows what it declared.
- **What storing the body costs.** The body goes in the render store,
  versioned with its row, so a reader at a commit sees that commit's
  text. On one real root (186k files) the markdown is 1.25 GB and the
  render stores already 9.6 GB. Doltlite keeps every version a commit
  reached and does not compress
  ([`doltlite.md`](../doltlite.md) §"What a write costs"), so a
  re-render that changes every item's body (a render-version bump)
  keeps the old copy too, until history is thrown away, which can be
  done at any time. An append is cheap if the key is the item's uuid,
  which is time-prefixed, so new items land in the last leaves: a
  growing agent session adds its new messages instead of rewriting the
  whole file as it does today. Step 3 measures one full re-render's
  growth so the number is known, not to decide where the text goes.
  If it is large, a render-version bump can write the new text on a
  fresh branch and move `main` to it, then drop the old branch's
  history, so two full copies never stay.
- **Reactions.** An item of their own (they have an author and a time)
  or an attribute of the item they react to?
- **Time-of-day bucketing** for display ("this conversation on 3 May")
  is a grouping too, and could be offered as `group:day` without
  being stored.

## Found on the way

Three docs disagree with the code; each gets fixed by the step that
touches it, or sooner:

- `docs/dev/grid_rows.md` §"`conversation_uuid`…" says a document row's
  `conversation_uuid` is its own uuid; for a bucketed chat it is the
  chat's.
- `docs/dev/data_architecture_parse_and_render.md` §`render_inputs`
  calls Slack a periodizing renderer; Slack renders one document per
  thread.
- `datalib/backend/embedding_map/README.md` says the map reads qmd's
  vectors; it reads the search index's.
