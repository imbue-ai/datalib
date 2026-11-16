# Messages, not documents: grouping chosen when a search runs

**Status: proposal (2026-11-16). Nothing here is built.** Facts about
the tree were checked against the code that day; file references are
to that tree.

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
- `qmd_path`, the `unindexed/` directory convention, and unlinking an
  old file when its path moves or its document empties;
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
page, an event, a tool call. It is one row, and its text is stored with
the row. Everything is keyed by the item's uuid, which already exists
and does not move.

**Containment is a tree of any depth.** Each source's things nest
differently, and no fixed set of levels fits them all:

| Source | From the top down |
|---|---|
| Slack | workspace › channel › thread › reply |
| Email | account › thread › message |
| WhatsApp, Signal, Messenger | chat › message |
| Notion | workspace › page › page › … › block; a discussion on a block |
| Google Drive | folder › … › file › comment thread › reply |
| Claude Code | project › session › sub-agent session › message |

So every node (an item, or a container that holds items) names **one
parent and how it sorts among its siblings**, and that is all the tree
stores. A container is a row too, with a uuid a person can open; some
containers have text of their own (a Notion page, a Slack thread's
first message) and some have none (an email thread). "The children of
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

**Everything that is not containment is an edge.** Reply-to (a Signal
quote, an email's `In-Reply-To`, a GitHub review reply), a mention, a
link, a label on an email: each is a relation between two nodes, not a
parent, so a message that quotes one from last year stays where it was
said.

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

**What is read together is read in pages, not as a file.** The viewer
opens it at an item and asks for the items around it in order. A
ten-year group chat opens as fast as a short one.

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
that step, as the aggregator deleted qmd's index.

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
   fills each node's parent, its sort key among its siblings, and its
   nearest ancestor that is read together and that is a place; reply-to
   and the other relations go in as edges. Containers' upstream values
   come from the ingest crate's recipe. The few sections that are not
   their row today become rows or lose their anchor: GitHub's and
   GitLab's description (keyed by the document row), Notion's table of
   contents and Perseus's sentence spans (no rows); everywhere else a
   section's uuid is already its row's
   ([`problems_as_step_events.md`](problems_as_step_events.md)
   §"What the tree does today"). `grid_index`
   carries the columns and an index on `(parent, sort key, uuid)`. A re-render
   and a re-index; nothing a person sees changes. Fixes the
   `conversation_uuid` docs on the way.
2. **Children in order.** One route that lists a container's
   children in order, a page at a time. The viewer can use it
   beside the file.
3. **Text in the render store.** Each item's rendered text is stored
   with its row. The search index reads items from the store instead of
   cutting files, and `keyword_index` folds into `grid_index`, which
   already reads every render store. Leaving a source out of search
   must survive the fold: today it is a source with no `keyword_index`
   step (`datalib_step/src/append.rs`), and `embed` stays the per-source
   switch for meaning. After the fold it is the source's absence from
   the aggregator's inputs, which already drops any source it does not
   name, and the config upgrade rewrites every existing root's
   `keyword_index` steps into that.
4. **Grouping when a search runs.** Search ranks items and groups them
   by the grouping asked for. `is:document` becomes an alias. The eval
   compares the grouped answer with today's.
5. **The viewer reads items.** `DocCard` opens what is read together at an
   item and pages through it. The `.md` files stop being written.
6. **No time buckets.** A bucketed source renders one container per
   chat. The `period` setting goes.
7. **Embedding context.** Chunks are cut from windows of what is read
   together, and the eval picks the window.
8. **The rest of the table above:** the map, links, the problems banner
   with the problems plan, the docs.

## Open questions

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
- **One parent is enough?** A Drive file could once sit in several
  folders, and an email carries several labels. Labels are edges, not
  parents; a second folder would be too. Nothing found so far needs
  two parents in the tree.
- **What storing the text costs.** The text goes in the render store,
  versioned with its row, so a reader at a commit sees that commit's
  text. On one real root (186k files) the markdown is 1.25 GB and the
  render stores already 9.6 GB. Doltlite keeps every version a commit
  reached and does not compress
  ([`doltlite.md`](../doltlite.md) §"What a write costs"), so a
  re-render that changes every item's text (a render-version bump)
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
