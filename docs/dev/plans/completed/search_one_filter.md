# Search: one filter, two rankers

**Status: built (2026-11-20), kept as the record of what was decided
and why.** Steps 1 and 2 landed (#1892, #1912; word forms in #1904,
#1906 and #1928); step 3 was measured and held off (below). Answers
[#1887](https://github.com/imbue-ai/datalib/issues/1887). Four tests
reproduce what step 1 fixes; they are listed under "What goes wrong
today". [`docs/user/searching.md`](../../../user/searching.md) says
what a person is promised now; the code it names is the reference.

Step 1 as built differs from its description below in three ways.

- **The filter is evaluated once, in memory.** `keyed` works out what the
  filters keep, for every filter alike, cached per query and commit as
  before. Meaning, landing and the words all read it; each words
  statement binds it as two JSON arrays (the messages kept, and the
  documents kept whole) and applies it before its cut. Putting the
  filter's SQL into each words statement instead was tried and
  measured: a person filter costs over a second to evaluate, and was
  evaluated in every statement (7.7 s against 2.7 s for a three-word
  `from:ri` search).
- **A pasted id lists the rows naming it newest first**, not the ones
  most about it first, and a row no longer shows which of its terms
  matched (`cc: …`).
- **The words ranker took the crowding fix too.** It read its best 8,000
  messages and kept one per document, so long agent sessions filled it:
  on one real library, `search` found 937 documents and
  `search source:<agent sessions>` 524. It now takes each document's best
  message in SQL before its cut, and both find 1,000.

Measured on that library (566k rows, while a sync ran; best of three
runs of `datalib-applet search … semantic:off`, page of 200): the new
code is within half a second of `main` on unfiltered, `source:`,
`-source:`, `after:`, `with:me` and `from:ri` searches, while finding
more. With meaning on, a `source:` filter now pays to work out what it
keeps (3.3 s for a 387k-row source, once per filter and commit), which
`main` skipped by scoping. That cost is reading the table: the
`source_id` index lacks `markdown_uuid`, so each of the 387k rows is a
read of the table (1.5 s cold, 0.25 s warm, on a copy). With
`markdown_uuid` in the index it is read from the index alone (0.025 s),
but in all eleven filter-key indexes that is 424 MB more of text, on
indexes already 1,011 MB beside a 1,047 MB table. Step 2's integer ids
make the same covering a few bytes an entry, so it waits for them.

Step 2 as built differs from its description below in three ways.

- **The search index keeps naming rows by uuid.** A row's id is its
  `grid_rows` rowid, which `rows.row_id` now is; it holds while the row
  is written in place, and a row removed and written again (a source
  reset, then rendered anew) gets another. The keyword tables are
  rewritten only when a document's text changes, so row ids there would
  name the wrong rows after a reset. The filter reads integers
  throughout and hands the rankers uuids, as before. Measured on a
  clone of a real library: what `source:` keeps of an 82k-row source
  went from 1.16 s (cold; 0.07 s warm) to 0.03 s, with no new index on
  `grid_rows`.
- **Which terms are the document's.** Of 957,854 rows inside documents,
  every one held its document's title and named its document as a
  container: 1.9M of 6.5M terms, gone. Participant and subject terms
  were already on the document's row alone. Names (a channel, an
  account) are the row's own and stay: 8 of 566,628 differ from their
  document's.
- **`id:` is not a terms key.** Every row has an upstream id and 117k of
  1.2M are uuids; a term for each would add a million values to the
  three value indexes. A uuid typed after `id:` reads its `UpstreamId`
  term, which every uuid-shaped upstream id now has, in one spelling;
  anything else compares the column as written. The column itself
  stays as the source wrote it, since the row's uuid is minted from it
  (`docs/dev/entity_ids.md`). `account` and `project` are stored with a
  uuid in one spelling. In the real library no uuid was stored any other
  way, so no answer there changes.

## The model

A query is **filters** and **words to rank by**.

| typed | what it is |
|---|---|
| `key:value`, `-key:value`, `before:`, `after:`, `is:` | a filter on a row's properties |
| `"two words"` | a filter: the row's text holds those words, in that order |
| `+word` | a filter: the row's text holds the word |
| `-word`, `-"two words"` | a filter: the row's text does not hold it |
| bare words | ranked: by its words (BM25, every word required, as today) and, unless `semantic:off`, by meaning, which needs none of them |
| a bare id or address (`picard@enterprise.org`, a uuid, a Notion link) | a filter: rows that name it in any field |

Quoted and `+` words also rank, as bare words do. A query with no bare
words lists what the filters keep, newest first (or by `--sort`); one
with only ids lists the rows that name them, the ones most about them
first, as today.

**Every filter keeps rows, and is applied before anything is ranked or
cut.** A row's text is its message for a row inside a conversation, and
the whole document for a document's own row and for a page's rows. A
ranked result is a document, shown at its best row that the filters
kept. So `warp -breach` finds a thread whose other message says
"breach", shown at the message that does not; a thread every message of
which says "breach" is not found at all, whichever ranker would have
found it.

There are two rankers, words and meaning, fused by reciprocal rank as
today. The third matcher today, the search terms' "names and ids", stops
being a ranker: an id becomes a filter, and a name is already in the text
the words ranker reads (the item's envelope line: who, to whom, labels).

## What goes wrong today

Each is a test on this branch that fails on `main`:

| test | what happens |
|---|---|
| `a_source_keeps_a_name_match_ranked_past_the_depth` (`mod.rs`) | `holodeck source:tng_email` finds nothing: the names matcher cuts the whole library to its best 1,000 rows, all of another source, and only then filters. The issue's hole. |
| `one_long_conversation_does_not_crowd_out_another_title` (`mod.rs`) | `holodeck` finds one document: each message of a long chat carries the chat's title as a term, so its 1,000 messages fill the names matcher's cut, and merging one per document leaves one. |
| `a_quoted_phrase_is_held_by_every_document_found` (`search_tests.rs`) | `"warp core"` on the TNG fixture returns 40 documents, 30 of which never say it: meaning embeds the phrase without its quotes and adds whatever is near. |
| `an_excluded_word_keeps_out_a_document_that_says_it_everywhere` (`search_tests.rs`) | `starship -enterprise` returns 11 documents that say "enterprise" in every piece: meaning drops `-enterprise` before it embeds. |

The same, measured on a real library (566k rows, 60k documents; counts
only):

- **The source hole.** For one of the ten most common author names in a
  Gmail source, `<name> source:<gmail> semantic:off` left out 12 of the
  275 threads the name matches; their best name match ranked 1,571st to
  3,660th across the library.
- **The crowding.** For 12 words from the titles of the six longest
  agent sessions, the names matcher's 1,000 rows reached 1 to 24
  documents of the 6 to 1,507 that match. Title terms are 566,152 of the
  index's 2.8M terms: one per row, copied from its document.
- **Exclusion.** `<word> -<other> semantic:off` returned 1,000 documents
  of which 154 say the excluded word (the words matcher excludes per
  message; the names matcher only where a field holds it). With meaning
  on, 168.
- **Quotes.** A quoted two-word phrase with meaning on returned 1,000
  documents, 693 of which never say it. With `semantic:off`, none.
- **Not exact either.** The keyword index stems (`porter unicode61`), so
  `"runs"` matches "running" and `"WARP.CORE"` matches "Warp-Core".
- **`total` is never more than 1,000** for a search with words, however
  many match: it counts the cut.

## Why: six filter sites, three shapes, three granularities

A free-text search today
(`applets/src/unified_grid_and_search_index/mod.rs`, `fused_ranking`)
runs three matchers that each answer at a different grain and take the
filters a different way:

| matcher | answers with | takes the filters | its cut |
|---|---|---|---|
| names and ids (`search_terms.rs`, `HITS_SQL`) | rows | after its cut (`entries_kept`) | 1,000 rows |
| words (`search_index::Reader::words`, `page_words`) | documents at a message | before: `among`, a JSON array of every document the filters keep, and a `source:` scope | 1,000 documents |
| meaning (`HeldVectors::nearest`) | documents at a chunk | before: the kept sections, and the scope | 1,000 documents |

The filters are worked out in two shapes, `keyed` (a set in memory,
cached per query and commit, `None` for a positive `source:` alone) and
`filter_uuids` (SQL over a list of ids), and applied again when a hit
lands on a message (`land`) and when a sorted search is listed. One
search opens the grid index file about five times, each at its own
snapshot (the repo's read, `search_terms::lookup`, and a `Reader` per
words query), which is why a ranking carries `terms_at` and is not
cached when two reads disagree. `-word` and `"phrase"` are free text,
each matcher reading them its own way: words as FTS5 `NOT` and phrases
per message, names as an exclusion of fields, meaning not at all.

## The plan

### 1. One read, one filter, applied first (no change to the index)

- **Parse into the model above** (`query.rs`): a quoted, `+` or `-` word
  becomes a `Field::Text` term and a pasted id a `Field::Names` term, and
  `free_text` becomes the words to rank by, with no quotes or minus.
- **Compile every filter to one SQL row set**: `build_where`, plus the
  text filters as `IN` over `search_fts` joined to `search_sections` by
  the row's uuid (or its document's, for a document's own row), plus ids
  as a lookup in `terms`. Evaluated once per query and commit into
  `keyed` (see the status above).
- **Words ranks within it**: `search_fts MATCH` the words, joined to what
  `keyed` keeps before its `ORDER BY … LIMIT`. Meaning takes the kept
  sections from `keyed` (`kept_of`).
- **Fuse, cut once, show each document at its best kept row**, which the
  rankers already name, so landing stops filtering.

As built, this deleted `fields_ranking`, `HITS_SQL` and the lookup
behind it, `narrows_documents`, `entries_kept`, `collection_scope` and
`CollectionScope`, `terms_at`, `markdowns_of`, the `among` arrays and the
page exclusion pass in `search_index::read`, and the quote and minus
handling in `hits::lex`. `keyed` stays, as the one evaluation of the
filters; `land` no longer has a second way to filter. All four tests
above pass.

What a person notices: `"…"`, `+…` and `-…` now hold whichever way a
document was found, so such searches return fewer, right, results;
`source:` no longer loses name matches; a word no longer matches as the
start of a field (`per` finding "Percy"), which only the names matcher
did. The search runs on Enter, so nothing needs a word half typed.

### 2. The index in the shape the model reads (a re-index)

The grid index and the search index are derived stores, so a change of
shape costs a rebuild, not a migration (AGENTS.md).

- **One integer per row.** `rows.row_id` becomes the id every table
  joins on: `search_sections` names its row by it (a page's pieces name
  the page's row), and `rows` holds `doc_row_id`, `is_document`,
  `source_id` and `touched_at_utc`, so a filter and a ranker meet on
  integers rather than uuid strings and `json_each`. The search terms
  are integers already; `grid_rows` and its indexes are text (a
  36-character uuid in every entry of eleven indexes that together
  outweigh the table). Its filter-key indexes then end in the row's and
  its document's integers, which covers what a filter keeps.
- **A document's terms once, on its row.** Title, container,
  participant and subject terms go on the document's row only; a
  message answers to them through `doc_row_id`. About 1.1M of the real
  library's 2.8M terms, and the reason one chat could crowd out the
  rest.
- **No stemming; meaning covers word forms.** The keyword index today
  cuts each word to its stem (`porter`: "running" and "runs" are both
  "run"), so a quoted phrase cannot be exact. Index the words as written
  instead (`tokenize='unicode61 remove_diacritics 2'`): a bare word then
  finds its own form, meaning finds the others along with synonyms and
  other languages, and a quoted phrase is exact (its words in order;
  case, accents and punctuation still fold, as in Google's quotes). One
  table, not a second 284 MB one beside it. Porter is English-only, and
  conflates words that differ ("universe", "university"). What it costs
  is a word form with `semantic:off`, or in a source not embedded:
  measure it with `scripts/search_eval.py` before and after. Only the
  keyword tables are rebuilt; no chunk is embedded again.

  **Tried both ways.** Without stems, 454 queries on a real library with
  `semantic:off` went from 54% to 52% found in the top 10; with meaning
  on, about half the lost targets came back, most of the rest being long
  descriptions of a passage in one long page. #1906 then put stems back
  for bare words, in a second table beside the one quoted phrases read,
  and a day later they went again, for these reasons:
  - Porter only strips regular English endings. "ran" does not find
    "run", nor "went" "go", "mice" "mouse" or "children" "child"; nor
    does any German or Italian form find another ("Häuser" and "Haus",
    "parlato" and "parlare", "libri" and "libro").
  - It merges words that differ: "universe" finds "university",
    "organization" "organ", "news" "new".
  - Meaning already finds all of these, irregular forms, synonyms and
    other languages included.
  - It cost a second keyword table (about 280 MB) and long searches: with
    stems a search of eight words or more took a median 1.7 to 2.5 s on
    the words alone, against 0.7 s without.
- **A uuid stored one way.** #1886 matches a uuid however it is typed by
  asking each key for four spellings (dashed or not, lowercase or
  capitals, `IN (SELECT value FROM json_each(?))`), and the pasted-id
  lookup separately reads `terms`, which holds an upstream id only when it
  is dashed. Instead, write every uuid-shaped value canonically
  (`datalib_uuid_text::canonical`) when the index is written, into
  `terms` (an upstream id of any shape becomes an `UpstreamId` term), and
  make `id:` a terms key. Then a typed uuid is canonicalized once, every
  lookup is one `=`, `spellings` and its `IN` branch go, a source that
  stores ids bare or in capitals is found by a pasted link too, and `id:`
  and a pasted id read one table. Ids that are not uuids are kept as
  written, case and all (two Drive comment ids differ only in case). In
  the real library every uuid-shaped upstream id is already lowercase and
  dashed, so this changes no answer there; it closes the cases #1886
  guards against with one lookup in place of four.

### 3. Every key a term (optional, measure first)

`source:`, `kind:`, `channel:`, `account:`, `project:`, `subject:` and
`change:` are `grid_rows` columns today, each with an index that serves
a newest-first listing with no words; the person keys, labels and photo
keys are already rows of `terms`, the melted `(value, kind, row)`
table. Moving the column keys into `terms` gives every key one path for
filtering, `/search/keys` and `/search/values`, and lets `grid_rows`
drop columns only some kinds use. The cost is the listing with no words:
`source:x` alone would read its rows from `terms` and sort them by
`rows.touched_at_utc` rather than walk a covering index. Measure that on
a real library before deciding.

**Measured (2026-11-20) on a copy of a real library's index (1.2M rows),
and held off.** The seven keys as terms add 3.6M terms (66 MB) and need
each row's time in `rows` again (about 30 MB); their seven indexes,
which go, are 711 MB. Times in ms, first run / best of three:

| | today | from the terms |
|---|---|---|
| `source:` alone, 585k rows, first page | 1 / 0 | 308 / 41 |
| `source:` alone, 82k rows | 0 / 0 | 12 / 5 |
| `source:` + `kind:` | 17 / 0 | 332 / 90 |
| `-source:` | 14 / 0 | 287 / 260 |
| count for `source:` | 33 / 23 | 11 / 9 |
| group by source | 501 / 594 | 137 / 90 |
| sort the grid by Source | 251 / 223 | 899 / 487 |

Grouping, the suggested values and sorting by a column read the same
indexes, so all four would have to move to the terms, not filtering
alone; without its index, sorting by Source reads the whole table (8 to
9 s). Two other layouts were tried: the row's time inside the term's
key keeps paging as fast as today but costs 213 MB for these terms
alone; walking today's newest-first index and keeping the rows a key's
terms name is fast for a large key and slow for a small old one (115 ms
for 201 rows). About 0.6 GB of a 5.4 GB file was not worth a slower
listing and four moved paths.

Two smaller findings from the same measurement:

- **Indexes that leave out empty values** shrink the sparse columns'
  indexes by about 310 MB (`channel` 88 MB to 2.7, `notion_page_uuid` 90
  to 6.8, `diff_status` 88 to 0, `project` 99 to 39, `account` 99 to
  65), with lookups as fast, provided the condition is `IS NOT NULL`
  alone (a `!= ''` beside it keeps the planner from using them). Not
  done: grouping and sorting by such a column then read the whole
  table, 0.5 s becoming 11 to 14 s.
- **`contact:`, `email:` and `phone:`** read a contact card's own
  columns, which no index served: one search took 35 to 44 s. They were
  removed (`with:` finds a card by its name, address or number), and
  the grid's Contact, Email and Phone columns no longer filter.

## Decided (2026-11-19)

- **Bare words are all required** of the words ranker, as in Google,
  Gmail, Slack, SQLite FTS5 and Postgres's `plainto_tsquery` (Lucene's
  default is any). They are not a filter: meaning still finds a document
  holding none of them. Unchanged from today.
- **Meaning, not stemming, covers word forms**, and a quoted phrase is
  exact: step 2. Decided again on 2026-11-20, after trying stems back
  (above).
- **No prefix matching.** The search runs on Enter.
