# Search tabs: fields first, then words, then meaning

*Proposal (2026-10-08). Steps 0 to 3 of the order of work are built;
the terms from renders are not. Every number below was
read from a real data root that day (122,487 `grid_rows`; a 1.6 GB
qmd index holding 180,773 vectors for 60,559 documents in 8 sources)
through its `system/runs/runs.sqlite`, the
`//third-party/doltlite:doltlite` shell, and the qmd 2.8.3 the app
ships, run on a copy of the index. Each SQL timing includes about
0.05 s of process start.*

Today a free-text search is one qmd hybrid query (keyword and vector,
merged), and the grid shows nothing until it answers. This plan runs
three searches at once, each shown in its own tab: a full-text match
over everything a row answers to (the search terms), qmd's keyword
search, and qmd's vector search. A tab
is greyed out until its search answers, and what a tab shows never
changes while you look at it.

## What is slow, measured

**A pasted uuid took 17.9 s.** The request's log lines: the search
started `qmd mcp` (~2 s), qmd's hybrid search took 13.0 s, and mapping
its hits to grid rows took 1.9 s more (`grid_row_refs` reads every
row's `uuid, kind, qmd_path, provider, is_document`). SQL finds the same
rows in 0.03 s: the uuid is one row's `uuid` and six rows'
`conversation_uuid` and `markdown_uuid`, all indexed. Neither `uuid`
nor `markdown_uuid` is a search key, so the search bar cannot ask.

**Inside a hybrid search, the vector half is the cost.** Query expansion
never runs (the daemon sends typed `lex` and `vec` sub-queries), and
rerank is off.

| step | cost |
|---|---|
| loading the embedding model, once per `qmd mcp` process | ~1.5 s |
| embedding the query | 0.09 s |
| keyword (BM25) query | 0.07–0.5 s |
| vector query, every source | 5.3–5.8 s |
| vector query, one source | 0.7–1.2 s |

A vector query that names no collections searches each of qmd's
default collections in turn (`searchVec` in qmd's `dist/store.js`), and
for a collection over 20,000 vectors it scans all 180,773. Two sources
here are that big, so an unscoped vector query scans the whole table
twice, plus the smaller sources one by one.

**A free-text search returns at most 40 documents.** qmd fuses the
`lex` and `vec` lists and cuts the result to `candidateLimit`, which
defaults to 40, rerank or no rerank. The daemon asks for 1,000
(`QMD_DEPTH`) but never passes `candidateLimit`, and every search in the
log came back with 40 hits.

**The daemon restarts far more than it needs to.** It started 48 times
for 172 searches, because it restarts whenever `index.sqlite`'s mtime
moves, and every keyword or embed batch moves it. A running `qmd mcp`
reads the index live, so new rows need no restart
([`qmd_behaviour.md`](../qmd_behaviour.md), "How a running `qmd mcp`
behaves").

**SQL is fast enough to go first.** On the same grid index, with the
indexes it already has:

| match | time |
|---|---|
| a uuid against `uuid`, `conversation_uuid`, `markdown_uuid` and `notion_page_uuid` (one index lookup each: `MULTI-INDEX OR`) | ~0.03 s |
| an exact handle, `author_handle = 'email:…'` | 0.03 s |
| a word as a substring of `conversation_name`, `author`, `author_handle`, `channel` or `account` | 0.38–0.71 s |
| a word as a substring of `preview` | 0.31 s |

## The shape: three tabs, three searches

| tab | what answers it | pages |
|---|---|---|
| **Fields** | the search terms (below): every id, person, label and title a row answers to, in one full-text index | without limit, like every SQL search |
| **Words** | qmd's keyword (BM25) query over every document's whole text | qmd's ranked list |
| **Meaning** ("QMD semantic (vector)") | qmd's vector query | qmd's ranked list |

The three run at once. Each tab reads its own list from the results
cache (`results::Key` gains the tab), with its own count, paging, sort
and grouping. **The tabs answer in different ways, and the labels say
so:** Fields finds the ids, people, labels and titles a row answers
to, Words finds the
words anywhere in a document, ranked, and Meaning finds documents about
the same thing, whatever words they use.

**Nothing changes under the mouse.** A tab is greyed, with a spinner,
until its search answers; then it shows its count. Rows appear in a
tab only by your choice: clicking a tab is the only thing that changes
the rows on screen.

**Which tab opens.** Fields answers first. If it has rows, it opens.
If it has none, nothing is painted yet, so the first of Words and
Meaning to answer with rows opens, and nothing moves to make way. Once
any rows are on screen, the open tab never changes by itself.

**No fusion.** Hybrid search merged the keyword and vector lists into
one; with a tab each there is nothing to merge. `qmd_vsearch:`, which a
person may have typed or saved, opens the Meaning tab. `qmd:` opens
Words. The "Meaning only" checkbox goes: its job is now a tab.

**A query of identifiers only skips qmd.** A pasted uuid or email
address is answered by Fields, and the qmd tabs say "not searched: an
identifier". qmd's hits for a uuid are the documents whose text
contains it, which Fields already found.

### How deep each tab goes

Fields pages without limit through the grid's existing SQL paging.

Words and Meaning are ranked lists, so each is cut somewhere. The
daemon sends `candidateLimit`, which lifts the merged cut of 40. What
remains is qmd's own: each sub-query takes the best 20 documents of
each collection it searches, hard-coded and out of reach of the MCP
arguments (fact 7 in `qmd_behaviour.md`), and an unscoped search is
one search over every collection, so Meaning is the 20 nearest
documents in all, or 20 of one source under `source_id:`. Naming every
collection would reach 20 of each, but ranked by source rather than
by nearness (fact 8). Twenty is a fair depth for Meaning, where
nearness past the first few is noise. It is too
shallow for Words, where a keyword match far down the list is still a
real match: the Words tab should read qmd's own FTS5 table
(`documents_fts` in `index.sqlite`, plain SQLite that doltlite reads,
its WAL included) directly with BM25 and page it without limit, rather
than going through `qmd mcp` at all. Its keyword query took 0.07–0.5 s
through qmd; read directly it needs no daemon and no model.

### The search terms: everything a row answers to, in one tall table

A row answers to more than its columns hold. An email has one
`author_handle` in `grid_rows`, but also its To, Cc and Bcc, its
labels, its subject and its own ids. Those sets do not belong in
`grid_rows`, which keeps one value per column; they belong in one tall
table beside it, one row per term, dictionary-encoded so a term is three
integers:

```
rows  (row_id INTEGER PRIMARY KEY, uuid UNIQUE, touched_at_utc)
vals  (val_id INTEGER PRIMARY KEY, value UNIQUE)
terms (val_id, kind, row_id)  PRIMARY KEY (val_id, kind, row_id), index (row_id)
vals_fts  USING fts5(value, content='', contentless_delete=1,
                     tokenize="unicode61 tokenchars '@.-_+:/'")
```

`kind` is the `SearchTermKind` enum's code. The FTS5 index covers the
distinct values alone, linked to `vals` by rowid. The tokenizer keeps
`@ . - _ + : /` inside a word, so a uuid, an email address or
`slack:T…/U…` is one token, matched exactly. On a real root the
dictionary took the file from 131 MB (a uuid, a value and a timestamp
on every term) to 58 MB: there are 4.6 terms per distinct value.

**The terms live in a plain SQLite file beside the grid index,** not
in it: `unified_index/grid_index/search_terms.sqlite`, written by `grid_index`
and attached read-only by each reader of a grid commit. Nobody needs
the terms' history, and a doltlite store keeps it: with 732k terms, 200
one-document replaces each committed grew a store 35.5 MB and a plain
file 0.7 MB, and the plain file built in half the time at a quarter of
the size. The facts this rests on, the measurement and the two-process
test are in [`doltlite.md`](../doltlite.md) § "Full-text search
(FTS5)" and `doltlite_two_process_test`. What a separate file gives up:

- **No shared snapshot.** A reader pinned to a grid commit reads the
  terms as they are now. `grid_index` writes a document's terms just
  after sealing the commit that holds its rows, so the terms are at
  most one pass ahead or behind: a term whose row is not in the commit
  drops out of the join, and a brand-new row can be missed for that
  one pass.
- **No WAL.** Through doltlite a plain file keeps a rollback journal
  whatever `journal_mode` answers (dolthub/doltlite#3740), so a reader
  waits while the writer commits. Terms are written one document at a
  time, in small transactions: the slowest write in the two-process
  test took 1 ms.
- **Rebuildable.** The file holds nothing the render stores and
  `grid_rows` cannot give again, so a missing or damaged one is
  rebuilt, not migrated.

**`kind` is an enum**, `SearchTermKind`, with the usual strum pair, and a
new kind is new data, never a schema change. Built: `id`, `container`,
`from`, `title`, `name`. Planned: the person kinds `to`, `cc`, `bcc`,
`participant`, `mention` and `reactor`, and `label`; and, if the terms
come to serve the `author:`, `channel:` and `account:` keys,
`name` split into one kind for each ([`search_autocomplete.md`](search_autocomplete.md)
§"The wide columns and the tall search terms"). Each kind has an affinity, a
pure function in code (`affinity(kind)`): a row's own id outranks a
`to`, a `to` outranks a `cc`, a `title` outranks a `name`.

**Which rows carry a person, and in what role:**

- **On the item, not the document.** A message, a reaction, an email
  carries its people; a chat's document row does not, or a
  10,000-message chat would repeat every participant on it. A
  document's people are a query over its items.
- **A mention only where the source marks it up**: Slack's `<@U02>`,
  a Notion user mention, a GitHub `@login`, each an exact id. Never an
  address found in running text: quoted replies repeat every earlier
  message and signatures add noise.
- **No cap on recipients.** Dropping some would make the record lie.
  `vals` already stores each address once; what still grows is a
  reply-all thread, forty replies to the same 300 people being 12,000
  terms saying one thing. If that misses the budget, store each
  distinct recipient list once and point each email at it, never drop
  any. Measure on a copy of a real root first, as the derived terms
  were, with `grid_index`'s incremental pass under 10% slower.

**One table holds every term, so one query reaches all of them.** The
uuid columns are in it as `id` terms, `author_handle` as a `from` term,
`conversation_name` as a `title` term. Two writers fill it:

- **`grid_index` derives the terms a row already holds**, in one pure
  function of the `GridRow`: its uuids, its author's handle, its
  title, its author, channel and account as names. No render changes
  for these.
- **A render supplies the terms a row does not hold**: recipients,
  labels, mentions, participants. It returns them alongside each
  document, the way it returns `edges` and `problems`; they are stored
  in its render store, and `grid_index` copies them.

Either way, `grid_index` owns a document's terms the way it owns its
edges: each time it loads a document it deletes the terms carrying its
`markdown_uuid` and inserts the new set.

**How a search uses it.**

- **A bare word or identifier** (`sam@s.com`, a uuid, `budget`) is one
  `MATCH` over every term, ranked by the best kind each row matched
  in, then FTS5's own score, then newest first. A handle is normalized
  by `datalib_handle` first, so `sam@s.com` searches `email:sam@s.com`.
- **A keyed search names a kind**: `to:sam@s.com`, `cc:…`, `from:…`,
  `label:work`. It is the same `MATCH` restricted to `kind = ?`, and
  `-to:…` is `uuid NOT IN` that. These keys are declared beside the
  grid's column keys and read through the same grammar, so a key the
  search does not have is still refused by name. `from:` replaces
  `author_handle:`, which a person no longer needs to know. A value
  that is a handle or a contact matches exactly and anything else
  matches part of one, and `with:` is a person in any role:
  [`search_autocomplete.md`](search_autocomplete.md) §"The keys".
- **Mixed with other terms**, it narrows like any other key:
  `label:work source_id:gmail is:document budget` is the rows matching
  every part.

**A message's text is not a term, its preview included.** Words in
the body are what the Words and Meaning tabs search; a preview is only
the first few hundred characters, so a term made of it would match a
word near the top of a message and miss the same word further down.

It replaces three things planned separately: the four-column uuid
lookup, the substring scans of the short fields, and `row_handles`,
a table of which rows name which handle in which role: that is the
terms' person kinds.

**Not `edges`.** An edge leads from a place in one document to a place
in another, for navigation, and only Perseus writes them
([`edges.md`](../edges.md)). A term says a row has a value, for
search. They meet where a term's value can itself be opened: a `to`
term's handle is a person the contacts store resolves, and an `id`
term naming another document would be a whole-document link. Whether
edges should one day become terms of a `link` kind, keeping edges only
for Perseus's section-to-section links, is left for later.
`source_contact_handles` has this same tall shape for contact records,
and could fold in later too.

## Making qmd's tabs fast in themselves

1. **Reach every collection, as one list.** An unscoped query
   searches the collections the server read at startup (fact 3 in
   `qmd_behaviour.md`), and naming them all ranks each apart and
   interleaves them by rank (fact 8). Built: an unscoped search sends
   `collections: []`, every collection the index holds ranked
   together (fact 4); `source_id:` scopes a query to its source's
   collection, which is what makes a scoped vector query take under a
   second. Found by the fixture test in which every document searches
   for itself (`search_finds_itself_tests.rs`).
2. **Restart only when the index file is replaced.** Watch its inode,
   not its mtime; a new collection is covered by step 1. Built, with 1.
3. **Map only the hits.** Built: the rows behind the paths qmd returned
   are looked up through an expression index on `qmd_path` with case and
   every `-`/`_` dropped (qmd folds both), then kept only where the path
   matches exactly. On a real root, 40 hits took 2.8 ms against
   1.5–1.9 s for reading every row; building the index took 1.5 s once.
4. **The vector scan itself.** Each big source is a scan of every
   vector. Splitting the vectors per collection, or a smaller vector
   type, is a change in qmd, not here; note it upstream and measure
   again after 1 to 3.

## Order of work

0. **The doltlite facts FTS5 needs.** Done in imbue-ai/datalib#1107:
   the facts in `doltlite_facts_test`, the write cost in
   `doltlite.md`, and the attached search terms file beside a sealing writer
   in `doltlite_two_process_test`.
1. **The search terms, derived terms only.** Built in this step's PR,
   with one narrowing: only a query made entirely of identifiers is
   answered from the terms; words still go to qmd until the tabs give
   their answer a place. On a real root (122,579 rows) the first pass
   wrote 674,672 terms, 58 MB, with the whole step taking 4.3 s, and a
   pasted uuid's lookup took 1.5 ms. The search terms file, the
   `SearchTermKind` enum, the derivation in `grid_index`, and bare words and
   identifiers searched through it. An identifier-only query does not
   ask qmd. Test: a uuid search answers without asking qmd at all (the
   applet tests can give it a daemon that fails on any request).
2. **The qmd fixes** ("Making qmd's tabs fast in themselves", 1 to 3)
   and `candidateLimit`, each its own PR, with a `qmd_facts_test` test
   for anything new we rely on.
3. **Tabs.** Built. The API: `tab=fields|words|meaning` on the search
   and groups endpoints, each tab its own list in the results cache.
   Fields reads the search terms file with any word as the start of one; Words
   reads qmd's `documents_fts` with BM25 (1,000 deep, scoped by
   `source_id:`, each hit placed on the message where its first word
   is); Meaning is qmd's vector query alone. The UI: a small tab strip
   at the start of the source chips' row, all three asked at once, the
   first with rows opening, a dot on a tab that came back while another
   was open, and the list, table, groups and source chips all showing
   the open tab (`grid/searchTabs.ts`, `search-tabs.spec.ts`). "Meaning
   only" stays until the search predicate autocomplete work lands, then
   goes.
4. **Terms from renders.** Built for chat-common: a message's To, Cc
   and Bcc by handle, the people it mentions and its labels, as `to`,
   `cc`, `bcc`, `mention` and `label` terms on its own row, carried in
   each render store's `supplied_search_terms` and copied by
   `grid_index`. Bcc comes from the sender's copy of an email; mentions
   from email (an `@` `mailto:` link), Slack (`<@U…>`) and Signal (a
   `mentionAci` range). Still to come: `participant`, and mentions in
   Google Chat, WhatsApp and Messages, whose markup is not yet checked
   on real data. The keys that read them
   (`to:`, `cc:`, `from:`, `with:`, `label:`) belong to the search
   autocomplete plan.
5. **More identifier kinds** if wanted: an upstream URL
   (`chatgpt.com/c/…`) as an `id` term.

## Open questions

- A query that mixes an identifier and words (`someone@example.com
  budget`): Fields can require both, which is likely what a person
  means. The qmd tabs would search the words alone.
- The live refresh when the index commits still patches rows in place
  (`applyPatch` in `GridCard.ce.vue`). The same "nothing changes
  unasked" rule may want a "N changed · Show" there too; that is its
  own change.
