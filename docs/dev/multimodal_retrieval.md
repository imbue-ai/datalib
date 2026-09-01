# Design: multimodal retrieval layer for datalib

**Status:** Draft / proposal. Nothing here is built.
**Scope:** Replace the `qmd_index` step for high-volume sources with a
purpose-built retrieval layer supporting arbitrary metadata prefiltering,
pluggable vector spaces, and a bounded bytes-at-rest budget.

**Revision note (2026-09-01).** Every claim about *this* tree in the
previous draft has been checked against the code and against a real data
root; several were wrong and are corrected inline. Claims about `qmd` and
`sqlite-vec` are still largely from their documentation — those are
marked. Per [`AGENTS.md`](../../AGENTS.md), treat an unmarked "we now do
X" as verified and a marked one as not.

---

## 1. Problem

datalib exposes three read surfaces over mirrored data:

- **SQL** — the `grid_rows` union table (one row per message/document,
  with `provider`, `kind`, `when_ts`, `author`, `channel`,
  `conversation_uuid`, `text`, `entire_chat`).
- **Markdown** — `<name>/rendered_md/`, one document per
  conversation, plus a sibling `blobs/` directory per document.
- **Semantic search** — the `qmd_index` step, a QMD SQLite index over
  the rendered markdown.

These are three parallel views with no join between them. There is no way
to combine an **arbitrary boolean predicate over `grid_rows` metadata**
with **vector or BM25 retrieval**. That is the capability this design
adds.

Four forcing constraints:

1. **Arbitrary metadata prefilter.** Not a partition. A boolean over
   several fields (`author`, `kind`, `when_ts` ranges, `channel`,
   `provider`). QMD's only pushed-down filter is `collections`.
2. **Scale.** ~250k emails, threaded into an estimated 25–40k documents.
3. **Multiple vector spaces.** Text embeddings now; CLIP over media
   later. Different dimensionality, different geometry, non-comparable
   distances.
4. **Bytes at rest.** The current pipeline stores the same text five
   times and attachment bytes at least twice, uncompressed. At the mail
   corpus's scale that stops being an aesthetic complaint. §4 is new in
   this revision and is the section with the most measured content.

Constraint 3 determines the *interface*. Constraint 4 determines the
*storage schema*. The other two could have been worked around.

---

## 2. Decision: leave QMD, scavenge for parts

QMD (`tobi/qmd`, MIT) was a stand-in and is not the right long-term fit.
Every friction point traces back to its origin as a personal-notes tool:

| QMD assumption | Why it fails here |
|---|---|
| Documents are files on disk; the only indexing entry point is `update()`, which scans the filesystem | Forecloses ever *not* materializing markdown (§4.5), and forces 250k emails to exist as files before they can be indexed |
| Filtering is collection-scoped | Cannot express an arbitrary boolean over metadata |
| One global `vectors_vec` table keyed `hash_seq` | Cannot hold two vector spaces |
| `vec0` is brute-force with no ANN index | Every query scans the full float32 corpus |
| Stores the document body twice internally (§4.2) | 2.2× the source text before a single posting is written |
| TypeScript + node-llama-cpp, invoked via `npx` | Foreign to datalib's Bazel/Rust stack; inherits bun-vs-node ABI and launcher issues |

Forking was rejected: datalib consumes QMD as a pinned npm package
(`DEFAULT_QMD_VERSION` in
[`unified_index/src/qmd/mod.rs`](../../datalib/backend/unified_index/src/qmd/mod.rs)),
so a fork forces datalib onto that fork, and the files needing permanent
ownership (`store.ts` search SQL, the indexing path) are exactly the ones
churning upstream.

### What we take

**The query expansion model — the one genuinely hard-to-reproduce
artifact.** `tobil/qmd-query-expansion-1.7B-gguf`. GRPO-trained to emit
*typed* expansions (`lex:` for BM25, `vec:`/`hyde:` for vector search)
under JSON-schema grammar constraints, scored during training on
named-entity preservation, format compliance, and diversity.

> **Action:** mirror the GGUF into our own artifact storage. It lives in
> one person's HuggingFace account.

**The `finetune/` directory.** Training pipeline, reward function, data
prep. It is Python, which matters for us. Mail expansions are a different
distribution from notes expansions (more proper nouns, thread subjects,
"that thing about X from someone at Y"), so retraining on our own corpus
is plausible.

**The recipes, which are documentation rather than code.**

