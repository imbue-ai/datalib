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

**An item names its parents, and its children can be listed in
order.** Three relations, each a plain column:

| Relation | Meaning | Examples |
|---|---|---|
| conversation | the place the item was said; it never ends | a chat (keyed on the chat, not who is in it: a group keeps its id as people join and leave), a Slack channel, an email thread, a Claude conversation, a Notion page with its comments |
| thread | a run of items read together | a Slack thread, a Google Chat topic, a Drive or Notion discussion; and where upstream has no threads, the whole conversation |
| reply to | the one item this one answers, where upstream says | a Signal or Beeper quote-reply, an email's `In-Reply-To`, a GitHub review reply |

**Every item has a thread.** A WhatsApp, Signal, Messenger or LinkedIn
chat with one person has no threads inside it: it is one long thread,
and its thread is the conversation itself. So conversation and thread
are always set, and a source with threads differs only in having more
than one per conversation. Grouping by thread then means the same thing
for every source. Reply-to is the one relation that can be empty,
because it is a fact about one item, not a grouping.

**An item keeps the upstream record it came from**, as
`upstream_entity_kind` and `upstream_id`, the pair `grid_rows` already
carries. A problem names what its step was holding when it arose (that
pair, an item uuid or a path) and never a document or a parent, and is
linked to its item when it is read
([`problems_as_step_events.md`](problems_as_step_events.md)). So the
pair must reach the stored item, and the item's parents must be
reachable from it.

A conversation and a thread are rows too, each with its own uuid, so a
parent column points at something a person can open. Each item carries
its position in its conversation and in its thread, so "the children of
X, in order" is one indexed query on `(parent, position)` and needs no
stored list.

**A group is chosen when a search runs, not when render runs.** A
search finds items. How its results are shown is a separate choice:
one row per item, per thread, or per conversation. A group's row is
derived from its items: who took part, its first and last time, how
many matched. Nothing about a group is fixed at render, so there is no
month to choose and no setting to change.

**What gets embedded is its own choice.** One message embedded alone
has lost its context; that is why chunks span messages today. So
embedding still needs a context: a window of nearby items in the
thread. A Slack thread usually fits in one window; a ten-year chat,
being one long thread, is many windows. How wide a window is and
whether windows overlap is decided by the eval, and changing it
re-embeds without changing anything a person sees.

**A conversation is read in pages, not as a file.** The viewer opens a
conversation at an item and asks for the items around it by position.
A ten-year group chat is one conversation and opens as fast as a short
one.

## What changes, and for whom

Every place that assumes a document, from the read side of the tree:

| Where | Today | After |
|---|---|---|
| `grid_rows.is_document`, `is:document` (grid default, Browse, dashboard, eval) | one row per document, or per message | `group:conversation`, `group:thread`, `group:none`; `is:document` kept as an alias for the first, since people type it |
| Search index (`search_documents`, `search_sections`, `search_chunks`) | keyed by document; sections are byte spans of the file | keyed by item; chunks name the items they cover and the group they were cut from |
| Ranking (`documents_of_hits`, `fuse`, `land`) | ranks documents, then lands each on a message by re-reading the file | ranks items, then groups them; a hit is an item, so there is nothing to land |
| `Keyed` / `Kept::Sections` | what the filters keep, per document | what the filters keep is a set of items |
| `WithoutVectors` | documents without vectors | items not covered by any embedded chunk |
| Viewer (`/chat/{markdown_uuid}`, `DocCard`) | the whole file, cut into sections by byte length | a conversation's items, a page at a time, opened at one |
| Links (`datalib:doc/<page>#<section>`), copied 🆔 | a document and a message in it | an item, with its conversation looked up |
| Embedding map | one point per document (the mean of its chunks) | one point per group at the grouping chosen |
| Problems banner | rows scoped to a document | problems name an item; a group shows its items' problems ([`problems_as_step_events.md`](problems_as_step_events.md)) |
| Feedback anchors (`data-section-uuid`) | a message inside a document | unchanged: already per item |
| Eval targets (`uuid`, `markdown_uuid`) | a hit anywhere in the document counts | a target is an item; "found" can be scored per group as well |
| `docs/user/searching.md`, `rendered_documents.md`, `agent_user.md` | describe documents | rewritten with the step that changes each |

There are few enough users that links people have saved need no forward
path. A query a person types does, which is why `is:document` stays as
an alias.

## Forward path for existing data

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

1. **Parent columns.** Render fills `thread_uuid`, `reply_to_uuid` and
   a conversation-wide position for every source that has them, and
   gives each conversation and thread a row of its own. `grid_index`
   carries the columns and an index on `(parent, position)`. A re-render
   and a re-index; nothing a person sees changes. Fixes the
   `conversation_uuid` docs on the way.
2. **Children in order.** One route that lists a conversation's or a
   thread's items by position, a page at a time. The viewer can use it
   beside the file.
3. **Text in the render store.** Each item's rendered text is stored
   with its row. The search index reads items from the store instead of
   cutting files, and `keyword_index` folds into `grid_index`, which
   already reads every render store.
4. **Grouping when a search runs.** Search ranks items and groups them
   by the grouping asked for. `is:document` becomes an alias. The eval
   compares the grouped answer with today's.
5. **The viewer reads items.** `DocCard` opens a conversation at an
   item and pages through it. The `.md` files stop being written.
6. **No time buckets.** A bucketed source renders one conversation per
   chat. The `period` setting goes.
7. **Embedding context.** Chunks are cut from windows of a thread, and
   the eval picks the window.
8. **The rest of the table above:** the map, links, the problems banner
   with the problems plan, the docs.

## Open questions

- **How wide an embedding window is.** How many items or bytes of a
  thread, and does it overlap the next? The eval decides.
  [`multimodal_retrieval.md`](multimodal_retrieval.md) §5.1 argues for
  embedding whole threads; that is one input, not settled.
- **How a group ranks.** Reciprocal rank fusion per document today.
  Per group it could be the best item's score, or a sum over items;
  a long conversation must not win by being long.
- **What a group row shows** in the grid when it spans years: first and
  last time, the last few participants, the number of items matched.
- **Notion and Drive.** A page is one item today with no rows for its
  blocks. Are blocks items, so a hit lands on a paragraph, or is the
  page the item and its comments the children?
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
- **Is a section's uuid always its row's uuid?** In `chat-common` the
  `data-section-uuid` wrapper and the message's grid row share one uuid.
  That has not been checked for every renderer outside it (email,
  Notion, Drive, GitHub); step 1 checks it, since the problems plan
  rests on it too.
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
