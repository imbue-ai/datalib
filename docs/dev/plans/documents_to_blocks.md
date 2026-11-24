# Moving from documents to blocks: search finds blocks, shown in their documents

**Status: proposal (2026-11-17), checked against the tree on
2026-11-24.** Landed:

- steps 1, 2, 3a, 3b and 3c (#1765, #1770, #1775, #1782, #1818);
- step 5, all of it (#1868, #1872, #1876, #1879): render writes no
  `.md` files, and the viewer pages through a document
  ([`completed/document_viewer_from_blocks.md`](completed/document_viewer_from_blocks.md));
- step 6 with step 1 (#1765): a chat is one document;
- the first part of 4a (#1863): every data node is a grid row;
- part of step 8 (#1913): a Library card browses each source's places.

Step 7 was answered another way: a message is a chunk of its own
(#1786), so no window is cut. Still open: the rest of 4a (a path on each
data node, `is:<kind>`), all of 4b, the rest of 8, 9 and 10. §"How
search works" was added on 2026-11-24 and is unreviewed. Facts in the
sections below were checked against the code on 2026-11-16, before most
of this landed; file references are to that tree.

## Words

| word | means |
|---|---|
| **block** | content: one thing a person or program wrote, with one author, one time and a body (a message, an email, a comment, an event, a tool call, a page's own text). What search ranks and a hit lands on. A `grid_rows` row; the text below and the search index's code call it an **item**. A page source's whole page (a Notion page, a calendar event, a contact) is one **big block** for now, which its renderer may later split into the blocks it is made of. |
| **layout** | what is drawn around blocks and says nothing of its own: front matter, the title, a `<details>` opener. The code calls a block and a stretch of layout alike a section (`Section`, the render store's `sections`). |
| **data node** | a node of the tree that holds blocks and other data nodes: a chat, a thread, a channel, a label, a page, a session. It often has a name and other metadata (its kind, its upstream id), and most of what it says is in the blocks it holds. A `nodes` row. Many data nodes are **places**: ones a person browses or filters by, such as a channel, a mailbox or a workspace. |
| **document** | a group of blocks that are often read together: a thread, a chat, a page. Render used to write each as a markdown file; since step 5 it is rows of its source's render store. |

The text below also calls a container **read together**. When that holds
is not settled; see §"Open questions".

## The problem

Every record Datalib renders goes into a markdown file, and the file
is the unit everything else is built on. qmd forced that: it could
only index files. Now that search reads its own index
([`search_index_from_ram.md`](completed/search_index_from_ram.md)), the file is
a choice, and it is costing us in three ways.

- **A chat is cut into months** (fixed with step 1, #1765: a chat is one
  document now). A Signal, WhatsApp, Beeper, Messages
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
  `is:document` picks between them. Nobody types it: the grid opens
  with it on and Browse adds it, and without it the grid shows every
  message instead. Free text is ranked per document and then landed on
  one message.
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
"node" below, never a DAG step, and a row of the `nodes` table),
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
  a Notion page, a Drive file, a session. This is what today's display
  mode folds a search's blocks into, what the viewer opens, and what
  embedding cuts its windows from. Nested ones stay separate: a Notion subpage is read on
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

**A search finds blocks; a display mode draws them.** A search answers
with blocks. How they are drawn (each folded into what it is read
together in, today's one mode) is a view setting, not part of the query
(§"How search works", rule 6). Nothing about how results are drawn is
fixed at render, so there is no month to choose and no setting to
change.

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

## How search works

A search is a line a person typed: **keys** (`from:`, `source:`, a date)
and **free text**. Its answer is a list of rows, best first. This
section says what a search does with each part, first as it runs
today, then as it should run once blocks are the unit. Facts about
today were checked against the code and one real library on
2026-11-24.

### Today: three units that do not line up

A search with free text runs in six steps
(`applets/src/unified_grid_and_search_index/mod.rs`):

1. **The keys become a set of kept rows.** `matching_rows` lists every
   grid row the keys keep, documents and messages alike, and `kept_of`
   turns that into, per document, either *Whole* (the keys kept only the
   document's own row) or *these messages*. The set is cached per query
   (`Keyed`).
2. **Words rank documents.** The keyword index (`search_fts`, one row
   per **piece**, a stretch of a document's text) is matched three ways
   in turn: the words in the order typed, every word in one piece, and,
   for a page, every word somewhere among its pieces. Each document
   keeps its best piece, and the cut is the best 1,000 documents. The
   kept set from step 1 goes in as JSON lists of uuids.
3. **Meaning ranks documents.** The query is embedded and scored
   against every held chunk vector of the kept documents, each document
   at its best chunk, down to the `semantic:` cutoff.
4. **The two rankings are fused per document** by reciprocal rank.
5. **A page of the answer is landed.** Each document shown is opened
   on one of its rows (`GridIndex::land`): the row its hit's piece
   names, or the document's own row under `is:document`; when the keys
   dropped that row, the document's first row they keep. The hit is
   quoted from that row's text.
6. **The first page counts what meaning cannot reach**: the documents
   the keys keep that have no vectors, listed from the grid on every
   search.

So a search works in three units: keys keep **rows**, the index holds
**pieces**, and rankings order **documents**. Most of the code in the
list above converts between them, and every conversion has turned out
to be wrong somewhere:

- **A piece does not always name its row.** In one real library every
  piece of a chat is a message row, but a page source's pieces often
  are not. Drive's 307 files hold 7,382 pieces with no row between
  them, GitHub's 1,514 pull requests 1,655, and every calendar event,
  Drive file and pull request opens with a heading piece with no row.
  Notion's text and a GitHub description are named by the document's
  own row. Step 1's *these messages* holds neither, so
  a search with any key, `source:` included, lost every hit in such a
  page's text (#1978 patches this).
- **A document's row means two things.** It is the container (keep the
  document *whole*) and it carries facts of its own (a time, an
  author). Which one a kept row means is a guess `kept_of` makes, and
  it guesses differently depending on whether some message rows were
  kept beside it.
- **Lists of rows travel as data.** A key that keeps most of the
  library (`-source:` one big source) makes step 1 hold up to 1.2M
  uuids and hand them to SQL as JSON, and step 6 walks the grid once
  per search: 1.6 to 2 s each, measured on the search eval.
- **Landing exists only because ranking is per document.** It re-reads
  what the ranking already knew.

### The rules

**1. A block is the only thing search reads.** Every piece the search
index holds is a block: it names a row of its own, with an author, a
time and a body. Layout (front matter, a heading, a separator) is never
searched. A container's own words (its title, a thread's subject) reach
its blocks as the title each block is indexed under, which is the FTS
`title` column today. A render contract test holds every renderer to
it: every searched piece names a row of the same document.

Until each renderer names its text, the index files a piece with no row
under its document's row, so nothing goes unsearched while the
renderers catch up. That rule replaces #1978's `own_text`. Drive's file
text becomes a big block named by the file's row, as Notion's did in
5e; GitHub's pieces with no row are each looked at and become a block
or are marked layout.

**2. A key keeps blocks, never a container.** A key is a test on a
block's own facts and the facts of the containers above it. The index
step copies the container facts a key can ask about onto each block, as
it already copies the subject (`conversation_name`) onto every message
row, and the account where a source has one. So *Whole* goes, and with it the guess about what a kept document
row means:

- A date window keeps the blocks said in that window. A ten-year chat
  whose last message falls in the window ranks by that day's messages,
  by construction rather than by a special case.
- `from:x` keeps x's blocks: x's emails in a thread, x's comments on a
  file, a page's own text when x wrote it.
- `-word` drops the blocks that say it, as it does today per piece. A
  document drops out only when every one of its blocks says it.

A data node is found by its own facts as a row of its own (`is:channel`,
4a). Free text never reaches a node directly, only through its blocks.

**3. Keys and rankings meet in the index, not through lists.** The
keyword index lives in the grid index's file, and with rule 1 a piece's
uuid is its row's. So a words search is one statement: the FTS match
joined to `grid_rows`, with the keys' own SQL as its `WHERE`. No set of
kept rows is listed, cached or bound. Meaning is scored in memory, and
picks one of two ways by how much the keys keep (the grid's estimate
for the keys' SQL):

- **Few kept** (a person, a channel, a week): list the kept blocks
  through their index and score only the chunks that cover them.
- **Most kept** (no key, or `-source:` one source): score every chunk,
  then check the best candidates against the keys in one indexed
  statement, a batch at a time, until the depth is full or the ranking
  runs out.

The source is held with every chunk, so `source:` and `-source:` never
touch SQL.

**A words statement scores only what it keeps.** Today it cannot be
narrowed to the sources asked for. It scores every match in the library
(`WITH m AS MATERIALIZED (SELECT rowid, -bm25(…) FROM search_fts WHERE
search_fts MATCH ?)`), and only then joins each match to
`search_sections` and `search_documents` to learn its source and
document and drop the rest. The page pass runs one such statement per
word of the query, and a common word matches much of the library: on
the search eval it was 0.4 s a statement, and the sentence-length
queries spent about 5 s each on words alone. Two changes, neither of
which changes how pieces are numbered:

- **Filter before scoring.** The source and the keys join in the same
  statement as the match, not after a materialized list of every match,
  so `bm25` runs only on the matches kept. The full-text engine still
  reads each word's whole list; what goes is scoring and holding the
  rest.
- **The page pass asks once, not once per word.** All the words in one
  match, or a word whose list is far longer than the others' left to
  the rest to narrow, rather than a full-library statement for each.

A search on a library whose files do not fit in memory waits on page
reads: on one 16 GB laptop, with a 5.5 GB grid index and a 5 GB
inference cache, a search's SQL spent most of its time in `pread`. The
keyword index is not what fills that file. Of its 5.5 GB, `grid_rows`
is 1.3 GB and its twenty-odd indexes another 1.8 GB, while the
full-text index and the tables beside it (`search_sections`,
`search_documents`) are 0.5 GB. So what stays in memory is decided by
the grid and its indexes, which is a question of which indexes the grid
keeps and how a library's index is managed, not of the keyword index's
layout.

Not taken: packing a piece's source, document and block into the
full-text row number, which FTS5 could then seek by range. It would make
a scoped search skip the rest of the library, but it fixes a bit layout
up front, how many bits each level gets, that has to hold for every
library.

**4. Each ranking ranks blocks, on evidence from their neighbours.** A
hit is one block, but what finds it may be said around it: a reply that
says "thank you" is found by the words of the message it answers. So a
block is scored by its own text and, at a discount, by its neighbours
within what is read together. Meaning does this already, since a chunk
spans neighbouring messages and speaks for the kept blocks it covers,
which share its rank. Words do not yet: today a reply that says none
of the words is not found. Under this rule a block's words score also
counts, at a discount, the words of the blocks next to it. How wide the
window is and how big the discount is are the eval's to pick; its `ctx`
targets, replies that say little on their own, measure it (on one real
library, 50% found in the top 10 with meaning, none by words alone).

Each ranking keeps at most a few blocks of any one document before it
cuts at the depth, so one long chat cannot fill it; the eval picks how
many.

**5. Fusion is per block.** Reciprocal rank fuses the two rankings'
blocks, so a block both find comes first. Today words finding one
message of a thread and meaning finding another add up for the thread;
per block they do not. If the eval misses that, a block whose document
holds other hits gets a small lift, and nothing else changes.

**6. A search answers with blocks; a display mode draws them.** A hit
is a block, so nothing lands. The answer is the ranked list of blocks
and nothing else: with words, best first; with keys alone, the blocks
the keys keep in the order asked. A data node a key asks for by its own
facts (`is:channel`) is in the list as itself.

How the list is drawn is a **display mode**, a view setting beside sort
and grouping, never a key in the query. A mode names which ancestor each
block folds into, a pure function over the list using the column each
block already carries (§"The model"), so ranking, keys and caching
never know which mode is on. There is one mode for now:

- **By what is read together.** Each block folds into its nearest
  read-together data node: a thread, a chat, a page. A row is that node,
  at its first block in the list, showing the node's title, that
  block's author and time, and a snippet quoted from that block's body;
  its other blocks in the list fold under it in order ("4 more in this
  thread"). Opening the row opens the viewer on the node at that block,
  with the rest of it around it. Inside a big block (a Drive file) the
  snippet is found by the words, as it is today.

Other modes are the same function with another ancestor: none (a row
per block), the nearest place (a row per channel or mailbox), a day.
None is built until someone needs it. Paging counts the rows a mode
draws, so the server folds before it pages. Grouping the rows by a
column (by source, by channel), as the grid does today, stays the
grid's own choice over the rows it draws.

**7. Every step costs what its answer costs.** A step is bounded by the
rows it returns or by one index range, never by the size of the
library. No cache stands in for a step that is slow (#1965). The eval's
SQL audit (#1942) checks the rule: a query whose statements scan the
library fails it. What meaning cannot reach is counted per source,
from the search index's documents and the held vectors, once per index
commit, and the line above the results says so. It stops narrowing by
the search's other keys, which is what made it a walk of the grid.

### What this removes

`Kept` and `KeptRows` with their three lists; `Keyed` and its cache;
`land` and the file re-read behind it; `documents_with_pages` on every
search; `documents_of_hits`'s second look at the grid; `is:document`,
which only the UI sends, becoming the one display mode. What stays:
`page_words`, while a page's text is several blocks (rule 1 makes
Drive's one); the three ways words match; fusion, per block rather than
per document.

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
| `grid_rows.is_document`, `is:document` (grid default, Browse, dashboard, eval) | one row per document, or per message, picked by a key the UI adds | gone from the query: a search answers with blocks, and the view's display mode folds them into what each is read together in (§"How search works", rule 6). Only the UI sends `is:document`, so the UI asks for the mode instead and the key needs no alias |
| Search index (`search_documents`, `search_sections`, `search_chunks`) | keyed by document; sections are byte spans of the file | keyed by item; chunks name the items they cover and the group they were cut from |
| Ranking (`documents_of_hits`, `fuse`, `land`) | ranks documents, then lands each on a message by re-reading the file | ranks blocks and fuses them per block; a hit is a block, so there is nothing to land |
| `Keyed` / `Kept::Sections` | what the filters keep, per document, as lists of uuids | gone: keys are the `WHERE` of the words statement, and meaning checks its candidates in SQL (§"How search works", rule 3) |
| `WithoutVectors` | documents without vectors among those the keys keep, read from the grid on every search | per source, from the search index and the held vectors, once per index commit (rule 7) |
| Viewer (`/chat/{markdown_uuid}`, `DocCard`) | the whole file, cut into sections by byte length | the items of what is read together, a page at a time, opened at one |
| The route's name, `/chat/{markdown_uuid}` | serves any document (a page, a PDF, an event), named for the first sources, which were chats | `/data_node/{uuid}`, named for what it opens; `/chat` kept as an alias, since agents use it (`agent_user.md`) |
| Links (`datalib:doc/<page>#<section>`), copied 🆔 | a document and a message in it | an item, with what holds it looked up |
| Embedding map | one point per document (the mean of its chunks) | unchanged: one point per document, lit by the blocks a search finds in it |
| Problems banner | rows scoped to a document | problems name an item; a document shows its items' problems ([`problems_as_step_events.md`](problems_as_step_events.md)) |
| Feedback anchors (`data-section-uuid`) | a message inside a document | unchanged: already per item |
| Eval targets (`uuid`, `markdown_uuid`) | a hit anywhere in the document counts | a target keeps its `uuid` (an item, which does not move); its `markdown_uuid` is replaced by the uuid of what holds it, read together, by a script run against the root. The set stays private; "found" is scored per block and per document |
| Agents and Minds workspaces reading files (`docs/agent_user.md` §"Markdown", `rendered_documents.md`) | read `<group>/render_markdown/*.md` directly | read through the applet: `/chat/{uuid}` already returns a page's text, and a route listing a container's items takes over from the files; the docs say so in the step that stops writing them |
| `docs/user/searching.md`, `rendered_documents.md`, `agent_user.md` | describe documents | rewritten with the step that changes each |

There are few enough users that links people have saved need no forward
path. A query a person types does; `is:document` is not one, since only
the UI sends it.

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
     block and stretch of layout of a document in order, keyed or not.
     chat-common hands each item's body and who it was addressed to, and
     marks its layout as frame, which is not kept; every other renderer's
     sections are kept whole and marked `framed`, unkeyed ones too (a PDF's text is
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
4. **Search finds blocks.** In two parts:
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
   - **4b. Ranking blocks, shown in their documents**, as §"How search
     works" lays out, in three parts, each checked against the eval:
     - **Every searched piece names a block** (rule 1): the render
       contract test, the index filing a piece with no row under its
       document's row, and Drive and GitHub naming their text.
     - **Keys keep blocks, and meet the rankings in the index** (rules 2,
       3 and 7): container facts copied onto blocks, the words statement
       joined to the grid and scoring only what it keeps, the page pass
       asking once, meaning's two ways, the count per source. `Kept`, `Keyed` and the
       per-search walk of the grid go.
     - **Blocks rank and fuse; one display mode draws them** (rules 4
       to 6): each ranking ranks blocks, words counting a block's
       neighbours at a discount as meaning's chunks already do, a few
       blocks per document at most;
       fusion is per block, and nothing lands. The view's one display
       mode folds the blocks into what each is read together in, and
       the UI asks for it in place of sending `is:document`. The eval
       scores a target per block and per document, and compares the
       answer with today's.
5. **The viewer reads items, and the `.md` files go.** Only two things
   read the files' bytes: the viewer's `/chat/{markdown_uuid}` and search
   landing (`hits/mapping.rs`), which quotes a hit and picks its row from
   the file. In order:
   - **5a. The store spells every document.** `sections` keeps a
     chat-common item's body alone and none of its layout, so
     chat-common's fourteen sources could not be read back from the
     store. A `drawings` table keeps the rest as drawn: each block's
     drawing with its body cut out and where it goes back, and each
     stretch of layout whole. The store then spells every document byte
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
     commit the grid index read (`source_cursors`), so the text matches
     the rows the grid shows; a render the index has not read yet shows
     once it has.
   - **5d.** `datalib-step markdown <root>` prints one document
     (`--source`, `--document`) or writes every one as
     `<folder>/<source>/<id>.md` (`--out`), as its render store spells
     it, at a commit with `--at`. It is the way out the README and the
     first-time guide promise; tests and fixtures read documents through
     it or the store.
   - **5e.** Render stops writing the files, and the ones an earlier
     build wrote are deleted the next time a source renders, each render
     store kept. `markdowns` loses `md_path` and `section_layout`; a
     renderer says `searchable` itself. The render store's shape
     changes, so with it, at no extra re-render: `drawn_pieces` and
     `SpelledPiece` are renamed `drawings` and `SpelledSection`, and
     Notion hands a page's text as a big block named by the page's row,
     its front matter as layout. Landed in #1879. Paging through a
     document a page at a time followed in #1916
     ([`completed/document_viewer_from_blocks.md`](completed/document_viewer_from_blocks.md)
     §"Step 6").
6. **No time buckets.** Landed with step 1 (#1765): a bucketed source
   renders one container per chat, and the `period` setting is gone.
7. **Embedding context.** Answered another way (#1786): the eval found
   that a chunk holding many messages scored lower against a query
   about one of them than that message's own chunk did, so every
   message is a chunk of its own and no window is cut
   (`search_index/src/chunk.rs`). Context from a block's neighbours is
   rule 4 of §"How search works".
8. **Browsing the tree.** Built so far (#1913): a Library card that
   draws each source's places as a tree, read a level at a time when
   opened, with what a place holds listed beside it. Not built: the
   ancestors table in `unified_grid_and_search_index`, with
   each row's sort key copied in so everything under a node is one
   index range around an anchor; and `under:` and `at:` as search keys.
   Whether a large subtree pages off that index or by
   the grid's list-then-slice (measured at 74k rows, not at today's
   941k) is measured then. `keyword:` and
   collections inside sets become `under:` their node, with the old keys
   kept as aliases. The library root, the source nodes and the folders
   above them come here too, with the `parent` config field and moving a
   source on the Manage screen.
9. **The rest of the table above:** the map, links, the problems banner
   with the problems plan, the docs.
10. **Each block keeps its body in the format it came in.** Today every
    body is turned into markdown at render. For some sources that is
    what the text already is (a Claude or ChatGPT message, a Notion
    page), and for a PDF it is a good target. For email it is a lossy
    conversion: each HTML part goes through `htmd`
    (`email_render/src/render/render.rs`), with handlers to repair what
    it gets wrong. A mail reader like Thunderbird does no such thing: it
    draws the sender's HTML in a locked-down view (no scripts, no remote
    content) and offers simplified HTML or plain text as fallbacks.
    Since 5e a block's body is a row of its own, so the format can be
    a column beside it:
    - **What is stored.** `sections` gains the body's format: markdown,
      HTML or plain text. Email keeps its HTML part as sent; chat sources
      whose messages are plain text say so rather than escaping them into
      markdown; markdown sources stay as they are.
    - **What draws it.** The viewer draws each block by its format: markdown
      through markdown-it as now, HTML through the sanitizer
      (`ui/src/cards/sanitize.ts`) on its allowlist, text as text. Each
      block is still a fragment of its own, so one cannot reach into the
      next.
    - **What search reads.** Search draws words from each format with its
      own extractor (`search_index/src/text.rs` does markdown today).
      Changing the extractor changes the indexed text, so it is an
      `INDEX_TEXT_VERSION` bump.
    - **What an export writes.** `datalib-step markdown` still writes
      markdown, converting an HTML block on the way out, or writes it as
      HTML. Which one is to decide; nothing else depends on it.

    Email goes first, since it loses the most. It costs a re-render of
    every email source and the two new drawing paths; the
    `htmd` repairs go.

## Open questions

- **What is read together.** Today each kind of container says so once,
  at render: a thread, a chat, a page. Whether a Slack channel's
  messages are read together depends on how the channel is used: a
  thready channel's threads are, and a flat channel's run of messages
  may not be. Step 4 (what the display mode folds into) and rule 4's
  neighbours lean on the answer. Until it is settled, the display mode
  folds by document.
- **How wide an embedding window is.** Answered (#1786): one message,
  no window (step 7).
- **How blocks rank against their documents.** §"How search works"
  fuses per block (rule 5). How many blocks of one document a ranking
  keeps before its cut, and whether a block whose document holds other
  hits should be lifted, is the eval's to say; a long chat must not win
  by being long.
- **What a folded row shows** when its document spans years: its other
  hits' times, the document's first and last time, the number of hits
  folded under it.
- **Why a row matched.** A row could say what found it: the words it
  holds, a key it passed, or "by meaning", and for a neighbour's words,
  which message said them. A miss is easier to trust, and a query easier
  to fix, when a person can see why the rows above it won.
- **Ranking by more than the text.** A person searching their own data
  is mostly recalling something they saw, and they remember who, when
  and where better than the words. Today nothing but the text ranks a
  block: no recency, no weight for blocks the person wrote or was sent,
  none for the source. How such a prior joins reciprocal rank fusion,
  and whether it helps, is the eval's to say, scored separately for
  queries that name a time or a person and those that do not.
- **How deep each source's tree goes.** A Notion page is one row today,
  with none for its blocks: a big block. Making blocks items lets a hit
  land on a paragraph and a page be windowed for embedding, at the cost
  of many more rows; Drive's paragraphs are the same question. Notion
  is the place to start: its renderer walks a page block by block and
  holds every block's id as it draws. Ranking blocks makes this more than
  a question of where a hit lands: a 40-page file as one big block has
  many chunks, so many chances at a high score by meaning, against a
  one-line message's one. Until big blocks are split, the eval reports
  its scores by the size of the target's block, so that bias shows.
  Which kinds are
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
- **Reactions.** Answered (#1925): an attribute of the message they
  react to (`sections.reactions`), each still keeping its own grid row
  and anchor.
- **Time-of-day bucketing** for display ("this conversation on 3 May")
  is a display mode that folds into a day, and needs nothing stored.
