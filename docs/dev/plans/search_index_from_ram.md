# Our own search index, fed from memory

**Status: proposal (2026-11-11). Steps 1 and 2 of "Order of work" are
built (`//datalib/backend/search_index`), and step 3 on the CPU
(`//datalib/backend/embedder`); the rest is not.** Numbers were
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
  their own. BM25's statistics stay global across sources, as now.
- **The model and the prompt format**: the pinned embeddinggemma GGUF and
  qmd's `title: … | text: …` format, so vectors mean what they meant. What
  runs the model moves from node-llama-cpp into Rust ("Embedding in
  Rust", below).
- **The step graph.** `keyword_index` and `embed` per source, an
  aggregator that drops a source it no longer reads.

## The index

A new crate, `search_index`, owns one plain SQLite file in WAL mode
(`unified_document_index/search_index/index.sqlite`), like the grid
index: not doltlite, rebuilt from the render stores whenever it is lost.
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

- `documents(doc_id INTEGER PRIMARY KEY, source_id, markdown_uuid UNIQUE,
  content_hash)`.
- `sections(section_id INTEGER PRIMARY KEY, doc_id, seq, uuid, start_byte,
  end_byte)`: each message's place in its document. The title is indexed
  once, on the first section's row.
- `sections_fts`: FTS5 with `content=''` and `contentless_delete=1`,
  columns `title, body`, rowid = `section_id`, one row per message. A
  contentless table keeps the inverted index and drops the stored copy;
  `bm25()` still works on it, `snippet()` and `highlight()` do not, and we
  use neither. `fts_facts_test` holds each fact on the SQLite we link,
  doltlite 0.51.3's.
- `chunks(doc_id, seq, start_byte, end_byte, first_section,
  last_section)`: what each embedded chunk covers.
- `vectors(doc_id, seq, vector BLOB)`. Still to come with the embedder: a
  one-row `embedding_model` naming the model pin and a fingerprint of the
  chunker and the prompt format; a change to either empties `vectors`.

A file in another shape is emptied under the write lock, so two steps
opening an old index together empty it once.

A document with no messages (a Notion page, a calendar event) is one
section with no uuid.

### Words: ranked per message, matched per document

A row per message lands a hit on the message that matched, with no file
read. A search for words spread across a thread still has to find that
thread, so a multi-word query matches each required word or phrase on
its own, against messages, and joins on `doc_id`:

- a document matches when every required term matches some message of
  it, and no excluded term matches any;
- its score is the sum, over terms, of the best `bm25()` any one of its
  messages got for that term;
- it lands on the message holding the most terms, which is what `land`
  computes from the file today.

A one-word query is a single `MATCH`. A phrase still has to sit inside
one message, which a phrase across two messages never meant anyway.
The ranking moves: BM25 over messages rather than whole threads favours
a short message that says the word. `scripts/search_eval.py` measures
it against qmd on the private eval set before qmd goes.

### Meaning: chunks follow messages

A chunk is whole messages packed up to 3,600 bytes, qmd's 900 tokens at
its four characters a token; only a message longer than that is cut
inside itself, at the last paragraph, line, sentence or word break, and
its pieces overlap by 540 bytes (qmd's 15%). Whole messages need no
overlap. A hit then names its messages from `chunks`, and `land` reads
no file.

### Writing it

**The source's steps read the rendered file into an `IndexDoc`**: the
`.md` plus `markdowns.section_layout`, which already holds each
section's uuid and byte length. That is the simplest place, and the one
that changes when rendering on demand lands: the `IndexDoc` will then come
from a render in memory instead.

**What is owed is a query.** The step compares `(markdown_uuid,
content_hash)` from the source's render store with the index's
`documents`: new or changed documents are indexed, gone ones deleted.
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
we call: load a model, tokenize, embed a batch. The CPU backend first;
Metal after, if the measured rate asks for it.

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

**Built, on the CPU: `datalib/backend/embedder`**, a C shim over
`llama.h` (`csrc/embedder.cpp`) and a Rust crate over it, with llama.cpp
built by `third-party/llama_cpp/llama_cpp.BUILD`. `parity_test` runs
qmd's own `formatDocForEmbedding`, `formatQueryForEmbedding`, tokenizer
and `embed` through the pinned node and package, on made-up texts, and
finds the prompts equal string for string, the tokens equal (ours add
BOS and EOS where node-llama-cpp adds them after tokenizing), and the
vectors equal to a cosine above 0.9999 on the CPU and above 0.999
against qmd on Metal.

**qmd embedded every input over 512 tokens wrongly.** node-llama-cpp
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
re-embed the upgrade does anyway fixes them.

**Rate on the CPU is too low; Metal is next.** 761-token inputs embed at
2.5 a second on an M4's 10 cores (`measure_rate`, an ignored test),
against qmd's ~12 a second with Metal. ggml's Metal backend
(`ggml/src/ggml-metal`, with its shader library embedded in the binary)
goes into the same build, used where the machine has it and checked
against the CPU's vectors by the same tests.

### Search moving over

`keyword_hits` becomes the per-term query above. `VectorCache` reads
`vectors` blobs, with no sqlite-vec layout to decode, and still ranks
every vector held in memory. A hit carries its section, and
`GridIndex::land` stops reading files.

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

## Forward path for existing data

Everything here is derived, so it is rebuilt, not migrated. The first
sync after the upgrade builds the new index from the files and
re-embeds every document: about 21 hours on this mac for its 930k
chunks, at the ~12 chunks/s qmd measured, and the Meaning tab answers
for what is embedded so far. Words search is whole within the first
`keyword_index` of each source. Once the new index serves, the step that
replaces `qmd_aggregator` deletes qmd's directory.

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
3. The embedder in Rust: llama.cpp `b10361` under Bazel, a binding,
   qmd's prompt format, the parity tests against qmd above. Built on the
   CPU; the Metal backend is left, and its rate against qmd's.
4. `markdowns.content_hash` in the render store.
5. `keyword_index` and `embed` write the new index, embedding in
   process; the aggregator drops a removed source's rows.
6. Search reads the new index and embeds a query in process;
   `search_eval.py` against qmd.
7. Retire qmd's update and embed, `qmd_sdk.mjs`, its tables and the
   `qmd_keyword` lock.

## Later

- **Render email and Slack on demand.** Both make one document per
  thread, the thread key is a stored, indexed raw column, and
  `markdowns.bucket_key` names the thread, so rendering one document is
  rendering one bucket (`render_diff::collect` already renders chosen
  buckets into memory). The `.md` files then stop being written, the
  index steps take their `IndexDoc` from that render, and `/chat` renders
  the document it is asked for.
- **Photos in Meaning search.** Lightroom writes no file; its render
  could hand the indexer a small `IndexDoc` per photo.
