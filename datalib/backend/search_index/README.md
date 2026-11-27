# `datalib_search_index`: the search index

The search index is what a search reads to rank documents: a keyword
index of every searched document's messages, and each document's
embedded chunks with their vectors. It holds no document text. A
document comes in as a value (`IndexDoc`, drawn from the source's render
store by `text.rs`), so nothing here reads a file, and everything here is
derived: lost or in an old shape, it is rebuilt from the render stores,
never migrated.

## Two files, two writers

| file | tables | written by |
|---|---|---|
| the grid index's file, `unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite` (plain SQLite, WAL) | `search_documents`, `search_sections`, `search_fts`, `search_state`, `search_sources` (`store.rs`) | the `unified_grid_and_search_index` step's keyword pass |
| the inference cache, `unified_grid_and_search_index/search_index/inference_cache.sqlite` | `vectors` (`cache.rs`); `search_chunked`, `search_chunks` (`chunked.rs`) | each source's `embed`, one at a time under the `embedder` lock; the index step's sweep, which never runs beside one |

**The keyword tables sit beside `grid_rows`** so that one query can narrow
by the grid's filters through `grid_rows`' own indexes and rank what is
left. They are a *part* of that file (`grid_index::Part::Keywords`): the
file records each part's shape under its own key, and a change of shape
empties only that part's tables, so a new grid column does not cost every
source's keyword index, and a new keyword table costs no grid row
(`datalib/backend/store_meta/README.md`). Every table this crate owns
starts with `search_` (`TABLE_PREFIX`).

**Several writers share the grid index's file.** Every write transaction
begins `IMMEDIATE`, so a writer waits for the other's lock rather than
failing when the other commits between its read and its write. The grid
holds the file for one source's load at a time, which is many minutes for
a large source loaded whole, so a keyword write waits up to an hour
(`WAIT_FOR_THE_GRID`), and writes in transactions of about a megabyte of
text (`PUT_BYTES`) and removes 500 documents at a time, so it never holds
the grid back for long either.

**`search_state.generation`** rises with every write that changes what
the index holds, so search knows to read its vectors again without
watching a file the grid writes far more often.

**Rows are named by uuid**, the message's `grid_rows.uuid`, not by the
grid's integer row id. A row removed and written again (a source reset,
then rendered anew) gets a new row id, and the keyword tables are
rewritten only when a document's text changes, so a row id stored here
would then name the wrong row.

## Words

`search_fts` is FTS5 with `content=''` and `contentless_delete=1`: the
postings and no stored copy of the text. `bm25()` works on it;
`snippet()` and `highlight()` do not, and nothing uses them.
`tests/fts_facts.rs` holds each fact on the SQLite we link. One FTS row is
one section (a message, or a piece of a page), title weighted twice the
body.

A conversation matches where one message holds every word, and the hit
lands on that message: the words are ranked per message, not per thread,
so a thread whose words are spread over several messages is found by
meaning, not by its words. A page (`search_documents.conversation = 0`)
may also hold the words across its pieces, ranked below one that says
them in one place. `Reader::words` takes each document's best message in
SQL before its cut, so one long chat cannot fill the cut by itself.

**Words are indexed as written**, case and accents folded
(`tokenize='unicode61 remove_diacritics 2'`), with no stemming. A quoted
phrase is then exact, and meaning finds the other forms of a word.
Porter stemming was tried and dropped: it only strips regular English
endings ("ran" never finds "run", nor "mice" "mouse", nor any German or
Italian form another), it merges words that differ ("universe" and
"university", "news" and "new"), it needed a second keyword table of
about 280 MB for quoted phrases, and a search of eight words or more took
two to three times as long on its words. Meaning covers all of those cases.

## Meaning

**A chunk is what is embedded.** Each message starts a chunk of its own;
a message longer than the budget (900 of the model's tokens, counted by
its tokenizer) is cut inside itself at the last paragraph, line, sentence
or word break, its pieces overlapping by 135 tokens; a heading of level
one or two starts a chunk too. `chunk.rs` says why each of those is so.

**The inference cache keys a vector by the prompt it was made from.**
`search_chunks.hash` is the blake3 of the exact string the model is shown
(`prompt.rs`: the frame, the title, the chunk's text), and `vectors` holds
`(model, hash) → vector`. A model is a pure function, so an entry is never
wrong, only unused: another model or another prompt simply misses, with
nothing to invalidate. An `embed` asks which of its chunks' hashes the
cache lacks and embeds only those, which means:

- a thread that grows re-embeds its new messages and nothing before them
  (`a_grown_thread_keeps_its_earlier_chunks`);
- an edit under one heading re-embeds that heading's chunks
  (`an_edit_under_one_heading_moves_only_its_chunks`);
- the same chunk under the same title in many documents, a signature or
  a footer, is embedded once;
- a change to the chunker re-embeds only the chunks whose text moved.

A document whose title changes re-embeds all its chunks, since the title
is part of every chunk's prompt. The sweep deletes the vectors no chunk
names and every other model's. The cache is a file of its own so that
rebuilding the keyword index, or the grid index, never throws embedding
work away.

**Search ranks by brute force, in memory.** The applet holds every
embedded chunk's vector as a unit vector in half precision (`half.rs`)
and compares the query with all of them, which is exact. There is no
approximate-nearest-neighbour index: doltlite's `vec1` is compiled out
of what we ship, and what it would and would not do with a filter is in
[`doltlite.md`](../../../docs/dev/doltlite.md) § "Vector search: vec1".
An index is worth building when the memory is the problem, not before.

The model, and why its vectors can be trusted, are
[`../embedder/README.md`](../embedder/README.md).

## How a search uses it

What a person can type, and what each part does, is
[`docs/user/searching.md`](../../../docs/user/searching.md). Behind it, in
the `unified_grid_and_search_index` applet:

- **The filters are worked out once**, for the query and the grid
  index's commit (`keyed` in
  `applets/src/unified_grid_and_search_index/mod.rs`, cached), before
  anything is ranked. Every ranker and the landing read that one answer.
  A words statement takes it as two bound JSON arrays (`KeptRows`: the
  messages kept, and the documents kept whole) and applies it before its
  cut. Writing the filter's SQL into each words statement instead was
  measured slower: a person filter can take a second to evaluate, and it
  was evaluated in every statement.
- **Words and meaning are fused by reciprocal rank**, one entry a
  document, then cut once (`RANK_DEPTH`).
- **Bare words are all required** of the words ranker, as in Gmail, Slack
  and FTS5's own default. They are not a filter: meaning still finds a
  document holding none of them. There is no prefix matching: a search
  runs on Enter, so no word is half typed.
- **A document's own terms are on its row alone.** Its title, its
  containers, its participants and its subject are terms of the
  document's row, and a message answers to them through its document
  (`datalib_schema::search_terms::search_terms_of`), so the thousand
  messages of one long chat cannot crowd every other document out of a
  match on their title.
- **A uuid is stored in one spelling.** An id that is a uuid is written
  lowercase with dashes (`search_terms::stored_value`), so a uuid
  however typed is one comparison. `id:` is not a terms key: a uuid typed
  after it reads the row's `UpstreamId` term, and anything else compares
  the `upstream_id` column as the source wrote it.
- **The column keys stay columns.** `source:`, `kind:`, `channel:` and
  the other `grid_rows` columns are filtered through their own indexes,
  not as terms. Moving them into the terms table was measured on a real
  library: it saved about 0.6 GB of a 5.4 GB file but made a listing with
  no words, grouping and sorting by such a column slower. Leaving empty
  values out of those indexes was measured too, and would make grouping
  and sorting read the whole table.
