# Moving from documents to blocks: search finds blocks, shown in their documents

**Status: partly built, checked against the tree on 2026-11-27.**
Built: the tree of data nodes and reply-to edges (steps 1 and 6,
#1765); each block's body in the render store, and search reading it
from there (3a to 3c, #1775, #1782, #1818); the viewer reading the
store, with no `.md` files written (step 5, #1868 to #1879, paging in
#1916); every data node a grid row (the first part of 4a, #1863,
settled one row per node in #1998); and the Library card with
`at:<node>` and `GET /places` (part of 8: #1913, #2002, #2007). Step 7
was answered another way: a message is a chunk of its own (#1786).
Step 2's route and index were built (#1770) and removed again (#2007),
since nothing read them. Of step 4b only rule 1's interim is in
(#1978), and the count rule 7 wanted moved was removed instead (#2021).

**Still open:** the rest of 4a (a path on each data node, `is:<kind>`),
the rest of 4b (rules 1 to 6: blocks ranked and fused, keys met in the
index, one display mode in place of `is:document`), the rest of 8
(`under:`, the library, source and folder nodes), 9 and 10.

How the built parts work now is in the reference docs: the tree in
[`grid_rows.md`](../grid_rows.md) §"`parent_uuid`, `sort_key`,
`read_uuid`, `place_uuid`: where a row sits", edges in
[`edges.md`](../edges.md), the render store's `sections` and
`drawings` in
[`data_architecture_parse_and_render.md`](../data_architecture_parse_and_render.md),
and search as it runs today in [`search.md`](../search.md). This file
keeps what is still to do and the decisions behind it; what each landed
step said is in git.

## Words

| word | means |
|---|---|
| **block** | content: one thing a person or program wrote, with one author, one time and a body (a message, an email, a comment, an event, a tool call, a page's own text). What search ranks and a hit lands on. A `grid_rows` row; the text below and the search index's code call it an **item**. A page source's whole page (a Notion page, a calendar event, a contact) is one **big block** for now, which its renderer may later split into the blocks it is made of. |
| **layout** | what is drawn around blocks and says nothing of its own: front matter, the title, a `<details>` opener. The code calls a block and a stretch of layout alike a section (`Section`, the render store's `sections`). |
| **data node** | a node of the tree that holds blocks and other data nodes: a chat, a thread, a channel, a label, a page, a session. It often has a name and other metadata (its kind, its upstream id), and most of what it says is in the blocks it holds. A `nodes` row. Many data nodes are **places**: ones a person browses or filters by, such as a channel, a mailbox or a workspace. |
| **document** | a group of blocks that are often read together: a thread, a chat, a page. Rows of its source's render store. |

The text below also calls a container **read together**. When that holds
is not settled; see §"Open questions".

## The problem

Every record Datalib rendered used to go into a markdown file, and the
file was the unit everything else was built on, because qmd could only
index files. Three of the costs that brought are gone: a chat is one
document rather than one a month (#1765), each row names its parent and
its replies are edges (step 1), and nothing is written to disk but the
stores and the blobs (step 5). One is left:

- **A search has to say what it wants rows to be.** The grid holds two
  kinds of row in one table: one row per document, which has no single
  sender and no single time, and one row per message, which does.
  `is:document` picks between them. The grid opens with it on and
  Browse adds it, and without it the grid shows every message instead.
  Free text is ranked per document and then landed on one message.

§"How search works" says why that is costly and what replaces it.

## The model

The tree is built: every block and data node names one parent and how
it sorts among its siblings, by a key that never renumbers; a data node
may also be filed under others (an email thread under its labels); each
row carries its nearest container that is read together and its
nearest place; and a container keeps its upstream id. The reference is
[`grid_rows.md`](../grid_rows.md) §"`parent_uuid`, …". The decisions it
rests on, which a later step must keep:

- **What a container is for is declared by its kind**, never discovered
  from its depth. A kind is read together (a Slack thread, an email
  thread, a WhatsApp chat, a Notion page, a Drive file, a session), a
  place (a Slack channel, a mailbox, a Notion workspace, a Drive folder,
  a Claude Code project), or both (a WhatsApp chat). Nested ones stay
  separate: a Notion subpage is read on its own, not as part of its
  parent.
- **Contained by is one parent; filed under is any number.** Removing a
  node's container removes the node. A label, folder, keyword or
  collection a node is filed under is a container itself, with its own
  parent.
- **Everything else is an edge, not a parent.** Reply-to (a Signal
  quote, an email's `In-Reply-To`, a GitHub review reply), a mention, a
  link: a message that quotes one from last year stays where it was
  said ([`edges.md`](../edges.md)).

**Store the body, format on read.** What render stores for an item is
its content, not text laid out for one reader:

- **The facts** a header is drawn from (author, time, handles, reply-to,
  edited) are columns. Since #1925 a chat message is kept as facts and
  drawn from them when it is read (`sections.reactions` and the rest).
- **The body** is stored once, in one normalized form of ours: markdown
  with our link forms for a person, a date and an attachment. Turning
  upstream's own form (Slack's markup, an email's HTML, Notion's blocks)
  into it is the provider's work and stays at render. Step 10 revisits
  this for sources whose own form is lost in the conversion.

Everything around the body (an item's header line, a group's title and
who took part, the separators) is drawn when something reads it, by one
function per output:

- **Markdown**, for the viewer and an export, and **HTML** later. These
  may change as often as anyone likes; a change costs nothing stored.
  The promises in [`rendered_documents.md`](../../user/rendered_documents.md)
  (the link a person is, the `<time>` a date is, `data-section-uuid`)
  are what these functions draw, not bytes on disk.
- **Index text**, what a chunk is embedded and keyword-indexed from:
  `search_index/src/text.rs`, versioned by `INDEX_TEXT_VERSION`
  (step 3b).

**A view per kind of node.** Not built. Opening a node shows it the way
its kind is read: a message in its context, a thread's items a page at
a time, a channel's threads to browse. A shared layer supplies the
views for everything that goes through it (chat-common for every chat
source's message, thread and channel), and a provider or an applet adds
one for a kind of its own. That is a registry keyed on the node's kind,
which is what [`dispatch_on_kind.md`](dispatch_on_kind.md) proposes for
chips and cards. So the kinds become a small closed set (channel,
thread, chat, mailbox, label, folder, page, calendar, project,
collection, …) designed with that registry, beside the free-text `kind`
a person reads, and `/chat` gives way to it.

## Browsing the tree

Built: the Library card draws each source's places as a tree, a level
at a time (`GET /places`, #2007), and a clicked place shows the search
list held to `at:<node>`, what is directly in it (#2002). Still to come
is everything that needs a node's whole subtree, and the nodes above
the sources.

- **Held to a branch.** `under:<node>` holds the grid and every search
  to everything beneath a node, as `at:<node>` holds it to what is
  directly in one. A node is a chip like a person, so it is clicked or
  typed, never spelled out. `source:` becomes `under:` the source's
  node, which it already means.

**Above the sources: the library and its folders.** A source's top
containers (an email account, a Slack workspace, a chat with no account
above it) have no parent today. They get one:

- **A source is a node.** It is the source's group: its uuid is minted
  from the group id, and its children are the source's top containers.
  Renderers do not mint it; they keep naming no parent at their top,
  and `unified_grid_and_search_index`, which knows each row's source,
  reads that as the source node. A step is not a node: it is how a
  source's data was made, and it lives in the DAG. A problem's `stage`
  says which step raised it.
- **A folder of sources is a group too**, one with no type and no
  steps, and a group names the folder it is in with one optional config
  field (`parent = "<group id>"`). A person arranges their sources into
  folders on the Manage screen. Moving a source never changes its id,
  which is its directory on disk; only its parent moves.
- **The library is the one root**, above every folder and source.

The arrangement belongs to the person, not to any source, so a move
costs one row: the source-to-folder links are read from the config into
a small table of their own, and no row of any source is rewritten,
re-rendered or re-indexed when a source moves. Every node but the
library then has a parent, so a problem rolls up through its
containers, its source and the person's folders to the library, and
"how many problems under X" is the same lookup as "how many items under
X".

**How "under" stays fast.** `unified_grid_and_search_index` keeps one
table of every node's ancestors, `(node, ancestor, depth)`, following
both kinds of parent, so "under X" is one indexed lookup however deep X
is and whichever way a node reached it. A message under a nested label
is found from the label and from every label above it, with no walk up
the tree at search time and no special case for labels, keywords or
folders. Its size is the sum of every node's depth: a few million rows
for a root of a million items about four levels deep. Step 4a's path on
each data node is the other way to get the same prefix range; step 8
picks one.

## How search works

A search is a line a person typed: **keys** (`from:`, `source:`, a date)
and **free text**. Its answer is a list of rows, best first. How it runs
today, step by step, is [`search.md`](../search.md) §"What a search
does". This section says what is wrong with that and how it should run
once blocks are the unit.

### Today: three units that do not line up

Keys keep **rows** (`keyed`, `kept_of`), the keyword index holds
**pieces** (a stretch of a document's text), and the rankings order
**documents**, which are then landed on a row (`land`). Most of the
search code converts between them, and every conversion has turned out
to be wrong somewhere (facts checked against one real library on
2026-11-24):

- **A piece does not always name its row.** Every piece of a chat is a
  message row, but a page source's pieces often are not. Drive's 307
  files held 7,382 pieces with no row between them, GitHub's 1,514 pull
  requests 1,655, and every calendar event, Drive file and pull request
  opens with a heading piece with no row. Notion's text and a GitHub
  description are named by the document's own row. A search with any
  key lost every hit in such a page's text until #1978 counted a piece
  with no row as its document's row.
- **A document's row means two things.** It is the container (keep the
  document *whole*) and it carries facts of its own (a time, an
  author). Which one a kept row means is a guess `kept_of` makes, and
  it guesses differently depending on whether some message rows were
  kept beside it.
- **Lists of rows travel as data.** A key that keeps most of the
  library (`-source:` one big source) makes the first step hold up to
  1.2M uuids and hand them to SQL as JSON.
- **Landing exists only because ranking is per document.** It re-reads
  what the ranking already knew.

### The rules

**1. A block is the only thing search reads.** Every piece the search
index holds is a block: it names a row of its own, with an author, a
time and a body. Layout (front matter, a heading, a separator) is never
searched. A container's own words (its title, a thread's subject) reach
its blocks as the title each block is indexed under, which is the FTS
`title` column today. A render contract test holds every renderer to
it: every searched piece names a row of the same document. Not built.

Until each renderer names its text, a piece with no row counts as its
document's row, so nothing goes unsearched while the renderers catch
up. That is built (#1978). Still to do: the contract test; Drive's file
text becomes a big block named by the file's row, as Notion's did in
5e; GitHub's pieces with no row are each looked at and become a block
or are marked layout.

**2. A key keeps blocks, never a container.** A key is a test on a
block's own facts and the facts of the containers above it. The index
step copies the container facts a key can ask about onto each block, as
it already copies the subject (`conversation_name`) onto every message
row, and the account where a source has one. So *Whole* goes, and with
it the guess about what a kept document row means:

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
search_fts MATCH ?)` in `search_index/src/read.rs`), and only then joins
each match to `search_sections` and `search_documents` to learn its
source and document and drop the rest. The page pass runs one such
statement per word of the query, and a common word matches much of the
library: on the search eval it was 0.4 s a statement, and the
sentence-length queries spent about 5 s each on words alone. Two
changes, neither of which changes how pieces are numbered:

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
the grid and its indexes
([`index_management.md`](index_management.md)), not by the keyword
index's layout.

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
block already carries (`read_uuid`, `place_uuid`), so ranking, keys and
caching never know which mode is on. There is one mode for now:

- **By what is read together.** Each block folds into its nearest
  read-together data node: a thread, a chat, a page. A row is that node,
  at its first block in the list, showing the node's title, that
  block's author and time, and a snippet quoted from that block's body;
  its other blocks in the list fold under it in order ("4 more in this
  thread"). Opening the row opens the viewer on the node at that block,
  with the rest of it around it. Inside a big block (a Drive file) the
  snippet is found by the words, as it is today.

Other modes are the same function with another ancestor: none (a row
per block), the nearest place (a row per channel or mailbox), a day
("this conversation on 3 May"). None is built until someone needs it.
Paging counts the rows a mode draws, so the server folds before it
pages. Grouping the rows by a column (by source, by channel), as the
grid does today, stays the grid's own choice over the rows it draws.

**7. Every step costs what its answer costs.** A step is bounded by the
rows it returns or by one index range, never by the size of the
library. No cache stands in for a step that is slow (#1965). The eval's
SQL audit (#1942) checks the rule: a query whose statements scan the
library fails it. The count of what meaning cannot reach, which walked
the grid on every search, is gone (#2021); a source's figures are on
its Manage row. If a count comes back above the results, it is per
source, from the search index's documents and the held vectors, once
per index commit, and no answer waits for it.

### What this removes

`Kept` and `KeptRows` with their lists; `Keyed` and its cache; `land`
and the re-read behind it; `documents_of_hits`'s second look at the
grid; `is:document`, becoming the one display mode. What stays:
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

Every place that still assumes a document, from the read side of the
tree:

| Where | Today | After |
|---|---|---|
| `grid_rows.is_document`, `is:document` (grid default, Browse, dashboard, eval) | one row per document, or per message, picked by a key | gone from the query: a search answers with blocks, and the view's display mode folds them into what each is read together in (rule 6). `searching.md`, `applets.md` and `agent_user.md` now teach `is:document`, so people and agents type it: it keeps an alias that asks for the display mode |
| Search index (`search_documents`, `search_sections`, the chunks) | keyed by document | keyed by item; a chunk names the items it covers and what they are read together in |
| Ranking (`documents_of_hits`, `fuse`, `land`) | ranks documents, then lands each on a row | ranks blocks and fuses them per block; a hit is a block, so there is nothing to land |
| `Keyed` / `Kept` | what the keys keep, per document, as lists of uuids | gone: keys are the `WHERE` of the words statement, and meaning checks its candidates in SQL (rule 3) |
| The route's name, `/chat/{markdown_uuid}` | serves any document (a page, a PDF, an event), named for the first sources, which were chats | `/data_node/{uuid}`, named for what it opens; `/chat` kept as an alias, since agents use it (`agent_user.md`) |
| Links (`datalib:doc/<page>#<section>`), copied 🆔 | a document and a message in it | an item, with what holds it looked up |
| Embedding map | one point per document (the mean of its chunks) | unchanged: one point per document, lit by the blocks a search finds in it |
| Problems banner | rows scoped to a document | problems name an item; a document shows its items' problems ([`problems_as_step_events.md`](problems_as_step_events.md)) |
| Eval targets (`uuid`, `markdown_uuid`) | a hit anywhere in the document counts | a target keeps its `uuid` (an item, which does not move); its `markdown_uuid` is replaced by the uuid of what holds it, read together, by a script run against the root. The set stays private; "found" is scored per block and per document |
| `docs/user/searching.md`, `rendered_documents.md`, `agent_user.md` | describe documents | rewritten with the step that changes each |

There are few enough users that links people have saved need no forward
path. A query a person types does.

## Forward path for existing data

Everything the open steps change is derived: render stores, the grid,
the search index. They are rebuilt from the raw stores. The vectors in
the inference cache are keyed by their prompt's hash, so a chunk whose
text is unchanged keeps its vector; chunks cut differently are embedded
again. Step 8's `parent` config field is optional, so an existing config
loads unchanged. `is:document` keeps an alias (above).

## Order of work

Each step lands on its own, leaves search working, and is checked
against the eval before the next.

1. **The tree.** Landed (#1765): every container a row of its own, each
   node's parent, filings, sort key and nearest read-together and place
   ancestors; reply-to as edges.
2. **Children in order.** Landed as #1770 and removed in #2007: the
   viewer came to page by a piece's place in its document (#1916) and
   the Library by places (`GET /places`), so nothing read the route or
   its index.
3. **The body in the render store.** Landed. 3a (#1775): a `sections`
   table of every block and stretch of layout, and front matter as
   facts (`Front`). 3c (#1818): `keyword_index` folded into the index
   step, a source left out of search by `params.not_searched`
   ([`config_model.md`](../config_model.md)), the aggregator gone.
   - **3b. Index text from the store** (#1782), which
     `search_index/src/text.rs` points at. The search index draws each
     item's index text from its body and its row: one plain envelope
     line (who, to whom where the item has its own recipients, when, as
     `Tue 3 Mar 2026 14:05` so a weekday or a month is a word to meet),
     its labels, then its body; no URLs, a chip as its name. A chunk
     that starts inside an item repeats its envelope line. The text is
     never stored: a chunk keeps its hash and its vector is filed under
     it, so a chunk re-embeds exactly when its text changes. So the
     drawing must be deterministic (a date in the item's own offset,
     never the machine's clock), and `INDEX_TEXT_VERSION` moves with
     every change to it, which the golden test
     `the_golden_draw_is_this_versions` enforces. A move costs a
     re-embed, never a re-render or a re-download.
4. **Search finds blocks.** In two parts:
   - **4a. Every data node is a row.** Landed (#1863, #1998): the index
     step writes each data node that is not already a document's row
     one row from `nodes`, with no `markdown_uuid`
     ([`grid_rows.md`](../grid_rows.md) §"`parent_uuid`, …"). In one
     real library 60,395 of 63,976 data nodes already were a document's
     row, and the 590 that were not were nearly all places (Slack
     channels, email labels and accounts, calendars, repositories).
     Still to come in 4a:
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
       contract test, and Drive and GitHub naming their text. The
       interim, a piece with no row counting as its document's row, is
       in (#1978).
     - **Keys keep blocks, and meet the rankings in the index** (rules 2,
       3 and 7): container facts copied onto blocks, the words statement
       joined to the grid and scoring only what it keeps, the page pass
       asking once, meaning's two ways. `Kept` and `Keyed` go.
     - **Blocks rank and fuse; one display mode draws them** (rules 4
       to 6): each ranking ranks blocks, words counting a block's
       neighbours at a discount as meaning's chunks already do, a few
       blocks per document at most; fusion is per block, and nothing
       lands. The view's one display mode folds the blocks into what
       each is read together in, and the UI asks for it in place of
       sending `is:document`. The eval scores a target per block and
       per document, and compares the answer with today's.
5. **The viewer reads items, and the `.md` files go.** Landed (#1868,
   #1872, #1876, #1879; paging in #1916,
   [`completed/document_viewer_from_blocks.md`](completed/document_viewer_from_blocks.md)).
   The store spells every document; `datalib-step markdown` is the way
   out.
6. **No time buckets.** Landed with step 1 (#1765); the `period`
   setting is retired.
7. **Embedding context.** Answered another way (#1786): a chunk holding
   many messages scored lower against a query about one of them than
   that message's own chunk did, so every message is a chunk of its own
   and no window is cut (`search_index/src/chunk.rs`). Context from a
   block's neighbours is rule 4.
8. **Browsing the tree.** Built: the Library card (#1913), one `nodes`
   row per data node (#1998), `at:<node>` (#2002) and `GET /places`
   (#2007). Not built: `under:`, everything beneath a node, with the
   ancestors table in `unified_grid_and_search_index` (or 4a's path),
   each row's sort key copied in so that it is one index range around
   an anchor. Whether a large subtree pages off that index or by the
   grid's list-then-slice (measured at 74k rows, not at today's 1.2M)
   is measured then. `keyword:` and collections inside sets become
   `under:` their node, with the old keys kept as aliases. The library
   root, the source nodes and the folders above them come here too,
   with the `parent` config field and moving a source on the Manage
   screen.
9. **The rest of the table above:** the route's name, links, the map,
   the problems banner with the problems plan, the docs.
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
  Which kinds are read together and which are places is each source's
  to declare, checked source by source.
- **Counts on the tree card.** "How many items under X" for a node with
  half a million under it is a count over the ancestors table; it may
  need keeping per node at `unified_grid_and_search_index` time rather
  than counting when a level opens.
- **Which upstream hierarchies to bring in.** Email labels and
  mailboxes, Lightroom's keywords and collections, Slack channels,
  Notion pages and Drive folders (#1919) reach the tree or the search.
  Apple Mail's mailboxes and a Claude Code project are still to check.
- **Old versions of every body.** Doltlite keeps every version a commit
  reached ([`doltlite.md`](../doltlite.md) §"What a write costs"), so a
  render-version bump that changes every body keeps the old copy too,
  until `--compact` folds the history. If that is too large, a bump can
  write the new text on a fresh branch, move `main` to it and drop the
  old branch's history, so two full copies never stay.
