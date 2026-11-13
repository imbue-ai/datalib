# Our own search index, fed from memory

**Status: proposal (2026-11-11). Steps 1 to 8 of "Order of work" and
the first half of step 9 are built (`//datalib/backend/search_index`,
`//datalib/backend/embedder`): search reads only the search index, the
steps no longer run qmd, and the aggregator deletes qmd's index. The
second half, deleting qmd's code, runtime and names, is not.** Numbers were
measured that day on `~/datalib/stay_alive_1` (sizes and counts only).
The wider retrieval redesign in [`multimodal_retrieval.md`](multimodal_retrieval.md)
overlaps with this one; this plan is the narrow first cut of it: keep the
model, keep the rendered files, stop qmd from storing the text again, and
move the indexing itself into Rust.
Rendering documents on demand, which would let a source stop writing its
files, is the step after this one ("Later", at the end).

## The problem

Every rendered document is written as a `.md` file, and qmd keeps two
more copies of its text. On the measured root the qmd index is 6.9 GB:

| part of `index.sqlite` | size |
|---|---|
| vectors (`vectors_vec_vector_chunks00`, 930k chunks × 768 `f32`) | 2.86 GB |
| `content` (the text, copy 2) | 1.38 GB |
| `documents_fts_content` (the text, copy 3) | 1.37 GB |
| `documents_fts_data` (the keyword index proper) | 0.89 GB |
| bookkeeping and indexes | ~0.7 GB |

It names 279,811 documents, of which only 171,827 are active: qmd keeps a
retired document's row. And qmd records nothing about where a chunk falls
among a document's messages, so search reads the file again to find out.

Search already reads qmd's tables itself: the Words tab queries
`documents_fts` (`unified_document_index/src/qmd/index_state.rs`), the
Meaning tab reads the vectors out of sqlite-vec's storage tables
(`qmd/vectors.rs`), and it asks qmd for nothing but a query's embedding
(`qmd_indexer/src/query_embedder.rs`). The other reader of the file is
`GridIndex::land` (`qmd/mapping.rs`), which reads a hit's `.md` to find
the `data-section-uuid` the hit falls in.

## What stays the same

- **The rendered files.** Render writes its `.md` files exactly as now,
  and `/chat/{uuid}` serves them.
- **One index file, written per source.** qmd's index is already one file
  (`unified_document_index/qmd_aggregator/qmd/index.sqlite`) with one
  collection per source; each source's `keyword_index` and `embed` steps
  write that source's rows into it. The new index keeps that shape: one
  file, rows tagged with `source_id`, each source's steps writing only
  their own. BM25's statistics stay global across sources, as now. The
  file becomes the grid index's ("One file with the grid index", below).