- Chunker break-point scoring: H1=100, H2=90 … blank line=20, list
  item=5, line break=1, with a squared-distance decay over a 200-token
  window before the cutoff, and code-fence protection.
- RRF at k=60, 2× weight on the original query, top-rank bonus (+0.05
  for #1, +0.02 for #2–3).
- Position-aware blending of retrieval vs reranker: ranks 1–3 → 75/25,
  ranks 4–10 → 60/40, ranks 11+ → 40/60.
- EmbeddingGemma prompt formats: `task: search result | query: {q}` for
  queries, `title: {t} | text: {c}` for documents.

Treat the fusion constants as **priors from someone else's corpus**, not
answers. They get re-tuned against our eval fixture (§9).

**Not taken:** the embedding and rerank models themselves (pull from
HuggingFace directly), the CLI, the MCP server, the collection/context
system, the storage schema.

---

## 3. Architecture

### 3.1 Core abstraction

```
search(filter: Predicate, query: Query, k: int) -> RankedCandidates
```

The caller does not know what kind of vectors are underneath. `Predicate`
resolves against `grid_rows` and is **vector-space agnostic** — it
restricts a set of documents; whether those documents have text vectors,
image vectors, or both is orthogonal. This is the interface to get right
now, while there is only one vector space behind it.

### 3.2 Pipeline

```
                      query
                        │
              ┌─────────┴─────────┐
              │  query expansion  │  (typed: lex / vec / hyde / caption)
              └─────────┬─────────┘
                        │
   filter ──────────────┤
      │                 │
      ▼                 ▼
 ┌─────────┐   ┌────────┴────────┬──────────────┬─────────────┐
 │grid_rows│   │                 │              │             │
 │ resolve │──▶│  FTS5 (BM25)    │ text vectors │ CLIP vectors│  ← N spaces
 │ → doc   │   │  (contentless)  │  (2-stage)   │  (2-stage)  │
 │   set   │   └────────┬────────┴──────┬───────┴──────┬──────┘
 └─────────┘            │               │              │
                        └───────┬───────┴──────────────┘
                                ▼
                        ┌───────────────┐
                        │  RRF fusion   │  rank-based, parameter-free
                        └───────┬───────┘
                                ▼
                        ┌───────────────┐
                        │   reranker    │  text-only (see §3.6)
                        └───────┬───────┘
                                ▼
                     hydrate top-k from the
                     canonical markdown (§4.4)
                                │
                                ▼
                         ranked results
```

RRF is the right fusion primitive here specifically because it is
rank-based and handles missing signals gracefully. A thread with no
attached images contributes nothing to the image list rather than scoring
zero.

The **hydrate** step is new in this revision and is load-bearing for §4:
no backend stores document text, so snippet text is read back from the
canonical copy for the ~20 rows that survive fusion.

### 3.3 Prefilter

The predicate resolves against `grid_rows` to a set of document
identifiers, which are pushed **into** each retrieval backend's SQL —
never applied to its output.

Not a theoretical concern. QMD's own changelog documents the failure at
collection granularity: searching globally and post-filtering meant a
large unrelated collection filled the FTS/ANN top-k, so requested
collections vanished entirely, producing false-empty results even though
each collection matched on its own. Any selective filter applied after
retrieval hits the same wall.

### 3.4 Vector search: two stages, and what each stage is *for*

**Correction to the previous draft.** It presented binary quantization as
the fix for both latency and size, and made it deferrable to M4. The
measurements in §4.2 say the framing was wrong in both directions:

- Two-stage binary→float32 **adds** storage. Stage 2 rescores against the
  full float32 vectors, so both representations must exist: `bit[768]`
  (96 B) *plus* `float32[768]` (3,072 B) = 3,168 B/chunk. It is a
  **latency** optimization, not a storage one.
- Vectors are nonetheless the **largest** term in the index — 15.75 MB of
  a 34.21 MB index in the measured corpus, 2.7× the source text. The
  storage lever is not binary, it is **int8** (768 B/chunk, 4×), or
  accepting binary-only and eating the recall loss.

Per 100k chunks:

| representation | bytes/chunk | 100k chunks | notes |
|---|---|---|---|
| float32 | 3,072 | 307 MB | what qmd stores today |
| int8 | 768 | 77 MB | typical recall cost ~1%; measure it (§9) |
| bit (binary) | 96 | 9.6 MB | stage-1 filter only |
| binary + float32 | 3,168 | 317 MB | fast, largest |
| binary + int8 | 864 | 87 MB | **recommended default, pending §9** |

So: keep the two-stage design for latency, and make the **stage-2
representation** the tunable. `sqlite-vec` supports `bit[N]`, `int8[N]`,
and `float32[N]` natively with `vec_quantize_binary` /
`vec_quantize_int8`. *(sqlite-vec capabilities are from its release notes
and ARCHITECTURE.md, not from reading its source.)*

Because the filtered candidate set is small by stage 2, an exact scan
stays viable. This is the same escape hatch QMD uses: it exact-scans a
collection's vectors with `vec_distance_cosine` when the set is within
20k rows, precisely because ANN plus post-filter cannot see rows that
never enter the global top-k, and sqlite-vec caps `k` at 4096.

**Rejected:** `vec0` metadata columns as the primary filter mechanism.
Supported operators are only `=`, `!=`, `>`, `>=`, `<`, `<=`, `BETWEEN`
— no `LIKE`, `IS NULL`, or scalar functions. Insufficient for an
arbitrary boolean. Partition keys on `provider`/`kind` remain available
as a coarse shard if measurement justifies the re-embed cost.

### 3.5 Fingerprinting — required from day one, and wider than the previous draft said

Every vector row carries a fingerprint, and **the fingerprint is part of
the primary key**, not a nullable column added later.

The previous draft scoped the fingerprint to `(model URI, prompt format,
chunking parameters, quantization)`. §4.4 makes the index store *offsets
into rendered markdown* rather than text, which means a renderer change
silently invalidates every offset. So the fingerprint must also cover:

- **`renderer_version`** — already an existing concept, stored on
  `markdowns.renderer_version` and documented as invalidating every
  cached render at once.
- **`row_set_hash`** — the existing per-document content key on
  `markdowns`.

Together those two already define "is this document's rendered text the
same text I indexed?" We do not need to invent a freshness key; we need
to stop throwing away the one that exists.

QMD retrofitted a narrower version of this and it shows: `qmd doctor`
explicitly warns when it finds multiple non-empty fingerprints in one
table. With one text model that is a nicety. With text + CLIP + a
markdown renderer under it, an unfingerprinted vector is unidentifiable.

### 3.6 Reranking

Qwen3-Reranker is text-only. A CLIP hit has no text to score. Two
options, to be decided by evaluation rather than argument:

- **A:** rerank text candidates only; merge image hits by fused rank.
- **B:** rerank image hits against surrounding text (filename, alt text,
  the message body they were attached to). Weaker signal, one ranked
  list.

Two implementation notes carried over from QMD:

- Deduplicate identical chunk texts before reranking and cache scores by
  **content hash**, not path. For threaded mail with quoted material this
  is closer to a correctness win than a performance one.
- Cap parallel rerank contexts. QMD caps at 4 and sizes the embedding
  context pool from the weight file, after an issue where assuming 150MB
  per context exhausted an 8GB card with a model needing ~1190MB per
  2048-token context.

### 3.7 Query expansion and routing

Keep expansions **typed** and route them exclusively: `lex` → BM25/FTS
only, `vec`/`hyde` → text vectors only, with the original query sent to
both. Untyped expansion sprays every variant at every backend.

**Add a fourth type for CLIP: `caption`.** CLIP's text tower is trained
on short captions with a 77-token limit and underperforms on long natural
-language queries. A short caption-shaped expansion variant, routed
exclusively to the CLIP index, is the fix — a routing-table change, not
an architecture change, which is the payoff for keeping expansion typed.

---

## 4. Bytes at rest

*This section is new. It is the part of the design with the most measured
content and the least borrowed reasoning.*

### 4.1 Measured baseline

Real data root (`~/datalib/thad_imbue_dev`), 2026-09-01. 1,514 rendered
documents, 4,496 grid rows, **5.90 MB** of markdown text across all
sources — the denominator for everything below.

| store | on disk | × text | what it holds |
|---|---|---|---|
| `*/rendered_md/**.md` | 5.90 MB | 1.00× | the rendered text |
| `*/rendered_md/**/blobs/` | 8.44 MB | — | **second copy** of attachment bytes |
| `unified_index/grid/db.doltlite_db` | 11.82 MB | 2.00× | `grid_rows.text` = 6.34 MB, + metadata, + 3 commits of history |
| `unified_index/qmd/index.sqlite` | 34.21 MB | 5.80× | broken out below |

Whole root: 3.6 GB, of which `slack/raw` alone is 3.2 GB
(`blobs.doltlite_db` 2.81 GB + `entities.doltlite_db` 587 MB).

**The mail corpus this design targets is not ingested yet.**
`fastmail/raw` is 976 KB. Every 250k-email number in this document is a
projection, not a measurement — see §6 item 4.

### 4.2 Where qmd's 34 MB goes

By `dbstat`, on a copy of the live index:

| segment | bytes | what it is |
|---|---|---|
| `vectors_vec_vector_chunks00` | 15.75 MB | 4,992 chunks × 768-dim float32 (3.16 KB/chunk) |
| `documents_fts_content` | 6.46 MB | **verbatim copy of every body** |
| `content` | 6.39 MB | qmd's own copy of every document (5.12 MB logical) |
| `documents_fts_data` | 2.13 MB | the actual inverted index |
| `content_vectors` + its index | 1.81 MB | chunk offsets and metadata — no text |
| everything else | ~1.7 MB | rowid maps, document rows, indexes |

Two findings worth stating plainly:

**qmd stores the body twice.** `documents_fts` is declared
`CREATE VIRTUAL TABLE documents_fts USING fts5(filepath, title, body,
tokenize='porter unicode61')` — with no `content=` option, so FTS5
maintains its own shadow copy in `documents_fts_content` *in addition to*
`content.doc`. That is 12.85 MB of duplicated text against 5.90 MB of
source, and it was not in the previous draft's accounting.

**The genuinely irreducible index is 2.13 MB.** Everything else is either
a copy of text we already have (12.85 MB) or vectors (15.75 MB).

### 4.3 The full copy count

For one email thread, today, end to end:

| # | copy | store |
|---|---|---|
| 1 | `.eml` bytes (canonical backup, incl. base64 MIME parts) | `<src>/raw/blobs.doltlite_db` |
| 2 | rendered markdown | `<src>/rendered_md/**.md` |
| 3 | per-message body text | `grid_rows.text` |
| 4 | document text | qmd `content.doc` |
| 5 | document text again | qmd `documents_fts_content` |

Plus attachment bytes twice — base64 inside the `.eml` (≈1.37× inflation)
**and** extracted into `rendered_md/<thread>/blobs/`.

The previous draft said "four times". Five is the count, and the ratio is
worse than the count suggests: **54.5 MB of derived stores over 5.90 MB of
text, ≈9×** — before a single email is ingested.

Two mechanisms deserve naming because both are one-line facts in the
code:

- [`blob_cas.rs:642`](../../datalib/backend/etl/src/blob_cas.rs)
  `materialize_to_dir` is a plain `std::fs::write` into
  `<page_dir>/blobs/`, deduplicated **only within one page bundle**. An
  image attached to N threads is written N times.
- **Nothing compresses.** No `zstd`/`gzip`/`flate` anywhere in
  `blob_cas.rs` or `doltlite_raw.rs`, and doltlite does not compress
  either: `fastmail/raw/blobs.doltlite_db` holds 523,028 bytes of
  `cas_objects.bytes` in a 532,597-byte file — 1.018× overhead, i.e.
  stored raw.

### 4.4 Decision: one canonical copy of the text, offsets everywhere else

**The rendered markdown file is the single canonical copy of derived
text. Every downstream store holds offsets, hashes, or postings — never
bytes.**

Four changes, listed in ascending order of risk:

**D1 — the retrieval index stores no document text.** Declare FTS5
contentless (`content=''`, with `contentless_delete=1`), and store chunk
`(markdown_uuid, byte_offset, byte_len)` rather than chunk text. Snippets
are computed in Rust by reading the canonical `.md` for the ~20 rows that
survive fusion — which is exactly what
[`db.rs::snippet`](../../datalib/backend/unified_index/src/db.rs) already
does for the grid today (240-char window centred on the first match).
*Removes copies 4 and 5: 12.85 MB in the measured corpus.*

> Note the alternative: FTS5 `content='<table>'` (external content) also
> stores zero duplicate text *and* keeps `snippet()`/`highlight()`
> working — but only if the canonical text is a **SQL table**. It is not,
> under this decision. That asymmetry is the single strongest argument
> for the in-table variant considered and rejected in §4.5.

**D2 — compress at rest.** Orthogonal to this entire design, and
plausibly the best value-per-line item in the document. Mail text and
`.eml` bodies compress ≈4:1 with zstd. Applies to the raw CAS
(the 3.2 GB term), and to the markdown if it moves into a table.
*Should not wait for M6, or for any of this.*

**D3 — attachments live in the CAS only.** Stop materializing
`<page_dir>/blobs/`. The `asset` endpoint
([`applets/src/unified_index/mod.rs:585`](../../datalib/backend/applets/src/unified_index/mod.rs))
already resolves `markdown_uuid` + a relative path and already has a
path-traversal guard; point its resolution at a CAS `blake3` instead of a
sibling file. Cross-document dedup comes free.
*Removes 8.44 MB in the measured corpus; for a photo-heavy mail corpus
this is the largest single line.*

> **Tradeoff to name, not to hide:** the rendered tree stops being
> self-contained. Today you can copy `rendered_md/` to another machine,
> or point Quarto at it, and images resolve. That is a real property of
> the product. Mitigation: an explicit `datalib export` that materializes
> `blobs/` on demand — opt-in, rather than paid on every sync.

**D4 — `grid_rows.text` becomes a span, not a copy.** Replace the
`LONGTEXT` with `(markdown_uuid, byte_offset, byte_len)`. Both ends of
this already exist: the renderer emits
`<div id="m-{uuid}" data-section-uuid="{uuid}">` wrappers delimiting
exactly these spans, and
[`qmd/mapping.rs:194`](../../datalib/backend/unified_index/src/qmd/mapping.rs)
already reads the `.md` back off disk to resolve a hit line to a section
uuid. *Removes copy 3: 6.34 MB.*

> **This is the risky one, and it should be staged last.** The grid's
> free-text filter is `LOWER(text) LIKE ?` with a `%needle%` bind
> ([`db.rs:184`](../../datalib/backend/unified_index/src/db.rs)) — a full
> scan over `grid_rows.text`. Spans cannot serve that, so the grid's
> free-text path must move onto the retrieval layer's FTS. That is a
> better query, not a worse one, but it couples the grid to a component
> that does not exist yet. **D4 is optional:** keeping `grid_rows.text`
> costs exactly one extra copy of the text, which after D1–D3 is no
> longer the dominant term.

Target state, measured corpus, D1–D4 applied and int8 stage 2:

| store | before | after |
|---|---|---|
| rendered `.md` | 5.90 MB | 5.90 MB |
| `rendered_md/**/blobs/` | 8.44 MB | 0 |
| grid db | 11.82 MB | ~5.5 MB |
| retrieval index | 34.21 MB | ~6.5 MB (2.1 postings + 3.8 int8 + 0.5 binary) |
| **total derived** | **60.4 MB** | **~17.9 MB** |

### 4.5 Should markdown be materialized at all?

This is the question the previous draft did not ask. Taking it seriously:

**The enabler already exists.** `markdowns.row_set_hash` +
`markdowns.renderer_version`
([`schema/src/markdowns.rs`](../../datalib/backend/schema/src/markdowns.rs))
already model rendering as a **pure function with a cache key** — ingest
recomputes `row_set_hash` from the canonical grid-row tuples and re-emits
the file only on mismatch. "Render on the fly" is that same cache with
capacity zero. The architecture is already there; only the policy is
hardcoded.

**QMD is what forecloses it.** Its only indexing entry point is a
filesystem scan, so while qmd is the index, the files must exist.
Leaving qmd is what turns this into a choice at all — which is an
independent argument for §2 that the previous draft did not make.

**What breaks under on-demand rendering:**

- Snippet hydration (§3.2) must re-render the top-k at query time.
  Cost per document ≈ read raw rows + mail-parse the `.eml`. For k=20,
  parallel, probably tolerable; unmeasured.
- The index stores offsets into a render that no longer exists, so a
  `renderer_version` bump invalidates every offset in the index — not
  just every file on disk. This is why §3.5 widens the fingerprint.
- The human-readable mirror disappears. For a project whose pitch is
  "your data, as files you can read," that is a product decision, not an
  implementation detail.

**Recommendation: keep markdown materialized by default; make it a
per-source policy, not an architecture.**

```toml
[[steps]]
id = "fastmail/rendered_md"
params = { render_mode = "materialized" }   # or "on_demand"
```

The reasoning is the arithmetic in §4.4: after D1–D3, the canonical
markdown is **1× the text** and the *smallest* remaining term. The terms
that dominate are the `.eml` bytes (D2's target) and the vectors (§3.4's
target). Rendering on the fly saves the cheapest thing in the budget
while adding query-time latency and coupling the index to
`renderer_version`. **Do D1–D3, re-measure against the real mail corpus,
and only then decide whether `on_demand` earns its keep for mail.** It
may never.

What this buys by being a policy rather than a decision: if the 250k-mail
numbers come back hostile, the escape hatch is a config value on one
step, not a redesign.

---

## 5. Ingestion

### 5.1 Threads, not messages

Index one document per thread, using `conversation_uuid` from
`grid_rows`. datalib already renders one markdown document per
conversation for chat sources; mail is the same shape.

- **Volume.** 250k messages → an estimated 25–40k threads.
- **Retrieval quality.** "Sounds good, let's do Thursday" is worthless as
  a standalone chunk and worthless to a reranker. The thread is the unit
  a person actually wants returned.

Note the existing subtlety: `markdowns` is keyed on the **rendered file**,
not the abstract conversation — a provider that shards one conversation
into per-period files (beeper) produces many `markdown_uuid`s for one
`conversation_uuid`. Mail should render one file per thread and not
shard, or §4.4's span arithmetic acquires a second dimension.

### 5.2 Strip quoted replies

Email is roughly 40–60% quoted text by volume. QMD's dedup is SHA-256
over whole documents, so it does not catch intra-document repetition —
the same quoted block gets embedded once per reply down a thread. A
straight multiplier on embedding time, index size, and vector count (the
dominant storage term per §4.2). It also degrades retrieval: chunks that
are 80% quoted material match queries for the wrong reasons.

Under §4.4's span model, quote stripping has a consequence worth
designing for: the canonical markdown should be the *rendered thread the
human reads* (quotes intact, so the document still makes sense), while
the *indexed* spans skip the quoted regions. Offsets make that natural —
a chunk is a set of byte ranges over the canonical file, and quoted
ranges are simply not in it.

### 5.3 Media

Images embed **once and never rechunk**, so the incremental story is
simpler than text: key on blob hash, no reprocessing on thread edits.

Media arrives already mirrored — Signal and WhatsApp backups bring
messages plus media, Slack brings file attachments, Google Takeout brings
Maps and Photos material. Bytes live in the per-source
`blobs.doltlite_db`. No new download path is needed — and under D3, no
new copy either.

---

## 6. Integration with datalib

### 6.1 A custom step, writing a sidecar DB

New step `mail_index` (later `media_index`), downstream of `grid_index`,
writing to its **own** sidecar SQLite. Contract is in
[`docs/dev/step_protocol.md`](step_protocol.md).

Three reasons for a sidecar:

1. One writer per doltlite file is load-bearing — the working set is per
   *file* and shared across processes, so two writers commit each other's
   in-flight rows.
2. `qmd_index` owns `unified_index/qmd/index.sqlite` and may rebuild it.
3. Declaring our own output path is what earns incrementality and skip
   logic from the scheduler.

A plain SQLite file (not doltlite) is right here: the index is fully
derived, so history has no value and would cost real bytes.

### 6.2 Language boundary

datalib is Bazel, Python, and Rust. Leaving QMD is the opportunity to
**drop the Node dependency entirely**.

The previous draft proposed Python via `llama-cpp-python`. Worth
re-examining: everything in the shipping path is Rust, and
[`AGENTS.md`](../../AGENTS.md) is explicit that "Python is only used for
fixture / test-pipeline tooling and scripts." A Python step is legal
under the step protocol — any executable is — but it would be the first
Python in the shipping path, and it would need its own runtime staging
story alongside the Node one we are trying to delete.

**Open question, not a decision.** Rust (`llama.cpp` via `llama-cpp-2`,
or `candle`) keeps the shipping path single-language and reuses the
existing `sqlx`/doltlite plumbing; Python gets the `finetune/` pipeline
and the ML ecosystem for free. A defensible split: **Rust for the step
that ships, Python for the offline eval harness and any retraining** —
those never need to run on a user's machine. Resolve before M3.

### 6.3 Query surface

Extend the existing grammar rather than inventing a second one.
`GET /applet/unified_index/search` already implements a Gmail-flavored
language: `field:value`, `-field:value`, quoted values, with `source:`,
`source_name:`, `kind:`, `channel:`, `author:`, `account:`, `project:`,
`before:`/`after:`, `convo:`.

Field terms compile to the `grid_rows` prefilter; free text goes to the
retrieval layer. `author:alice after:2025-01 boat trip` is the whole
feature.

---

## 7. To verify before building

Three of the previous draft's four items are now resolved. Kept here with
their answers, because the answers are the useful part.

| # | Question | Answer |
|---|---|---|
| 1 | What `grid_rows` key corresponds to a rendered document? | **`markdown_uuid`**, confirmed. It is the FK into `markdowns`, whose `md_path` column holds the path relative to the data root; `/applet/unified_index/chat/{markdown_uuid}` resolves through it. Note `grid_rows.qmd_path` is a *denormalized duplicate* of `markdowns.md_path` (the schema documents the invariant that they must be byte-equal, and that `markdowns` is preferred). |
| 2 | Can a plain SQLite client read these stores? | **Split answer.** The qmd index is a plain SQLite file and stock `sqlite3` reads it (verified — every measurement in §4.2 came from stock `sqlite3` + `dbstat` on a copy). The `.doltlite_db` stores are **not** SQLite-file-compatible and need a doltlite-linked shell (`bazelisk build //third-party/doltlite:doltlite`). So the prefilter cannot be a plain `ATTACH`; the retrieval step must link doltlite (which every Rust binary in the tree already does) or the filter must be resolved through the existing repo layer. |
| 3 | Which paths are current? | **`unified_index/grid/db.doltlite_db` and `unified_index/qmd/index.sqlite`.** [`core/src/layout.rs`](../../datalib/backend/core/src/layout.rs) is the source of truth and the live data root matches it. The tree diagram at `docs/agent_user.md:26` (`backend_index/db.doltlite_db`) is **stale** — worth a one-line fix. |
| 4 | Actual thread count and post-quote-strip token volume for the mail corpus. | **Still open, and now blocking.** `fastmail/raw` is 976 KB in the measured root — the corpus is not ingested. This gates the stage-2 representation choice (§3.4), the `render_mode` decision (§4.5), and the embedding schedule (§8). **Est. 2h once a mailbox is actually pulled.** |

New item:

| # | Question | Why it matters | Est. |
|---|---|---|---|
| 5 | Does FTS5 `contentless_delete=1` behave under incremental re-index of a changed thread? | D1 depends on being able to delete and re-add a document's chunks. Contentless FTS5 historically could not delete; `contentless_delete` is recent. If it misbehaves, the fallback is a rebuild-per-source, which changes the step's incrementality story. | 1h |

---

## 8. Risks

**Embedding throughput is the schedule gate.** 100k+ chunks through
EmbeddingGemma-300M on local hardware is hours at best. Budget for
several passes. QMD's embed session historically had a hardcoded
30-minute cap, later made configurable, with remaining batches skipped
and resumed on re-run. Our runner needs equivalent resumability from the
start, and the §7 item 8 failure mode — partial vectors on an interrupted
run being treated as complete — is the specific bug to design against.

**D4 couples the grid to an unbuilt component.** Mitigated by staging it
last and by its being optional (§4.4).

**D3 removes a real product property** (a self-contained rendered tree).
Mitigated by an explicit export, but it is a decision to make
deliberately, not to discover.

**Upstream model availability.** Mitigated by mirroring (§2).

**Reranker latency at k.** May force a smaller candidate set than fusion
quality wants. Eval fixture decides.

**Scope creep into media before text is proven.** Explicitly deferred
(§10).

---

## 9. Evaluation

**Build the eval fixture before the index.** Without it, RRF constants
get tuned by vibes.

Shape: ~30 real queries against our own mail with hand-labeled relevant
documents, runnable in under a minute. Report precision@k, recall, MRR,
and F1 across each backend independently (BM25, vector, hybrid, full
pipeline).

It answers, cheaply:

- Does semantic retrieval beat plain BM25 on *this* corpus? FTS5 handles
  250k documents without complaint, and for many mail queries (names,
  subject lines, invoice numbers) lexical plus a good metadata filter may
  be most of the win.
- **What does int8 cost in recall?** (§3.4 — this is now a storage
  decision, not just an accuracy one.)
- Does the reranker earn its latency?
- Does the expansion model help on mail, or was it tuned for notes?
- **What does snippet hydration cost at k=20?** (§3.2 — gates §4.5.)

**Run against a stratified 10k-thread sample before committing GPU time
to the full corpus.**

---

## 10. Sequencing

**M0 — Verify.** §7 item 4 (ingest a real mailbox and measure) and item
5. Item 1–3 are done. No new code.

**M0.5 — Storage wins that need none of this.** D2 (compress at rest)
and D3 (attachments in CAS only). Both are independent of the retrieval
layer, both are measurable immediately against the existing 3.6 GB root,
and D2 is the largest single lever in the document. Doing these first
also de-risks M0 by shrinking the mail corpus before it lands.

**M1 — Eval fixture.** 30 labeled queries, harness, baseline numbers for
FTS5-only over the canonical text. This is the control.

**M2 — Threaded render + quote stripping.** Thread assembly from
`conversation_uuid`, quote stripping, chunking as byte spans (§5.2).
Produces the real §7-item-4 numbers.

**M3 — Text retrieval, 10k sample.** `mail_index` step; contentless FTS5
+ float32 vectors + RRF + reranker, behind `search(filter, query, k)`.
D1 lands here. Compare to M1 baseline. Resolve §6.2 (Rust vs Python)
before starting.

**M4 — Scale to full corpus.** Pick the stage-2 representation from M3's
recall numbers (§3.4). Add stage-1 binary. Re-tune fusion constants.

**M5 — Query surface.** Extend the Gmail-flavored grammar; wire the
prefilter. D4 lands here or is dropped.

**M6 — CLIP.** Not before M5 is measured.

The constraint on M0–M5: **do not foreclose M6.** Concretely — the
`search()` interface stays vector-space agnostic, and the fingerprint
(now including `renderer_version`) stays in the primary key.

---

## Appendix A: QMD failure modes to not rediscover

QMD's changelog is a catalogue of bugs we would otherwise find one at a
time. Several land directly on email. It is MIT — lift code where that is
easier than rewriting. *(From its changelog and docs, not from reading
its source; confirm before depending on a specific constant.)*

- **Dotted tokens.** FTS5's `porter unicode61` tokenizer splits on dots
  and stores the parts as adjacent tokens. QMD shipped fixes twice: bare
  terms like `2026.4.10` (sanitization stripped dots into `2026410`,
  which could never match), then again for dotted tokens inside quoted
  phrases. **For mail this is not an edge case** — message-ids, domains,
  and addresses are all dotted.
- **Inverted BM25 normalization.** The formula was `1/(1+|x|)` instead of
  `|x|/(1+|x|)`, so strong matches scored *lowest*. Silently broke
  min-score filtering and made a strong-signal short-circuit dead code.
- **UTF-16 surrogate pairs split across chunk boundaries.** Produces an
  unpaired surrogate that some embedding backends reject as invalid JSON
  — deterministically, so the chunk fails on every retry.
- **Hyphenated tokens** in FTS5 lex queries (`real-time`) and **stripped
  underscores** in search terms.
- **BM25 field weights** not covering all FTS columns.
- **Rerank cache keys** omitting the model URI, so swapping rerankers
  served the previous model's cached scores.
- **Embedding-context pool sizing** assuming a fixed per-context VRAM
  cost across models.
- **Partial vectors on interrupted embed runs** treated as complete;
  requires verifying full chunk coverage before marking a document
  embedded.
- **Orphaned vector cleanup** that was not transactional, desyncing the
  vector index from chunk metadata and silently making documents
  unsearchable by vector.

And one found here rather than in the changelog:

- **FTS5 declared without `content=`** keeps a full shadow copy of every
  indexed body (§4.2). Costs 2.2× the source text before any postings.

## Appendix B: Sources

**Verified against this tree** (2026-09-01): `schema/src/grid_rows.rs`,
`schema/src/markdowns.rs`, `core/src/layout.rs`,
`etl/src/blob_cas.rs`, `etl/providers/email/src/download/schema_raw.rs`,
`etl/providers/email/src/render/`, `unified_index/src/db.rs`,
`unified_index/src/dolt_repo.rs`, `unified_index/src/qmd/*`,
`applets/src/unified_index/mod.rs`, `third-party/qmd/src/store.ts`
(schema only). Measurements from `~/datalib/thad_imbue_dev` via stock
`sqlite3` + `dbstat` and `/usr/local/bin/doltlite`, both on copies.

**Documentation only, not source-read:** `tobi/qmd` README + CHANGELOG
(through 2.8.3); `asg017/sqlite-vec` release notes, ARCHITECTURE.md,
issues #25/#26/#29/#121.
