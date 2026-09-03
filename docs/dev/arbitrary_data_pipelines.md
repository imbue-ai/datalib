# Making Datalib fit into Arbitrary Data Processing Pipelines

> **Status: proposal.** Nothing in §4 is built except where a paragraph
> says it already ships. Written 2026-09-03 against
> `imbue-ai/default-workspace-template#534` ("Add data-pipeline-builder
> skill and wire it into the hardening flow") and datalib `main` @
> `5ccc7950`. Audience: the authors of that PR, plus whoever picks this
> up here.

## The short version

PR #534 adds a skill that walks an agent through building an ingestion
tool: batches of raw records land, the tool merges them into a
queryable store, incrementally, idempotently, with backfill, a problem
log, and bounded retention. It does this in about ten minutes and
~600 lines of stdlib Python, and the eval says the agents that follow
it beat the agents that don't on exactly the axes you'd expect —
merging, resilience to bad records, not reprocessing on re-run.

Read that SKILL.md next to `docs/dev/data_architecture_ingestion.md`
and the **storage core** converges, closely and independently:
upstream identity as the primary key with no surrogates, `ON CONFLICT
(id) DO UPDATE` with every column from `excluded` as the only write
shape, chunked multi-row writes inside one transaction, and a raw
layer preserved verbatim with everything downstream derived and
rebakeable from it. Datalib has been building that engine in Rust
since May 2026 across twenty provider crates.

**The data-quality surface does not converge, and the skill is ahead
of us on most of it.** An earlier draft of this document called the
whole comparison "uncanny" and scored several rows in our favour that
do not survive checking. The corrected reading is below; the summary
is that we have thought hard about *storing* records correctly and
much less about *what to do with a record we cannot store*.

Three things follow, and they are the three claims of this document.

1. **The opportunity is not "replace the skill's Python with datalib."**
   The skill's economics are the whole point of the skill — ten
   minutes, stdlib only, no dependency resolution — and a Rust library
   loses that fight by default. What datalib can hand over is the part
   that is expensive to get right and cheap to get wrong, delivered as
   a **binary with a JSON protocol**, which is the one vehicle a
   stdlib-only Python skill can consume.
2. **The bytes-on-disk half is much further along than it looks** (§2).
   Five providers already scan a local tree, and between them they have
   the walk, the content-vs-path identity split, the rescan cursor, the
   ignore cascade, and the render-to-markdown-plus-sidecar path.
   Exactly one thing is missing, and it is the same thing every time:
   **no provider's parse step is caller-configured.**
3. **There is a public corpus to check the result against, and it has
   to stay a check** (§5). EnterpriseRAG-Bench — the benchmark behind
   PR #534 — is ~500k JSON documents on disk across nine enterprise
   source types. It is a fair *instance* of the shape class §2 and §4B
   are about: a tree of self-describing records, each with an id, a
   title, a body, a time, an author, some tags, and some cross-links.
   It is worth running because `docs/dev/multimodal_retrieval.md`
   (drafted 2026-09-01, two days before this) proposes a hybrid
   retrieval layer with an arbitrary metadata prefilter and has no
   external number attached to it. It is **not** the target.

**The design goal is that an agent approaching any corpus of this
shape finds datalib useful and usable. The bench is how we find out
whether we got that right — nothing more.** So the rule for everything
below is: **build for the shape class, not for the corpus.** Every
knob proposed here has to be justifiable from sources that already
exist in this tree or in the world; a knob whose only motivation is a
benchmark field name is a bug in the proposal, not a feature of the
product. §5 lists what would count as overfitting, so the rule stays
falsifiable rather than decorative.

And the exchange runs both ways, which is the point of the last two
sections: **§6 is what we should adopt from them, in the order I'd do
it. §7 is what they could take from us.** §6 matters more — it is the
half with a bug in it.

---

## 1. The skill read as a spec, checked against the tree

| `data-pipeline-builder` says | datalib's answer | where |
| --- | --- | --- |
| §1 Profile the data before writing the tool | **nothing.** No profiler, no field-role inference. Closest thing is `download_metrics::snapshot_db_file`, which `COUNT(*)`s every table before/after a run | `etl/src/download_metrics.rs` |
| §2 Test-first pure `parse(record) -> row`, table-driven, ≤8 oddity rows | Same shape: per-provider `render/schema_translate.rs` holds the pure projection, insta goldens over TNG fixtures assert it, `.update` targets regenerate | `providers/*/src/render/`, `tools/insta.bzl` |
| §3 Storage by access pattern → embedded SQLite | Same answer, one level further: **doltlite** — SQLite's API and JSONB, plus git-shaped history, so "what changed since commit X" is a native query rather than something you compute | `docs/dev/doltlite.md` |
| §4 identity = verified unique key; version = `(record version, batch)`; merge = keep the max; ledger in the same transaction | **Identity: same rule.** PK is always the upstream id, never a surrogate; every write is a complete `ON CONFLICT(id) DO UPDATE SET <every col> = excluded.<col>`; one commit per source per run. **Version and ledger: no equivalent.** Datalib has no notion of an input *batch* at all, so there is no batch identity, no `(version, batch)` ordering, no "skip a batch the ledger already has," and no `--force` to re-read one. The nearest thing is `file_checkpoint`, a per-*file* `(scope, path, size, mtime)` cursor upserted in the same tx as that file's last flush | `etl/src/bulk.rs`, `etl/src/file_checkpoint.rs` |
| §5 Parallel readers, single writer, one tx per batch, WAL | Same: single writer per doltlite file is load-bearing (two writers on one file commit each other's in-flight rows), chunked multi-row INSERT at `SQL_CHUNK = 400`, one entity tx + one CAS tx per batch | `etl/src/bulk.rs`, `AGENTS.md` §"Inspecting doltlite stores" |
| §7 `load` / `status --json` / resumable / documented columns | `datalib-dag <config.toml>`, NDJSON on stderr (`run_plan`, `step_start`, `progress_*`, `step_finish`, `run_summary`), per-step versions in `system/dag_state.json`, `GET /api/dag`, Ctrl-C checkpoints and the next run resumes | `docs/agent_user.md`, `dag/src/scheduler.rs` |
| §7b Drop, count, log; never abort, never hide; `<store>.ingest.jsonl` | **Only for fetch failures, and the opposite policy for parse failures.** A per-item network/API failure is tolerated: `warn!`, a counter, `last_error` stamped on `<t>_bookkeeping`. A record that will not *parse* fails the step — `grid_index`'s per-sidecar loop propagates every error with `?` (an unreadable sidecar, or an id claimed by two sources, ends the whole load), and the step protocol classifies an unparseable row as a `data` failure, which poisons the downstream subtree. There is no problem sink, no per-reason counts, no field-nulling policy | `data_architecture_ingestion.md` §"Error handling", `etl/src/grid_index.rs` |
| §7c Retention: bound the store, the ledger, and the log inside `load` | **Nothing, and structurally awkward** — see §3c. The skill is ahead of us here | — |
| §8 Verify: full load in background; export identical for every batch ordering; 20-record spot check sharing no code with `parse` | **Partly, and the weaker parts.** `--reset-and-redownload` checks *completeness* (refetch from scratch, let dolt's diff report what incremental dropped) and the live-golden asserts a second run is a no-op. Neither order-independence nor the independent spot check has an equivalent — see below | `data_architecture_ingestion.md` §"Verifiable via `--reset-and-redownload`" |
| "Environment hygiene": no orphaned workers, kill only your own | `dag/src/lock.rs` guards the data root; SIGINT is a graceful checkpoint path with a 130 convention | `docs/dev/step_protocol.md` §Signals |

The §4 merge difference is the load-bearing one, and it is not a
wash. The skill merges by **`max((version, batch))`**, so an older
batch carrying an older version cannot regress a newer one and any
load order produces the same store — order-independence by
construction. Datalib merges by **last complete write wins**, which is
sound *only* because the raw store is fed by one writer walking an
upstream whose newest state is by definition the truth, and because
the version question is pushed into doltlite's commit history rather
than into a column. Take that assumption away — overlapping re-exports
arriving from disk in arbitrary order, which is exactly the case
datalib has never had to handle — and last-write-wins is simply
wrong. So this is not "two rules for two input models": the skill's
rule is strictly more general, and §4B should adopt it rather than
port ours.

### Where the skill is ahead of us

Seven things, checked against the tree rather than against our own
prose. The pattern is consistent: they are all about **the record we
cannot store**, which is the question we have thought least about.

1. **Profiling before building** (§1). We have nothing. The nearest is
   `download_metrics::snapshot_db_file`, which counts rows per table
   before and after a run — useful for "did anything land," useless for
   "is this field an identity."
2. **One problem sink with a reason taxonomy** (§7b). Unreadable file,
   non-object document and no-usable-identity are *dropped*; a field
   failing coercion is *nulled*; a value whose type the contract does
   not cover is nulled rather than passed through. Each is one line in
   one log with a reason and a sample. We have a single `errors=N` for
   fetch failures and nothing at all for projection.
3. **A field-nulling policy, stated as policy.** "Any rule that turns a
   non-null source value into null is a judgment call." We have no such
   concept, which means we also cannot count them.
4. **The judgment-call table** (§8) — every lossy rule listed in the
   README with the number of records it affected, drawn from the
   ingestion log. This is the best idea in the skill and we have no
   analogue anywhere. It makes lossiness a reviewable artifact instead
   of a property you would have to go read the parser to discover.
5. **The systematic-breakage exit** — stop non-zero when a batch looks
   broken as a whole (say >20% of its records dropped), so a schema
   change is not swallowed one warning at a time. Compare
   `data_architecture_ingestion_practices.md`
   §"Detecting upstream shape drift", which says in as many words:
   "not implemented today, and we don't know yet what we want. A
   previous attempt (`endpoint_shapes`) was deleted." The skill's rule
   is crude, and it is a real answer to the question we left open.
6. **A spot check that shares no code with the parser** (§8): for 20
   random records, the exported row must equal the raw record for every
   pass-through and simple scalar field. Our provider goldens assert
   that output matches *what it matched last time*, which enshrines
   whatever the parser did — including a bug — and `AGENTS.md` warns in
   its own voice that a test which cannot fail is self-concealing. The
   skill's check compares against the source instead. That is a
   strictly stronger property and we do not have it.
7. **Order-independence as an asserted property** (§8: the export is
   identical for every batch ordering). This is downstream of the merge
   rule: `max((version, batch))` is order-independent by construction,
   last-complete-write-wins is not. `--reset-and-redownload` checks
   completeness, not order.

**§7c retention** is an eighth, already covered in §3c.

None of these are hard to add in isolation; the reason we don't have
them is that datalib grew from the download side, where the
interesting failures are network-shaped, and the skill grew from the
parse side, where they are data-shaped. That is also why the exchange
is worth having in both directions: §6 is what we should adopt,
§7 is what they could take back.

### What datalib has that the skill doesn't ask for

- **A versioned store.** doltlite keeps history space-efficiently and
  can enumerate the delta between any two versions. That turns
  "what changed since I last processed this?" from bookkeeping you
  maintain into `dolt_diff_<table>` you query. Datalib's render step is
  driven entirely by it (`etl/src/render_cursor.rs`).
- **A scheduler over arbitrary commands.** `datalib-dag` derives edges
  from output/input path overlap, skips steps whose input versions
  didn't move, retries by failure class, and poisons only the subtree
  below a failure.
- **A content-addressed blob store** with per-provider edge tables, so
  attachments dedupe by blake3 (`etl/src/blob_cas.rs`).
- **A cross-document `edges` table** — `(src_markdown_uuid,
  src_anchor_uuid, dst_markdown_uuid, dst_anchor_uuid, label)`. Most
  hand-rolled pipelines never build one, and it is what multi-hop
  questions run on (`docs/dev/edges.md`).
- **A UI.** `datalib-http` serves a grid, a query language
  (`field:value`, `-field:value`, `before:`/`after:`), a preview pane,
  and an applet host where a pipeline contributes its own cards and
  endpoints. That is the "show" half of `fetch-process-show`, built.
- **A distribution path that already reaches Minds.** Every tag
  publishes per-triple tarballs including fully static
  `*-unknown-linux-musl` binaries under stable filenames, and
  `qi-imbue/datalib-inspiration` already installs them into a Minds
  workspace from a pinned tag.

---

## 2. Bytes on disk: what is already built

This is the section that changed most on a second look. The mental
model "datalib mirrors personal data from web APIs" is how the project
is *described*, and it undersells the tree: **five of the twenty
providers never touch the network.** They read a local directory.

| provider | subject | identity | render side |
| --- | --- | --- | --- |
| `fsindex` | the tree itself | **path**-keyed, with a Merkle tree over directories | none |
| `pdf` | documents | **content**-keyed (`blake3(bytes)`), paths hang off it | yes → md + sidecars |
| `media` | music/photos/video | content-keyed, plus `payload_blake3` (a second hash that excludes container metadata) | none |
| `google_takeout` | an unzipped Takeout root | per-sub-feed upstream ids | yes |
| `perseus` | an immutable TEI corpus | upstream section ids | yes |

Plus the file-import halves of `email` (mbox), `contacts` (a `.vcf`
directory), `signal` / `whatsapp` / `sms_backup_restore` (backup
files), and `lightroom` (a SQLite catalog). All of them go through the
**same** `download` → `render` shape as the API-backed sources, on
purpose: "render has exactly one input contract per provider
regardless of whether the data came over the wire or off disk"
(`data_architecture_ingestion.md` §"Wire-fidelity of the raw store").

The reusable machinery underneath, already factored out:

- **`etl/src/fswalk.rs`** — blake3 hashing with an mmap threshold at
  16 MiB, a gitignore-shaped walker (the `ignore` crate, i.e.
  ripgrep's), and Unison's `(mtime, size, inode, dev)` rescan cursor so
  an unchanged file skips the read entirely. Shared by `fsindex`,
  `pdf`, and `media` — and by the qmd index-state checker.
- **`etl/src/file_checkpoint.rs`** — a shared `ingested_files`
  `(scope, path, size_bytes, mtime_ns)` resume cursor, lifted out of
  the mbox importer for anyone to use.
- **`SourceCommon.input_path`** — the config envelope's "where do I
  read from," distinct from `raw_path` ("where do I keep my store"),
  tilde-expanded once at load. Every file-backed provider takes it.
- **A per-provider `ignore` cascade and a `max_bytes` ceiling**, so one
  multi-gigabyte file can't stall a scan.
- **The content-vs-path identity decision, already made and written
  down.** `pdf`'s `schema_raw.rs` argues it at length: the question is
  "what documents do I have," not "what files are on this disk," so the
  entity is keyed on `blake3(bytes)` and a second table records where
  copies live. A `mv` doesn't touch the document row. `fsindex` keys on
  path because *the tree* is its subject. Both are right; the point is
  that the choice is explicit and either is available.
- **The rest of the pipeline is provider-agnostic already.**
  `datalib_index_lib::emit_sidecar` is the render→index wire contract;
  `etl/src/title.rs` writes the cross-provider title block;
  `etl/src/section.rs` writes the `data-section-uuid` anchors the UI
  navigates by; `grid_index` needs no per-provider change to pick up a
  new sidecar tree.

**So what is actually missing?** One thing, and it is the same thing in
every one of those crates: **the parse step is hardcoded.** `pdf` knows
how to extract text from a PDF. `perseus` knows TEI. `google_takeout`
knows Takeout's dozen sub-shapes. There is no provider that says: *here
is a tree of JSON / JSONL / CSV / Markdown, here is which field is the
id, the title, the body, the timestamp, the author, the labels, and the
outbound links.* That is the gap — and it is a much smaller one than
"no generic ingest" made it sound in an earlier draft of this document.

---

## 3. Where datalib still does not fit

**a. No caller-configured parse, and adding one today means Rust.**
Each of the twenty provider crates is hand-written and wired into a
compile-time match in
[`datalib_step/src/dispatch.rs`](/datalib/backend/datalib_step/src/dispatch.rs).
The recipe for a new source is six steps, two of which are "add the
crate to the workspace `members` list and to `MODULE.bazel`" and "wire
it into the dispatch match" — days, not ten minutes. `AGENTS.md` is
also explicit that `cargo` and `pnpm` are not supported inner loops, so
consumers get **binaries**, not a build.

**b. `grid_rows` needs two columns, not a replacement.** An earlier
draft of this document said the union table was the wrong shape for
non-conversation data. That was too strong, and the correction changes
the plan. The table is designed to be extensible: a unified core where
a unified answer exists, added columns where it doesn't. Map a
Confluence page from the benchmark corpus onto it and the core lands
cleanly —

| corpus field | `grid_rows` |
| --- | --- |
| `dataset_doc_uuid` | `upstream_id` (and the seed for `uuid`) |
| `author` | `author` |
| `created_at` / `last_updated` | `when_ts` |
| `space` | `project` |
| `owner_team` | `channel` |
| `content` | `text` |
| `original_location` | `source_url` |
| `title` | `conversation_name` (which for a top-level row already "duplicates the row's own title"), plus `markdowns.title` |
| `related_pages` | the `edges` table |

— which leaves exactly one field with nowhere to go: **`labels`**.
That gap is not news from the benchmark, and it is bigger than one
column: it is two features that share a word, and they are worth
separating carefully — see "Two kinds of label" at the end of this
section.

The naming question is the other half of the schema critique, and it
is a docs problem rather than a schema one: `conversation_uuid` /
`conversation_name` / `entire_chat` carry general semantics under
chat-flavored names. The field docs should lead with the general
meaning and give the chat mapping second.

**c. Retention is unbuilt, and doltlite makes it harder than it looks.**
Retention is one of the several places §1 finds the skill ahead of us,
and the one with an architectural obstacle behind it rather than just
absence. There is no pruning anywhere in the ingestion path. And the
property that makes doltlite valuable works against retention: per
`data_architecture_ingestion.md`, "SQL operations (even DROP TABLE) do
not actually delete anything." `system/usage.doltlite_db` is documented
as a timeseries "which nothing prunes."
`docs/dev/multimodal_retrieval.md` §4 measured a real data root and
found the same text stored **five** times and attachment bytes twice,
with nothing compressed at rest. There is no CAS garbage collector
either — a `blob_cas::gc_orphans()` sweep was removed, uncalled, in
`7f588ba1`, and three documents went on recommending it for months
(fixed on this branch).

**d. The queryable store isn't queryable from a released install.**
`docs/agent_user.md` tells an agent to run
`doltlite -readonly <root>/unified_index/grid/db.doltlite_db "SELECT …"`.
The `doltlite` CLI ships in the devcontainer/docker image, but it is
**not** in `//datalib/backend:dist` and therefore not in the tarball
`scripts/install.sh` unpacks. An agent that installed datalib the
documented way has the store and no shell for it. If we are inviting
other pipelines to write into a doltlite store, shipping the CLI is
table stakes. (The doc now says so; the tarball still doesn't have it.)

**e. No Python binding, and that's fine.** There is no pyo3 wheel and
we should not build one first. The skill's packaging rule is "stdlib
only, no per-skill dependencies" — a wheel is a dependency, a
subprocess is not. `subprocess` + `json` is stdlib.

> Docs fixed on this branch while auditing, each verified against the
> code first: `agent_user.md`'s layout diagram named three paths that
> don't exist and undercounted the shipped binaries by two;
> `etl/src/layout.rs`'s module doc named the pre-`unified_index`
> aggregate paths; the `--reset-and-redownload` / `--refetch-blobs`
> help text in `control.rs` and `datalib_step/main.rs` described the
> retired `blob_refs` table rather than the per-provider CAS edge table
> those flags actually touch; and `DOLTLITE_RAW_PORT_GUIDE.md` §7
> documented the retired `RefStub` / `pre_seed_blob_stub` / `gc_orphans`
> write path as the pattern to copy for a new provider.

### Two kinds of label, one query field

Picking up §3b's one missing column, because it isn't one column.

**Source labels are already in the store, and nothing surfaces them.**
Wire-fidelity guarantees that a GitHub PR's `labels` array and a Notion
page's multi-select properties are sitting in `payload` JSONB right
now. I checked the render side of `github`, `gitlab`, `notion` and
`slack`: none of them projects a label anywhere. The reason is
circular — there is nowhere to project one *to*. `email` is the single
exception, and only incidentally: Gmail/JMAP model labels as
mailboxes, so the join table had to exist regardless, which is also
why `email` is the only provider with `only_extract_labels` /
`only_render_labels` knobs. (An earlier draft of this document also
claimed `media` has tags. It doesn't, in the sense that matters — its
"tags" are ID3/EXIF metadata like artist and album, not a label set.
Checked and corrected.)

**User labels are a different feature.** A label a *person* applies —
"funny", "sabbatical" — spanning photos and emails and expenses at once
is something only a unified store can offer, and no per-provider column
produces it. It is long-wanted and **deliberately deferred**: the
design has real subtleties (merge semantics, hierarchy, rename,
deletion, what a label means on a row whose document was re-rendered)
that this document is not the place to settle. Two constraints are
worth recording here so whoever does settle them starts from them
rather than rediscovering them:

- **It cannot live in `grid_rows`.** `grid_index` applies a document by
  `DELETE FROM grid_rows WHERE markdown_uuid = ?` followed by a
  re-insert (`etl/src/grid_index.rs:941`). A hand-applied label written
  there is destroyed by the next render of its document — silently, and
  only for the documents that happened to change. The existing
  precedent for precious, un-derivable, server-written state is
  `feedback`, which has its own `system/feedback.doltlite_db` and one
  writer for exactly this reason.
- **Whatever store it gets, its durability rests on `uuid` being
  deterministic from upstream** — the Ship-of-Theseus rule in
  [`docs/dev/entity_ids.md`](entity_ids.md). A content-hash identity
  would orphan every label on the first edit. Worth noting because it
  means the identity discipline already in place is a precondition, not
  an unrelated concern.

The source-label column (`grid_rows.labels`, derived, sorted) is
separable from all of that and is the part §3b is actually asking for.

---

## 4. The three surfaces

### Surface A — `datalib-dag` as a harness for any pipeline (ships today)

**Nothing to build.** The DAG config already accepts arbitrary
commands; a config consisting only of custom steps is valid
(`dag/src/config.rs` — `steps` defaults to empty and no step type is
required). So a skill-built pipeline becomes:

```toml
[[steps]]
id = "linear.ingest"
command = "python3 .agents/skills/roadmap/scripts/run.py load"
inputs  = ["uploads/linear"]
outputs = ["linear/store"]
```

and inherits, for the cost of one TOML table: skip-when-unchanged (if
the step prints one content version for its output); retry by failure
class; live progress in a UI and on a stream; graceful cancellation;
and multi-step composition, where a second step declaring
`inputs = ["linear/store"]` runs after it, automatically, and only when
the store actually moved.

`docs/dev/step_protocol.md` is the complete contract and already
carries a runnable Python example. What's missing is not capability
but **findability** — an agent handed a directory of exports and a
ten-minute budget will not discover any of this, because everything
that describes it describes personal-data mirroring. Four things fix
that:

1. A page — "datalib-dag as a pipeline harness" — aimed at someone who
   has never heard of personal-data mirroring, opening with a config
   that contains no datalib providers at all.
2. A ~40-line stdlib `dag_step.py` helper (parse `--params`/`--inputs`,
   read `DATALIB_DAG_CHANGED_INPUTS`, emit progress/outcome, hash a
   tree for the fallback version) that a skill vendors rather than
   reading a spec for.
3. A one-screen primitives index — what exists, one line each, with a
   link: the scheduler, the step protocol, `fswalk`,
   `file_checkpoint`, `blob_cas`, `emit_sidecar`, `edges`, the query
   language. `docs/agent_user.md` is close to this already but is
   organized around "run a sync," not "I have data and a problem."
4. Ship the `doltlite` CLI in `:dist` (§3d), so the store an agent is
   told to query is queryable from the install it was told to do.

A day or two for the first three; item 4 is a release-workflow change.
The discoverability half matters more than it sounds: `data-pipeline-
builder` exists because agents left alone re-derive this badly, and a
primitive nobody finds is indistinguishable from one that doesn't
exist.

### Surface B — a `docs` provider: configured parse over a file tree

Given §2, this is smaller than it first appeared. It is `pdf` with the
extractor replaced by a declarative projection — the scan, the identity
split, the rescan cursor, the ignore cascade, the render path and the
sidecar emit are all already there and already shared.

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

[steps.params.map]         # → the unified grid_rows core
title   = "$.title"
text    = ["$.description", "$.body"]   # first present wins
when_ts = "$.updatedAt"
author  = "$.assignee.name"
project = "$.team.key"
labels  = "$.labels"
edges   = "$.relations[*].targetId"
```

Note what is *not* in that config: any field name datalib knows about.
The fallback lists are the caller's, expressing what their corpus
happens to call things. Datalib ships no built-in vocabulary of source
field names, and shouldn't — that is the single design choice that
keeps this from becoming a parser for whichever corpus we tested
against.

What the caller gets without writing it: the `max(version, batch)`
merge with the ledger in the same transaction; the problem sink
(unreadable file / non-object document / no identity → dropped and
logged; failed coercion → nulled and logged; counts into the ledger
row; the ≥20%-dropped systematic-breakage exit); parallel read with a
single writer at Rust speed; a content version for the scheduler so
everything downstream skips; and `status --json` from the ledger.

Three design calls to make first:

- **Type coercion is the whole game.** Real exports declare an int and
  emit `"312"`, declare a bool and emit `"false"`, and mix
  `"2025-12-08"` with `"2025-05-14T09:12:41-07:00"` and
  `"2025-01-29T16:00:12Z"` across sibling sources. That is the skill's
  §2 oddity list, and it is what the `map` block's coercion rules have
  to survive — a nulled value must be counted and logged, never
  raised, and never guessed at. Datalib's timestamp policy (preserve
  the offset; `parse_with_assumed_utc` only for an audited naive feed;
  null rather than fabricate) already covers the hard case. §5's
  corpus exhibits every one of these, which is why it is a fair test
  rather than a target.
- **Follow a field-name indirection when the record offers one.** Some
  exports self-describe: a record says which of its own fields is the
  title and which are content. A `map` entry should be able to say
  "the field name is in *this* field" — a general indirection, worth
  ~20 lines. What it must **not** become is a built-in list of names
  datalib goes looking for; that is the overfitting failure mode
  (§5).
- **doltlite or plain SQLite?** doltlite buys `dolt_diff`, which is why
  writing *into* the store beats writing next to it. It costs the
  never-deletes property, which collides with §7c retention.
  Recommendation: `store = "doltlite" | "sqlite"` in params, default
  doltlite, and document the tension rather than pretending a prune
  reclaims disk. `doltlite` reads and writes plain SQLite files — it
  only ever *creates* doltlite-format ones — so one reader path covers
  both.

Estimate: one to two weeks with a fixture suite, most of it in the
projection config and its error messages, not in the plumbing.

### Surface C — the pipeline's own view

`fetch-process-show` ends in *show*, and `datalib-http` already hosts
applets: a config-declared server contributing frontend card components
and the endpoints behind them, spawned on demand
(`docs/dev/applets.md`). A skill-built pipeline could ship its own card
over its own table without owning a web stack. Nothing needs building
for the mechanism; what's missing is a worked minimal example.

### Why a binary and a protocol, not a crate

A Rust crate serves consumers who build Rust; this consumer builds
stdlib Python inside a ten-minute budget. A pyo3 wheel is a dependency
the skill's packaging rules forbid, and it couples our release cadence
to theirs at the ABI level. A static musl binary invoked over
`subprocess` with JSON in and NDJSON out is stdlib-compatible, already
built by our release workflow, already installed into Minds workspaces
by `datalib-inspiration`, and versioned by a tag the consumer pins.
Publish crates later if someone asks; lead with the binary.

---

## 5. One instance, used as a check

The skill in PR #534 was built against
[EnterpriseRAG-Bench](https://github.com/onyx-dot-app/EnterpriseRAG-Bench)
(onyx-dot-app; [paper](https://arxiv.org/abs/2605.05253)). This section
describes it in detail for one reason only: it is a concrete,
independently-authored instance of the shape class, so it is evidence
about whether §4 generalizes. Read the field names below as *examples
of the variation a generic ingest has to absorb*, not as a list of
things to support.

**The corpus.** ~500k synthetic documents across nine enterprise source
types — Slack 275k, Gmail 120k, Linear 35k, Google Drive 25k, HubSpot
15k, Fireflies 10k, GitHub 8k, Jira 6k, Confluence 5k. On disk as one
JSON file per document under
`generated_data/sources/<source>/<nested/path>/<slug>.json` (four
sources are checked into the repo; the rest arrive as per-source zips).
Datalib has real providers for four of these already, and Notion is a
close structural analogue of Confluence.

**The documents look like `grid_rows`.** A Confluence page carries
`title`, `space`, `author`, `owner_team`, `status`, `created_at`,
`last_updated`, `labels`, `related_pages`, `content`,
`original_location`, `dataset_doc_uuid`. A GitHub PR carries `repo`,
`pr_number`, `title`, `author`, `state`, `labels`, `linked_linear`,
`linked_jira`, `description`. A Gmail thread carries `thread_id`,
`subject`, participants, and a nested `messages: [...]` array — the
same one-document-many-rows shape datalib renders as one markdown file
with per-message `data-section-uuid` anchors. Every document also ships
`title_field_name` and `content_field_names`, i.e. its own projection
hints, plus a `dsid_…` `dataset_doc_uuid`.

**The answer contract is trivially small.** Systems are scored from a
JSONL file, one line per question:

```json
{"question_id": "qst_0001", "answer": "...", "document_ids": ["dsid_abc", "dsid_def"]}
```

`document_ids` are exactly those `dataset_doc_uuid` values — so a
retrieval system only has to get from a question to a set of document
ids. `metrics_based_eval` scores correctness, completeness, document
recall, and invalid extra documents; `comparative_eval` runs two
systems head-to-head with three-judge consensus voting.

**So the drop-in is four pieces, three of which exist:**

1. `docs.download` + `docs.render` over the corpus root — Surface B.
   Set `identity = "$.dataset_doc_uuid"` so the id the harness wants is
   the id the store is keyed on.
2. `grid_index` + `qmd_index` — unchanged, no per-provider work.
3. A ~50-line harness script: read `questions.jsonl`, query, map hits
   back to `upstream_id`, write the answers JSONL. Runs as a fourth
   `[[steps]]` entry, or standalone.
4. The retrieval strategy itself — the only genuinely new part, and the
   interesting one.

**Why the strategy part is interesting.** The bench's published
baselines are BM25, vector search, and a bash agent with
grep/find/head; its headline negative result is that **vector search
underperformed BM25**, because embedding models trained on public data
do badly on enterprise jargon and structured formats. Datalib's qmd
index is hybrid BM25 + embeddings over the rendered markdown, which is
the configuration that finding argues for. And the bash-agent baseline
is a direct comparison against datalib's own read surfaces: the
question is whether `grid_rows` SQL plus a hybrid index beats an agent
with `grep` over the same tree.

**The strategic point.** `docs/dev/multimodal_retrieval.md` (drafted
2026-09-01) proposes replacing `qmd_index` with a retrieval layer whose
first two forcing constraints are an *arbitrary boolean metadata
prefilter* — not a partition — and *scale*, motivated by a 250k-email
corpus. This benchmark is ~500k documents whose questions are naturally
scoped by source, author, team, and date range, with a public
scoreboard and a comparative-eval harness. That design has no external
number attached to it today; running it here would give it one before
we build it. It is also a genuine load test for `grid_index` and
`qmd_index`, neither of which has been run at that size, and we have no
other corpus that size to hand.

### What would count as overfitting

The value of the exercise is that it is *someone else's* corpus. That
holds only as long as we don't quietly turn it into ours. Concretely,
each of these is a defect, not a shortcut:

- **A config key that only makes sense for this corpus.** Any
  `params` field named after a benchmark concept. `identity =
  "$.dataset_doc_uuid"` belongs in the *harness config*, never in
  datalib's defaults.
- **A built-in field-name vocabulary.** No `title`-detection heuristic
  that happens to know `subject` and `transcript`. If a corpus
  self-describes (this one ships `title_field_name` /
  `content_field_names`), the *mechanism* to follow a field-name
  indirection is general and fine; a hardcoded list of names it looks
  for is not.
- **Tuning retrieval to the question set.** Weighting BM25 against
  embeddings, or picking chunk sizes, by score on these 500 questions.
  The published BM25-beats-vector finding is a reason to *keep* a
  hybrid index; it is not a licence to fit coefficients.
- **A schema column added because the bench has one.** `labels` earns
  its place from Gmail, GitHub, Notion and Slack (§3b). If a second
  column can only be motivated by this corpus, don't add it.
- **Reporting a number without the caveats.** The corpus is synthetic
  and generated for the bench, scoring needs an LLM and API keys, and
  we would be the ones choosing the retrieval strategy. A score here
  is a signal about the shape class, not a product claim.

The test for all of these is the same: **would this change still be
right if the benchmark didn't exist?** If the answer needs the
benchmark, it's overfitting.

---

## 6. Adopting it: the order I'd do these in

The seven gaps in §1 are not equally worth closing, and only one of
them can find a bug that exists *today*. Ordered by that.

**1. The independent spot check — first, because it is the only one
that is a test rather than a process.** Everything else changes how we
handle future data; this one asks whether the twenty providers we
already shipped are lossless right now, and our goldens cannot answer
that: an insta snapshot asserts the output matches what it matched
last time, so a field that has been silently dropped since the day the
provider landed passes forever.

The place to put it is already built and already independent.
`tests/fixtures/ingested_tng_test.py` is **Python**, so it shares no
code with the Rust render path by construction — the strongest form of
the skill's "shares no code with `parse`" rule — and it already opens
both the index db and the per-source entity stores through the
doltlite shell. It also already contains the right idea applied to
identity: `_roundtrip_failures` recomputes each row's uuid from its
stored backpointer and fails when they disagree, on the grounds that
"a mismatch means the backpointer is decorative … broken in a way
nothing else would notice, because both columns still look perfectly
plausible." Extend exactly that reasoning from identity to **content**:
sample N rows per provider, pull the raw payload by upstream id, and
assert the pass-through scalars (`author`, `when_ts`, `text` prefix,
`source_url`) match what the payload says. Anything that fails is
either a bug or a rule that belongs in the judgment-call table.

**2. The problem sink, in render.** Today an unparseable record fails
the step, and the step protocol classifies it as a `data` failure that
poisons every downstream step — including `grid_index`, which fans in
from *every* source. That is right for "the store will not open" and
wrong for "one of forty thousand Slack messages has a field we did not
expect." Add `problem(source, where, field, reason)` writing
`<stanza>/rendered_md/_problems.jsonl` (beside `_render_cursor.json`,
same lifecycle), with per-reason counts in the step's `outcome` event.
This needs a third category in `step_protocol.md`'s absent-vs-malformed
rule: **malformed-but-isolated**, which is neither.

**3. The systematic-breakage threshold.** Falls out of (2) for almost
free once the counts exist: fail the step when a run drops more than
some fraction of what it read. Worth doing specifically because
`data_architecture_ingestion_practices.md`
§"Detecting upstream shape drift" records that we tried a
shape-comparison approach (`endpoint_shapes`), deleted it, and "don't
know yet what we want." Counting drops and thresholding them is cruder
than shape comparison and would have caught the same class of problem.

**4. The judgment-call table.** Once (2) exists this is generated
rather than written: per provider, every rule that nulls a value and
how many records it hit on the last run. Surface it wherever a user
can see it — the Manage tab, or a generated section per provider.

**5. Order-independence — adopt it in §4B, don't retrofit it.** Our
last-complete-write-wins is genuinely sound for one writer walking a
live upstream, and rewriting twenty providers onto `max((version,
batch))` would buy nothing. But any generic on-disk ingest must have
it from the first commit, because that is precisely the input model
where our assumption fails.

**6. Profiling — take the idea, skip the tool.** We write providers by
hand against APIs we can read the docs for, so an agent-facing profiler
is not the win here. Where it *does* apply is §4B: the `docs` provider
should print what it found (identity candidates, null rates, timestamp
shapes) on first run, because there nobody has read the corpus.

**7. Retention** is real but blocked behind doltlite's never-deletes
property (§3c). Separate track; don't let it gate 1–4.

One meta-observation worth recording. The skill is a **procedure** with
time budgets and an explicit list of things never to do ("NEVER run a
full-data inspection or benchmark", "no sweeps, searches, floors, A/B
or timing runs"). `AGENTS.md` is a **runbook of principles**. Both are
aimed at the same failure modes — the ones this repo has hit and
written up, like the DAG runner hashing 3.4 GB for two weeks to version
a step it had already skipped. Principles are better for judgment
calls; a numbered procedure with a stop-list is better at preventing a
specific expensive detour. We have almost none of the second form.

---

## 7. What the skill could take back

Six things this codebase learned by getting them wrong first, each
mapping to a section the SKILL.md already has.

**1. Sort unordered collections before storing them.** A JSON array is
not necessarily a list. When a source returns a *set* — permissions,
tags, labels, member ids — the order is whatever the server emitted,
and nothing promises it's stable between fetches. Left unsorted, a
re-export of an unchanged object serializes differently from itself and
everything downstream believes it changed. Found here on 2026-08-31:
claude.ai returns a project's eight `permissions` strings in different
orders on different fetches. **Sort it; don't declare it volatile** —
dropping the field loses a real signal, sorting removes only the noise.
This is the exact failure mode of the skill's target case (recurring,
overlapping re-exports); it defeats §8's "export is identical for every
batch ordering" assertion in a way that looks like a merge bug; and
§4's tie-break paragraph doesn't cover it. Note that the benchmark
corpus is full of such bags — `labels`, `topics`, `reviewers`,
`related_pages`, `competitors_mentioned`.

**2. Preserve the source's UTC offset; never fabricate a timestamp.**
Store the offset the source gave you (`-07:00` stays `-07:00`): you can
recover UTC from an offset but not the offset from UTC, and the offset
is how the timestamp read to the human who saw it. When there is no
timestamp, store **null** — not epoch, not now, not midnight. §1 asks
the agent to *find* absurd timestamps but doesn't say what to store,
and a plausible-looking fallback is the easiest way to make
incompleteness indistinguishable from data.

**3. The projection-vs-payload trade is conditional, and §3 states only
one side.** §3 says to store the projection, not raw records —
"10-50x smaller." Datalib deliberately does the opposite at the raw
layer (payload as JSONB, normalize at render) so a parser bug is a
re-render, not a re-fetch. Both are right in context: the skill's
target is *batches on local disk*, where the raw inputs are the archive
and a re-parse is free — the skill says so. But an agent that carries
§3 to an API-backed pull, where the batch is gone once the window
closes, has thrown away data it cannot get back. One clause closes it:
"because the raw batches remain on disk; if the source is an API you
cannot re-pull, store the payload too."

**4. Split per-attempt bookkeeping off the record row.** Datalib pairs
every entity table `<t>` with a `<t>_bookkeeping` sidecar carrying
`fetched_at`, `attempt_count`, `last_error`. The reason is the skill's
own problem: bookkeeping changes on every attempt regardless of whether
the source changed, so storing it on the record makes every diff noisy
and makes unchanged content look changed. §7b puts problem *counts* in
the ledger row, which is right; the general rule is that nothing which
moves per attempt belongs on the record.

**5. Absent is not malformed.** From the step protocol: a source that
has never been downloaded emits nothing and exits 0; a store that
exists and won't open fails loudly. §7b's "exit non-zero only when
nothing could be done" is the same instinct; the sharper phrasing is
the pair — **absent vs malformed, and only the second is an error** —
because in a DAG a wrong `data` failure poisons every step downstream,
including ones that had nothing to do with the empty source.

**6. Prefer failing loudly to succeeding quietly.** The dangerous
fallbacks are the ones that *succeed* — a correct answer reached the
slow or lossy way raises no error, so an assumption that expired weeks
ago hides behind a vague "feels slow." Worked example: the DAG runner
spent 40s per run hashing 3.4 GB to version a step it had already
skipped, every run, for two weeks, before anyone noticed. If you add a
fallback, log when it fires.

---

## 8. Open questions

1. **Harness, store, or both?** If a skill-built pipeline is happy with
   its own SQLite and only wants scheduling, incrementality, retry and
   a UI, Surface A is the whole answer and B is speculative. If the
   store is worth sharing — for `dolt_diff`, or for the datalib read
   surfaces — B is the one that matters. This is a day versus two
   weeks.
2. **Is the benchmark run worth doing first?** It is the cheapest way
   to find out whether §4B generalizes, it produces a number for a
   design we are about to commit to, and most of it is a throwaway
   script. Running it *before* the generalization is the unusual but
   correct order — provided the §5 guardrails hold and the harness
   config stays outside datalib.
3. **Is retention a hard requirement?** If yes, Surface B probably
   defaults to plain SQLite with doltlite opt-in — a real reversal of
   datalib's current bet, worth deciding on purpose (§3c).
4. **Does the ten-minute budget survive reading a second contract?**
   The step protocol is one page, but it *is* a page. If the answer is
   no, datalib ships `dag_step.py` and the skill vendors it, so the
   agent copies a file instead of reading a spec.

## See also

- [`docs/dev/step_protocol.md`](step_protocol.md) — the step contract;
  Surface A is a packaging exercise on top of it.
- [`docs/dev/multimodal_retrieval.md`](multimodal_retrieval.md) — the
  retrieval design §5 proposes scoring against the benchmark.
- [`docs/dev/pipeline_dag_architecture.md`](pipeline_dag_architecture.md)
  — the scheduler: edge derivation, skipping, retry, subtree poisoning.
- [`docs/dev/data_architecture_ingestion.md`](data_architecture_ingestion.md)
  — the principles §1's table is checked against.
- [`datalib/backend/etl/providers/pdf/src/download/schema_raw.rs`](/datalib/backend/etl/providers/pdf/src/download/schema_raw.rs)
  — the content-vs-path identity argument, and the template Surface B
  should be built from.
- [`docs/agent_user.md`](../agent_user.md) — what an agent using datalib
  reads today, and the doc Surface A would extend.