- **The model and the prompt format**: the pinned embeddinggemma GGUF and
  qmd's `title: … | text: …` format, so vectors mean what they meant. What
  runs the model moves from node-llama-cpp into Rust ("Embedding in
  Rust", below).
- **The step graph.** `keyword_index` and `embed` per source, an
  aggregator that drops a source it no longer reads.

## The index

A new crate, `search_index`, owns its tables in the grid index's file,
plain SQLite in WAL mode ("One file with the grid index", below), each
named `search_…`. Not doltlite; rebuilt from the render stores whenever
it is lost.
Its input is a value, never a path:

```rust
pub struct IndexDoc {
    pub source_id: String,
    pub markdown_uuid: String,
    pub content_hash: String,        // what the render store records
    pub title: String,
    pub sections: Vec<IndexSection>, // in document order
}
pub struct IndexSection {
    pub uuid: Option<String>, // None for the part above the first message
    pub text: String,
}
```

Tables (no document text anywhere):

- `search_documents(doc_id INTEGER PRIMARY KEY, source_id, markdown_uuid
  UNIQUE, content_hash)`.
- `search_sections(section_id INTEGER PRIMARY KEY, doc_id, seq, uuid,
  start_byte, end_byte)`: each message's place in its document. The
  title is indexed once, on the first section's row.
- `search_fts`: FTS5 with `content=''` and `contentless_delete=1`,
  columns `title, body`, rowid = `section_id`, one row per message. A
  contentless table keeps the inverted index and drops the stored copy;
  `bm25()` still works on it, `snippet()` and `highlight()` do not, and we
  use neither. `fts_facts_test` holds each fact on the SQLite we link,
  doltlite 0.51.3's.
- `search_chunks(doc_id, seq, start_byte, end_byte, first_section,
  last_section, hash)`: what each embedded chunk covers; `hash` is the
  hash of the prompt it is embedded as. The vectors are in the inference
  cache, below.
- `search_state(generation)`: raised by every write that changes what
  the index holds, so search knows to read its vectors again without
  watching a file the grid index writes far more often.

Tables in another shape are emptied under the write lock, so two steps
opening an old index together empty it once.

A document with no messages (a Notion page, a calendar event) is one
section with no uuid.

### Words: BM25 over messages

A Words hit is a message: one FTS5 query (`warp AND core NOT breach`)
over the message rows, ranked by `bm25()`, each document once at its best
message, which is where the hit lands. A message is also what most of a
search's other terms describe (`from:`, a date), so they narrow the row
that matched rather than a document that merely holds one.

What this gives up: words a thread says in different messages are not a
Words hit. That is what Meaning is for: a long thread is embedded in
chunks of its messages. Matching each term against messages and joining
them per document, to find such a thread by its words too, was built and
measured (below): it cost a join per term and several fixes to come near
qmd's speed, for no gain on the eval, so it went.

### Meaning: chunks follow messages

A chunk is whole messages packed up to 3,600 bytes, qmd's 900 tokens at
its four characters a token; only a message longer than that is cut
inside itself, at the last paragraph, line, sentence or word break, and
its pieces overlap by 540 bytes (qmd's 15%). Whole messages need no
overlap. A hit then names its messages from `search_chunks`; `land` still reads
the file, to quote the hit, and lands it within the chunk's bytes.

**A heading starts a chunk.** Packing never carries a chunk across a
heading of level one or two, inside a message or between them, so an
edit under one heading moves the chunks under that heading and no
others. Without it, an edit that changes one section's length shifts
every cut after it.

### The inference cache

A vector is the model's answer to one exact string, so it is cached
under that: `search_chunks.hash` is the blake3 of the prompt the chunk is
embedded as (`prompt::document`: qmd's frame, the title, the chunk's
text), and the vector is kept in the **inference cache** under
`(model, hash)` (`cache.rs`, its own file). Embedding a source asks the
cache which of its chunks' hashes it lacks, and embeds only those.

A model is a pure function, so an entry is never wrong, only unused. A
different model, or a change to the prompt, misses the cache rather than
reading a vector made another way, and needs no invalidation. A sweep,
once the index names every chunk it holds, deletes what no chunk names
and every other model's entries. The cache joins nothing; search joins
from `search_chunks` to it. Kept in a file of its own, it survives the index
being emptied for a change of shape, and the grid index being rebuilt.

What that saves:

- **a thread that grows** re-embeds its last chunk and the new ones:
  packing runs from the top, so the chunks before the new message are
  unchanged (`a_grown_thread_keeps_its_earlier_chunks`);
- **an edit under one heading** re-embeds that heading's chunks
  (`an_edit_under_one_heading_moves_only_its_chunks`);
- **the same chunk in many documents**, a footer or a signature block
  under one title, is embedded once;
- **a change to the chunker** re-embeds only the chunks whose text moved.

What it does not: the title is part of every chunk's prompt, so a
document whose title changes re-embeds all of its chunks. The title is
context the model reads the chunk in.

### Writing it

**The source's steps read the rendered file into an `IndexDoc`**: the
`.md` plus `markdowns.section_layout`, which already holds each
section's uuid and byte length. That is the simplest place, and the one
that changes when rendering on demand lands: the `IndexDoc` will then come
from a render in memory instead.

**What is owed is a query.** The step compares `(markdown_uuid,
content_hash)` from the source's render store with the index's
`search_documents`: new or changed documents are indexed, gone ones deleted.
`markdowns` gains a `content_hash` column, computed in render where the
text already is; a re-render fills it.

**Writes are one transaction per batch of documents**, and there is no
shared `content` table for one source's write to orphan another's, so
qmd findings 12 and 14 (`qmd_behaviour.md`: a lost body; a small update
scanning the whole index) do not carry over and the runner's
`qmd_keyword` lock can go. `qmd_embed` stays, as a memory lock: one
loaded model at a time.

### Embedding in Rust

The steps embed in process, so progress, stopping and memory are ours to
report and control rather than a node child's to relay.

**What runs the model: llama.cpp, built from source.** It is what runs
GGUF files, and what runs this one today: qmd 2.8.3's node-llama-cpp
3.20.0 is built on llama.cpp release `b10361`. Pinning the same release
gets its tokenizer, its `gemma-embedding` graph (bidirectional
attention, the 512-token sliding window, mean pooling, the two dense
layers) and its Q8_0 kernels, rather than a port of each that has to be
proven equal. It is MIT, and builds under Bazel's `cc_library` as
doltlite does, with a small hand-written binding to `llama.h` for what
we call: load a model, tokenize, embed a batch. On macOS the build adds
ggml's Metal backend, and the embedder runs there when asked for its best
device.

The alternative was `candle`, already a dependency through perseus. It
would mean writing the `gemma-embedding` model and the GGUF tokenizer
ourselves, and proving both equal to llama.cpp, for a pure-Rust build.

The pinned GGUF holds everything the model needs: the architecture, the
dense layers and the whole tokenizer (262,144 tokens with scores, BOS
and EOS added, no space prefix). The query embedder in the search
applet moves in process too, and node leaves search.

**Each piece is tested against qmd itself**, from the vendored source
(`third-party/qmd`) and the pinned runtime, driven by a script in the
test:

- the prompt format, query and document, string for string with qmd's
  `formatQueryForEmbedding` and `formatDocForEmbedding`;
- the tokenizer's token count for a set of texts, against
  node-llama-cpp's;
- the vectors: on the fixture's chunks, every one within cosine 0.999 of
  what qmd's `embed` writes for the same text;
- the long-section cut, against `chunkDocument` on the same text with
  the same budget, where the break rules are meant to agree.

**Built: `datalib/backend/embedder`**, a C shim over
`llama.h` (`csrc/embedder.cpp`) and a Rust crate over it, with llama.cpp
built by `third-party/llama_cpp/llama_cpp.BUILD`. `parity_test` runs
qmd's own `formatDocForEmbedding`, `formatQueryForEmbedding`, tokenizer
and `embed` through the pinned node and package, on made-up texts, and
finds the prompts equal string for string, the tokens equal (ours add
BOS and EOS where node-llama-cpp adds them after tokenizing), and the
vectors equal to a cosine above 0.9999 on an arm64 CPU (0.99975 on x86,
where node-llama-cpp's prebuilt picks AVX2 kernels and our build keeps to
portable ones) and above 0.999 between our Metal and qmd's. The x86 build
is slow for that reason; giving it ggml's run-time choice of CPU kernels
is left for when a Linux machine embeds for real.

**qmd embedded every input over 512 tokens wrongly**, a known defect
upstream (tobi/qmd#897, with the fix in tobi/qmd#993, both open). node-llama-cpp
gives an embedding context a batch of at most 512 tokens unless told
otherwise (`getDefaultContextBatchSize`), qmd does not tell it, and
llama.cpp then evaluates a longer input in 512-token pieces and pools
only the last piece. embeddinggemma attends both ways, so the earlier
tokens never see the later ones and the vector stands for the input's
tail. qmd's chunks are up to 900 tokens, so most of its long chunks were
embedded so. Ours takes the whole input in one batch; on a 761-token
input the two agree to a cosine of 0.84, and evaluated in qmd's pieces
ours reproduces qmd's to 0.9999
(`qmd_embedded_an_input_over_512_tokens_from_its_last_batch_only`). The
re-embed the upgrade does anyway fixes them. One commenter on #897
checked the whole-input vector against Google's own sentence-transformers
reference and found them equal to a cosine of 1.0000.

**Rate.** 761-token inputs, one at a time, on an M4 (`measure_rate`, an
ignored test): 2.5 a second on the CPU's 10 cores, 12.5 on Metal, which
is qmd's ~12. Evaluating several inputs in one batch has not been tried.
The shim picks the GPU itself and reports it (`Embedder::device`, `MTL0`
on a mac), and `a_mac_runs_the_model_on_metal` holds it there.

**Drop an embedder before the process exits.** On Metal, ggml asserts at
exit that no model is still loaded and aborts the process if one is, so
an embedder must never live in a static, including the search applet's
long-lived query embedder.

### Search moving over

Built, first behind a switch so both could be measured on one root,
and now the only index search reads
(`unified_document_index/src/search_index.rs`). Words is BM25 over
messages (`search_index::read`).
Meaning holds every chunk's vector from the inference cache in memory,
read again once the index and the cache have sat still, and embeds the
query in the applet's own process with the model the vectors were made
with. Both hand on the same hits as qmd's, each placed by its bytes in
the document, so the other terms, fusion and landing are unchanged. The
page that shows a hit still reads its file, once, to land it on a message
and quote it; that read goes when rendering on demand does.

**Measured against qmd** (2026-11-12). qmd on a clone of
`stay_alive_1` as it is, its vectors whole; the search index on a clone
re-rendered by this build (176k documents, 913k messages), with its
smaller sources embedded (every source but the two calendars, both Slacks,
Notion, Gmail and Claude Code). The private eval set; one `-c opt` applet
for every run.

Words alone (`semantic:off`), all 357 queries:

| | @1 | @10 | MRR | lands within 2 messages | p50 | p90 |
|---|---|---|---|---|---|---|
| qmd | 37% | 66% | 0.47 | 74% | 125 ms | 453 ms |
| search index | 32% | 59% | 0.42 | 86% | 104 ms | 988 ms |

The search, Meaning on, the 195 queries whose target is in an embedded
source, each scoped to that source:

| | @1 | @3 | @10 | MRR | lands within 2 messages | p50 | p90 |
|---|---|---|---|---|---|---|---|
| qmd | 75% | 92% | 97% | 0.84 | 73% | 50 ms | 184 ms |
| search index | 83% | 95% | 97% | 0.89 | 80% | 44 ms | 188 ms |

Words alone finds less, as expected: a thread whose words are in
different messages is no hit. With Meaning on it ranks better and lands
better than qmd, at the same speed; whole chunks embedded (qmd embedded
one over 512 tokens from its last 512) and chunks that follow messages
are the likely reasons. The Words tail is searches with `from:`, and
it was the grid's, not either index's: on one clone with one build, the
93 `from:` searches took 71 s on this index and 92 s on qmd (43 s and
51 s again warm), and the one statement over a second in both was the
grid's lookup of who `from:x` names, which read all 1.09M search-term
values for a bare word (1.5–2 s each under load). The values now have a
trigram index (`vals_tri`, 142 MB, under 10 ms a lookup); the p90 in
the table above predates it. Measuring the whole set with Meaning on
waits for the large sources to be embedded.

Two things the eval found on the way: a chunk of dense text can run past
the model's 2,048-token context within its 3,600 bytes (35 of 21k here),
which failed a whole source until such a chunk was cut to fit as qmd
does; and an eval of qmd on a re-rendered clone measures Words alone, as
its vectors name the old text.

**Not an ANN index, yet.** doltlite 0.51.3's amalgamation carries
SQLite's `vec1` extension (an IVF index with product quantization,
cosine or L2, version 0.7), compiled out unless the build sets
`DOLTLITE_VEC1=1`; ours does not. What it does is in
[`doltlite.md`](../doltlite.md) § "Vector search: vec1", each fact a
test in `vec1_facts_test` against a test-only shell built with it. For
this plan: a filter on a column of the vec1 table itself (a `source_id`
carried there) is applied before the nearest are chosen, but a filter
through another table's index, a `rowid IN (…)` or a join, is applied
after, and loses rows; a trained index ranks by compressed codes and
finds a filtered row only in the buckets it probes; and ranking by
`vec1_l2_distance` over rows an ordinary index found is exact. Brute force over vectors in memory is
exact and, at a million 768-wide vectors, about 0.7 billion
multiply-adds a query, on the order of 0.1 s on one core (an estimate,
not yet measured);
what it costs is memory (2.9 GB as `f32`, half as `f16`). An index is
worth measuring when that memory is the problem, not before. Snippets are
the one thing still read from the file: the few lines of the landed
message a page shows, read only for those rows, as today.

## One file with the grid index

**Every table the search reads goes into the grid index's file**
(`unified_document_index/grid_index/db.sqlite`): the keyword rows and
the chunks beside `grid_rows`, whose columns are the
filters (`source:`, `kind:`, `from:`, dates). One query can then narrow
by a filter through `grid_rows`' own indexes and rank what is left by
words or by meaning, in SQL, reaching the vectors in the inference
cache's file through `ATTACH`. An ANN table such as vec1's is built from
the cache into the grid index's file, and can always be built again. That is also the precondition for vec1
(`doltlite.md` § "Vector search: vec1"), with one more condition: vec1
applies a filter before ranking only when the column is in the vec1
table itself, so the filters an ANN query needs are copied into it,
per chunk. A chunk spans messages, so only what is the same for every
message of a document copies cleanly (source, kind, account, the
document's date range); a per-message filter such as `from:` goes
through the exact path, a join to `grid_rows` and `vec1_l2_distance`
over what it keeps.

The keyword rows key on the message: the section's uuid is its
`grid_rows.uuid`, so `search_fts` joins straight to the row it lands
on.

Built, with two changes to the grid index to make room:

- **A change of shape drops the owner's own tables, not the file.** An
  index written in another shape used to be deleted and rebuilt, which
  would now also cost every source's keyword index. The grid records its
  shape in the file's `_datalib_meta` as before; the search index under
  its own key there (`search_index_schema_hash`,
  `datalib_store_meta::part_schema_hash_in`). Each empties only its own
  tables: the search index the `search_…` ones, the grid every other.
  The vectors are not at risk either way: they are in the cache's file.
- **Several writers share it.** `grid_index` writes it in one
  transaction per source; each source's `keyword_index` writes batches
  of 200 documents, and removes in batches of 500. Every write
  transaction begins `IMMEDIATE`, so each waits for the other's lock
  rather than failing when the other commits between its read and its
  write. Measured on `stay_alive_1`'s run store (2026-11-13), an
  ordinary sync's `grid_index` holds the file at most 35 s for one
  source; one that loads a large source whole holds it many minutes, so
  the search index waits up to an hour and the grid five minutes. The
  embed step's writes go to the cache's file and wait on nothing.
- **The old file goes.** A `keyword_index` or the aggregator deletes
  `search_index/index.sqlite` where an earlier build left it, and the
  first `keyword_index` of each source fills the new tables; the cache
  stays where it was.

## Forward path for existing data

Everything here is derived, so it is rebuilt, not migrated. The first
sync after the upgrade builds the new index from the files and
re-embeds every document: about 21 hours on this mac for its 930k
chunks, at the ~12 chunks/s qmd measured, and the Meaning tab answers
for what is embedded so far. Words search is whole within the first
`keyword_index` of each source. The aggregator, `qmd_aggregator` still
by its id, deletes qmd's directory on its first run.

**What a person notices:** the 6.9 GB index becomes roughly the vectors
plus a smaller keyword index (about 3.8 GB here, less if the vectors are
stored as `f16`, which the eval decides). Words hits land on the message
that matched. Meaning search fills back in over the first day.

## Order of work

1. Built: facts test for contentless FTS5 with `contentless_delete`, and
   `bm25()` over it, on the SQLite we link.
2. Built: `search_index`, with the schema, `IndexDoc` → sections and
   chunks, the owed comparison, put and remove, unit-tested on made-up
   documents.
3. Built: the embedder in Rust, llama.cpp `b10361` under Bazel on the CPU
   and Metal, a binding, qmd's prompt format, the parity tests against
   qmd above.
4. Built: `markdowns.content_hash` in the render store.
5. Built: the inference cache, chunks keyed by their prompt's hash, and
   a heading starts a chunk.
6. Built: the search tables move into the grid index's file; each owner
   empties only its own tables on a change of shape.
7. Built, beside qmd's: `keyword_index` writes the new index after qmd's
   update; `embed` fills the inference cache after qmd's embed, only
   where the config sets `embed_search_index` (off by default, since
   search does not read it yet); the aggregator drops a source it no
   longer reads and sweeps the cache (`datalib_step/src/search_index.rs`).
8. Built: search reads the new index and embeds a query in process,
   where the applet's entry set `DATALIB_SEARCH_READS`;
   `search_eval.py` against qmd (below).
9. Retire qmd. Built: search reads only the new index; `keyword_index`
   and `embed` write only it, embedding every source (the config keys
   `embed_search_index` and `embed_contexts` only warn now); the
   aggregator deletes qmd's directory; the embedding map lays out the
   inference cache's vectors; the TNG fixture builds the search index by
   running `keyword_index`, and its vectors in an action of their own
   that reruns only when a chunk's prompt changes. Not built: deleting
   `qmd_indexer`, `qmd_sdk.mjs`, the qmd code under
   `unified_document_index/src/qmd/`, qmd in the Node runtime and the
   release, `third-party/qmd`, and the names that still say qmd (the
   `qmd_aggregator` step id, the `qmd_keyword` and `qmd_embed` locks,
   `/qmd_state` and the `qmd_*` fields of a search's answer).

## Later

- **Render email and Slack on demand.** Both make one document per
  thread, the thread key is a stored, indexed raw column, and
  `markdowns.bucket_key` names the thread, so rendering one document is
  rendering one bucket (`render_diff::collect` already renders chosen
  buckets into memory). The `.md` files then stop being written, the
  index steps take their `IndexDoc` from that render, and `/chat` renders
  the document it is asked for.
- **Photos in Meaning search.** Lightroom writes no markdown, and now
  needs none to be searched: its render can hand the indexer one
  `IndexDoc` per photo, built from its `NormalizedPhoto`
  (`photo_schema`). The title is the file name with its folder relative
  to the catalog; the text lays out, one labelled line each, what a
  person would search a photo by: its title, headline and caption, when
  it was taken (with its offset), where (location, city, state,
  country), the people tagged in it, its keywords with their hierarchy,
  the albums it is in, and the camera. One photo is one section and,
  short as it is, one chunk, so its inference hash covers exactly that
  text: a photo re-embeds when a field in it changes, and every photo
  re-embeds when the layout of the text does. The same text feeds the
  keyword index, so "Lisbon 2019" finds a photo by its place and year.
