# Shipping datalib as a toolchain for agents

> **Status: proposal.** Nothing in §3 is built except where a paragraph
> says it already ships. Written 2026-09-03 against datalib `main` @
> `5ccc7950`, prompted by
> [`imbue-ai/default-workspace-template#534`](https://github.com/imbue-ai/default-workspace-template/pull/534).
>
> **Depends on [`data_handling_practices.md`](data_handling_practices.md).**
> That is not a courtesy link — see §0.

## 0. Why this one comes second

The obvious move on reading
`imbue-ai/default-workspace-template#534` is to say "we have most of
that already, here are our primitives." That would be a mistake in a
specific way: **an agent that adopts a primitive inherits its defects,
and cannot see them.** Our render paths carry 424 unaudited
silent-fallback sites; a record render cannot deserialize fails the
whole render step and poisons its downstream subtree; nothing anywhere
counts a value we dropped. Handing that to someone whose own skill *does* count dropped
values would be trading down, and they would find out later and
downstream of us.

So the sequencing is: audit and retrofit (the companion doc), then
ship. Concretely, a surface below is ready to offer when the code
behind it has a problem sink, a published lossiness table, and a spot
check against source. **The one exception is Surface A**, which is
pure scheduling and carries none of our render behaviour — it can go
first, and probably should.

The rest of this doc assumes that ordering and describes the
destination.

## 1. What already exists that an agent could use

### On-disk ingestion is further along than the docs suggest

The mental model "datalib mirrors personal data from web APIs" is how
the project is described, and it undersells the tree: **five of the
twenty providers never touch the network.**

| provider | subject | identity | render side |
| --- | --- | --- | --- |
| `fsindex` | the tree itself | **path**-keyed, Merkle tree over directories | none |
| `pdf` | documents | **content**-keyed (`blake3(bytes)`), paths hang off it | yes → md + sidecars |
| `media` | music/photos/video | content-keyed, plus a second hash excluding container metadata | none |
| `google_takeout` | an unzipped Takeout root | per-sub-feed upstream ids | yes |
| `perseus` | an immutable TEI corpus | upstream section ids | yes |

Plus the file-import halves of `email` (mbox), `contacts` (a `.vcf`
directory), `signal` / `whatsapp` / `sms_backup_restore` (backup
files), and `lightroom` (a SQLite catalog) — all through the *same*
download → render shape as the API-backed sources, deliberately.

Factored out and shared already:

- **`etl/src/fswalk.rs`** — blake3 with a 16 MiB mmap threshold, a
  gitignore-shaped walker (ripgrep's `ignore` crate), and Unison's
  `(mtime, size, inode, dev)` rescan cursor so an unchanged file skips
  the read. Used by `fsindex`, `pdf`, `media`, and the qmd index-state
  checker.
- **`etl/src/file_checkpoint.rs`** — a shared `ingested_files`
  `(scope, path, size_bytes, mtime_ns)` resume cursor.
- **`SourceCommon.input_path`** — "where do I read from," distinct from
  `raw_path`, tilde-expanded once at load.
- **The content-vs-path identity decision, already argued.** `pdf`'s
  `schema_raw.rs` makes the case at length: the question is "what
  documents do I have," not "what files are on this disk," so the
  entity is keyed on `blake3(bytes)` and a second table records where
  copies live. `fsindex` keys on path because *the tree* is its
  subject. Both available; the choice is explicit.
- **The downstream is already provider-agnostic** —
  `datalib_index_lib::emit_sidecar` is the render→index contract,
  `etl/src/title.rs` and `etl/src/section.rs` write the cross-provider
  title block and anchors, and `grid_index` needs no per-provider
  change to pick up a new sidecar tree.

### The scheduler already runs anything

`datalib-dag` derives edges from output/input path overlap, skips steps
whose input versions didn't move, retries by failure class, and poisons
only the subtree below a failure. A config of purely custom steps is
valid today (`dag/src/config.rs` — `steps` defaults to empty, no step
type is required).

### The read surfaces already exist

`grid_rows` SQL over every source; the rendered markdown tree; the qmd
index (hybrid BM25 + embeddings); a query language (`field:value`,
`-field:value`, `before:`/`after:`); a cross-document `edges` table;
and an applet host where a pipeline contributes its own cards and the
endpoints behind them.

### The incremental contract is a capability, not a protocol

This is probably the strongest thing datalib has to offer someone
building on it, and until now the doc mentioned it once, in passing, as
a tradeoff.

Every stage of the pipeline materializes into a doltlite file, and every
such file answers one question natively: **"what changed since this
hash?"** So a consumer — one the producer never heard of, a person at
the CLI, an agent — attaches to any stage, remembers a single string,
and gets correct incremental semantics. No registration, no
subscription, no coordination with the producer, no schema for a change
feed.

Two properties do the work, and they are worth separating because only
the first is the obvious one:

- **The diff is proportional to the change, not to the data.** A prolly
  tree is content-addressed with structural sharing, so two commits
  share unchanged subtrees by identity and `dolt_diff` descends only
  where hashes differ. This is what makes the cursor cheap rather than
  merely correct — the alternative, a `WHERE updated_at > ?` scan or a
  Rust-computed hash tree, costs O(data) every time. (The tree learned
  this the expensive way; see
  [dolt_diff supersedes per-bucket fingerprints](../data_architecture_ingestion.md#dolt_diff-supersedes-per-bucket-fingerprints).)
- **The state and the change feed are the same object.** In a
  table-plus-changelog architecture the two can disagree: a consumer
  reads an event whose row isn't there yet, or finds a row no event
  announced. Here `dolt_diff` and `SELECT` derive from one structure, so
  that class of bug is not representable. A consumer holding a hash
  holds a consistent view, full stop.

**The worked example is already in the tree.** A conventional backup of
an Adobe Lightroom catalog writes a complete new copy of a multi-gigabyte
SQLite file every time, however little changed. The `lightroom` provider
mirrors that same `.lrcat` into doltlite table by table and lets the
prolly trees dedupe: run it again after a day of editing and only the
rows that actually moved cost anything, while every prior state stays
queryable through `dolt_log` / `dolt_diff_<table>`. Its module doc makes
the argument in full, and the engine is deliberately not
Lightroom-specific — any application whose format is a SQLite database
is a config stanza, not new code.

**Honest placement.** The mechanism is not new, and the pitch is
stronger for saying so. Apache Hudi shipped incremental queries in 2017;
Iceberg snapshots and Delta Lake's change data feed are the same
contract — the consumer holds a snapshot id and asks for everything
since. Datomic had as-of reads plus a transaction log in 2012. The
prolly tree itself comes from Noms, which is where Dolt's storage layer
originated.

What is unusual is the **operating point**, on two axes:

1. **Footprint.** Every system above assumes a cluster, a catalog,
   object storage and a query engine. This is a single file, statically
   linked into the binary — no server, no catalog, no daemon — running
   on a laptop over one person's data. Warehouse-grade incrementality at
   SQLite's operational cost is genuinely uncommon, and it is the
   difference between "a platform team can offer this" and "a binary can
   hand it to you."
2. **Thoroughness.** Those systems typically use the versioned format
   for warehouse tables and leave derived artifacts as plain files.
   Here the intent is to use it for *every* intermediate — the raw
   store, the render output, the problem log, and the DAG's own
   content-versioning — so consumer cursors, step versions and audit
   history are one mechanism rather than four. See
   [`data_architecture_parse_and_render.md` §2](../data_architecture_parse_and_render.md#where-this-is-heading-the-artifact-becomes-a-database)
   for the half of that which is still aspiration.

And the benefit that gets undersold: this is normally pitched on
**performance**, but here the **reliability** argument is at least as
strong. A malformed file, a half-written artifact, an orphan left behind
when identity moves — none of those are failure modes a table has. Whole
categories of bug stop existing rather than getting fixed.

#### What it depends on, and what it costs

**It degrades quietly if writes are not surgical.** A stage that
rewrites every row produces a diff the size of the table and the cursor
buys nothing. That makes
[`AGENTS.md`'s "give a bag an order before storing it"](/AGENTS.md)
load-bearing for the architecture rather than a tidiness rule: an
unchanged record that serializes differently from itself manufactures a
diff out of nothing. Any new rule of that kind — canonical field order,
stable float formatting, excluded volatile fields — is protecting this
property, and should say so.

**Nothing reclaims space.** doltlite never deletes, so every
intermediate keeps its full history. That is exactly right for the raw
store, which is the irreplaceable copy, and much harder to justify for
*derived* intermediates, where it means unbounded growth on the
artifacts we care least about preserving. Iceberg has snapshot expiry
and Delta has `VACUUM`; we have nothing.

**This is a deliberate deferral, not an oversight** (decided
2026-09-04). The mitigation is cheap precisely because the data is
derived — delete the intermediate and rebuild it, costing a re-render
rather than a re-fetch — and while the schemas are still changing often
enough that intermediates get deleted and rebuilt anyway, a built
reclaim mechanism would be solving a problem the churn already solves.
The condition to watch is the schema settling down: once intermediates
start living a long time, the calculus flips and this needs building.

### And it already reaches agents

Every tag publishes per-triple tarballs including fully static
`*-unknown-linux-musl` binaries under stable filenames, and
`qi-imbue/datalib-inspiration` already installs them into a Minds
workspace from a pinned tag. "Can an agent use a Rust binary in a
sandbox?" is answered, and the answer is yes.

## 2. What's missing

**a. No caller-configured projection.** This is the one gap. Every one
of the crates in §1 has the scan, the identity split, the cursor and
the render path — and a **hardcoded** projection inside that render
path. `pdf` knows PDFs, `perseus` knows TEI, `google_takeout` knows
Takeout's dozen sub-shapes.
Nothing says: *here is a tree of JSON / JSONL / CSV / Markdown, here is
which field is the id, the title, the body, the timestamp, the author,
the labels, the outbound links.* Adding one today means a Rust crate
and an edit to a compile-time match in
[`datalib_step/src/dispatch.rs`](/datalib/backend/datalib_step/src/dispatch.rs)
— days, not ten minutes.

**b. Findability, which is a bigger problem than capability.** An agent
handed a directory of exports and a ten-minute budget will not
discover any of §1, because every document describing it describes
personal-data mirroring. A primitive nobody finds is indistinguishable
from one that doesn't exist.

**c. `grid_rows` is missing a `labels` column.** Map a document-shaped
corpus onto the union table and the core lands cleanly — id → `uuid` /
`upstream_id`, author → `author`, created → `when_ts`, space → `project`,
team → `channel`, body → `text`, url → `source_url`, title →
`conversation_name` and `markdowns.title`, cross-refs → the `edges`
table. Exactly one field has nowhere to go: a set of tags. Every
provider with labels upstream (GitHub, GitLab, Notion multi-selects)
has them sitting in `payload` JSONB and projects none of them, because
there is nowhere to project to. The chat-flavoured *naming* of
`conversation_uuid` / `entire_chat` is a field-docs problem, not a
schema one.

**d. The queryable store isn't queryable from a released install.**
`agent_user.md` tells an agent to run `doltlite -readonly …`. That CLI
ships in the docker image and is **not** in `//datalib/backend:dist`,
so it is not in the tarball `install.sh` unpacks.

**e. No Python binding, and we should not build one.** The consuming
skill's packaging rule is stdlib-only, no per-skill dependencies. A
pyo3 wheel is a dependency; a subprocess is not.

## 3. The three surfaces

### Surface A — `datalib-dag` as a harness for any pipeline

**Ships today; needs packaging, not capability. Not gated on the
audit** — it schedules processes and knows nothing about how any of
them read data.

```toml
[[steps]]
id = "tickets.ingest"
command = "python3 .agents/skills/roadmap/scripts/run.py load"
inputs  = ["uploads/tickets"]
outputs = ["tickets/store"]
```

That one table buys skip-when-unchanged (if the step prints a content
version), retry by failure class, live progress, graceful cancellation,
and multi-step composition. Four things to package it:

1. A page — "datalib-dag as a pipeline harness" — opening with a config
   containing no datalib providers at all.
2. A ~40-line stdlib `dag_step.py` a skill vendors rather than reading
   a spec for: parse `--params`/`--inputs`, read
   `DATALIB_DAG_CHANGED_INPUTS`, emit progress/outcome, hash a tree for
   the fallback version.
3. A one-screen primitives index — `fswalk`, `file_checkpoint`,
   `blob_cas`, `emit_sidecar`, `edges`, the query language — organized
   around "I have data and a problem," not "run a sync."
4. Ship the `doltlite` CLI in `:dist` (§2d).

### Surface B — a `docs` provider: configured projection over a file tree

**Gated on the audit**, because this is the surface that would hand our
render behaviour to someone else. It is `pdf` with the extractor
replaced by a declarative projection; everything else in §1 is reused.

The split across the two steps follows our existing layering rather
than the skill's single `parse`: **download** walks the tree and stores
each record's bytes verbatim, so it owns `glob` / `record` / `identity`
/ `version`; **render** deserializes and projects, so it owns `map`.
That is not bookkeeping — it is what makes a projection bug a
re-render instead of a re-walk, and it is the reason the config below
has params on both steps.

```toml
[[steps]]
id = "tickets.download"
command = "datalib-step download docs"
outputs = ["tickets/raw"]
[steps.params.common]
input_path = "~/exports/tickets"
[steps.params]
glob     = "**/*.json"
record   = "."             # one record per file; "$.issues[*]" to fan out
identity = "$.id"          # falls back to blake3(bytes) when absent
version  = ["$.updatedAt", "@mtime"]

[[steps]]
id = "tickets.render"
command = "datalib-step render docs"
inputs  = ["tickets/raw"]
outputs = ["tickets/rendered_md"]
[steps.params.map]         # → the unified grid_rows core
title   = "$.title"
text    = ["$.description", "$.body"]   # first present wins
when_ts = "$.updatedAt"
author  = "$.assignee.name"
project = "$.team.key"
labels  = "$.labels"
edges   = "$.relations[*].targetId"
```

Note what is *not* there: any field name datalib knows about. The
fallback lists are the caller's. Shipping a built-in vocabulary of
source field names is how this becomes a parser for whichever corpus we
happened to test against.

What the caller gets without writing it: the order-independent
`max((version, batch))` merge with the ledger in the same transaction;
the R1 problem sink; parallel
read with a single writer; a content version so downstream steps skip;
and `status --json` from the ledger. Which is to say — **the deliverable
here is the practices doc's output, packaged.** If those aren't real
yet, there is nothing to sell.

Two design calls: **type coercion is the whole game** (real exports
declare an int and emit `"312"`, mix date-only with offset-bearing and
`Z` timestamps across sibling sources), and **doltlite or plain
SQLite** — doltlite buys `dolt_diff`, costs the never-deletes property
that collides with retention. Recommendation: `store =
"doltlite" | "sqlite"`, default doltlite, document the tension rather
than pretending a prune reclaims disk.

### Surface C — the pipeline's own view

`datalib-http` already hosts applets: a config-declared server
contributing frontend card components and the endpoints behind them,
spawned on demand. A pipeline could ship its own card over its own
table without owning a web stack. Nothing needs building; what's
missing is a worked minimal example.

### Why a binary and a protocol, not a crate

A Rust crate serves consumers who build Rust; this consumer builds
stdlib Python inside a ten-minute budget. A wheel is a dependency their
packaging rules forbid, and it couples our release cadence to theirs at
the ABI level. A static musl binary over `subprocess` with JSON in and
NDJSON out is stdlib-compatible, already built by our release workflow,
already installed into Minds workspaces, and versioned by a tag the
consumer pins. Publish crates later if someone asks.

## 4. Checking the result against someone else's corpus

Once Surface B exists, the cheapest way to find out whether it
generalizes is to point it at a corpus we did not design.
[EnterpriseRAG-Bench](https://github.com/onyx-dot-app/EnterpriseRAG-Bench)
([paper](https://arxiv.org/abs/2605.05253)) is a fair instance of the
shape class: ~500k JSON documents on disk across nine enterprise source
types, each with an id, a title, a body, a time, an author, tags and
cross-links. The answer contract is one JSONL line per question —
`{"question_id", "answer", "document_ids"}` — where the document ids
are a field in each document, so a system only has to get from a
question to a set of ids. Its published finding is that **vector search
underperformed BM25** on enterprise jargon, which is the configuration
our hybrid qmd index already is.

It is also a genuine load test: `grid_index` and `qmd_index` have never
been run at that size and we have no other corpus that big.

**It is a check, not a target.** Each of these is a defect, not a
shortcut:

- a config key that only makes sense for that corpus;
- a built-in vocabulary of field names datalib goes looking for (the
  *mechanism* to follow a field-name indirection is general and fine; a
  hardcoded list of names is not);
- tuning retrieval weights or chunk sizes to its question set;
- a schema column added because that corpus has one — `labels` (§2c)
  earns its place from Gmail, GitHub and Notion, and would if the
  benchmark vanished;
- reporting a score without saying the corpus is synthetic, the scoring
  needs an LLM, and we chose the retrieval strategy.

The test for all of them: **would this change still be right if the
benchmark didn't exist?** If the answer needs the benchmark, it's
overfitting.

## 5. What the skill's authors could take from us

The exchange runs both ways. Five things this codebase learned by
getting them wrong first, each mapping to a section their SKILL.md
already has.

1. **Sort unordered collections before storing them.** When a source
   returns a *set* — permissions, tags, labels, member ids — the order
   is whatever the server emitted. Left unsorted, a re-export of an
   unchanged object serializes differently from itself and everything
   downstream believes it changed. Found here 2026-08-31: claude.ai
   returns a project's eight `permissions` strings in different orders
   on different fetches. **Sort it; don't declare it volatile** —
   dropping the field loses a real signal, sorting removes only noise.
   This is the exact failure mode of their target case, and it defeats
   their §8 "identical for every batch ordering" assertion in a way
   that looks like a merge bug.
2. **Preserve the source's UTC offset; never fabricate a timestamp.**
   You can recover UTC from an offset but not the offset from UTC, and
   the offset is how the timestamp read to the human who saw it. No
   timestamp → store null, not epoch, not now, not midnight. Their §1
   asks the agent to *find* absurd timestamps but not what to store.
3. **The projection-vs-payload trade is conditional, and their §3
   states one side.** Storing the projection is right when the raw
   batches stay on disk — they say so. An agent carrying that rule to
   an API-backed pull, where the batch is gone once the window closes,
   has thrown away data it cannot get back. One clause closes it.
4. **Split per-attempt bookkeeping off the record row.** Anything that
   moves per attempt (`fetched_at`, `attempt_count`, `last_error`)
   belongs on a sidecar, or every diff is noisy and unchanged content
   looks changed. Their §7b puts problem *counts* in the ledger row,
   which is right; the general rule is worth stating.
5. **Absent is not malformed** — and, per our own R2, malformed-isolated
   is not malformed-systemic either. Three categories, not two. We got
   this wrong in the other direction and are fixing it.

## 6. Open questions

1. **Harness, store, or both?** If a skill-built pipeline is happy with
   its own SQLite and wants only scheduling, incrementality, retry and
   a UI, Surface A is the whole answer and B is speculative. This is a
   day versus two weeks.
2. **Does the ten-minute budget survive reading a second contract?**
   The step protocol is one page, but it *is* a page. If not, we ship
   `dag_step.py` and they vendor it.
3. **How much of the audit gates Surface B?** All of it, or just the
   provider the `docs` code is derived from (`pdf`)? My instinct is the
   latter — the shared machinery plus one clean provider — but it is a
   judgment call worth making explicitly rather than by default.

## See also

- [`data_handling_practices.md`](data_handling_practices.md) — the
  companion this depends on.
- [`step_protocol.md`](../step_protocol.md) — Surface A is a packaging
  exercise on top of it.
- [`multimodal_retrieval.md`](../multimodal_retrieval.md) — the
  retrieval design §4's corpus would give a number to.
- [`applets.md`](../applets.md) — the mechanism behind Surface C.
- [`/datalib/backend/etl/providers/pdf/src/download/schema_raw.rs`](/datalib/backend/etl/providers/pdf/src/download/schema_raw.rs)
  — the content-vs-path identity argument, and the template Surface B
  should be built from.
