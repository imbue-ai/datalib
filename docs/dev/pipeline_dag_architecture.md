# Pipeline architecture: toward an arbitrary processing DAG

> **Status (2026-07):** implemented. The scheduler and step contract live in
> `frankweiler/backend/dag` (the `datalib-dag` runner binary), the step-type
> host in `frankweiler/backend/datalib_step` (`datalib-step`), and the http
> worker/UI drive syncs through them. Review notes from the original
> addendum are folded in below as **Addendum** blocks, each with what the
> prototype chose.


## Motivation

The system we want to grow into is one that can define an arbitrary DAG of data-processing pipelines, where each node represents ones of:

* Materialized data at rest, on disk  
* A process that can:  
  * Optionally consume data from various input sources  
  * Optionally produce materialized data on disk

Today we have a fixed set of data sources that each run two steps — extract (ingestion) and translate — plus a final "load" step that merges everything together. We'd like to evolve toward the general shape.

Two questions frame the design:

1. How far are we, architecturally? What is already DAG-shaped, what is hardcoded, and what concretely has to change?  
2. The ingestion step is required to have certain properties (incrementality, resumability, idempotency, …). Is that concern mainly *within* ingestion — an ingestion run leaves behind state the next run reuses — or does the outer orchestrating system need to know about and exploit those properties? Monitoring progress is one thing the orchestrator cares about; what else?

This document answers both and specifies the node contract, scheduler, and migration that follow from the answers. Like the [ingestion](https://github.com/imbue-ai/mixed_up_files/blob/19f09d64fa1994317fc06f3e2d8bd4d29dfae9b7/docs/dev/data_architecture_ingestion.md) and [post-ingestion](https://github.com/imbue-ai/mixed_up_files/blob/19f09d64fa1994317fc06f3e2d8bd4d29dfae9b7/docs/dev/post_ingestion_architecture.md) docs, it is aspirational as much as descriptive: it describes contracts we intend to publish, not ones that exist today. Divergences should be either justified or fixed.

The framing constraint from the ingestion doc still holds — single user, single laptop, "no cluster, no scheduler service, no DAG server." The DAG here is an *in-process (then local-subprocess) scheduler over disk artifacts*, not a Prefect/Airflow deployment.

## Where things stand today

The pipeline is three stages running in-process inside frankweiler-sync — one binary, statically dispatched over a closed enum of sources (SourceConfig, [core/src/config.rs](https://github.com/imbue-ai/mixed_up_files/blob/19f09d64fa1994317fc06f3e2d8bd4d29dfae9b7/frankweiler/backend/core/src/config.rs)):

```
raw/<source>/*.doltlite_db  →  rendered_md/<source>/*.md + *.grid_rows.json  →  dolt_db/ (index)
        (extract)                          (translate)                              (load)
```

What is already DAG-shaped, and is the expensive part to get right:

* Artifacts are immutable files on disk. Every stage reads files and writes files; there is no in-memory handoff. This is exactly the "node \= a process reading input, writing output" contract.  
* Change detection already exists. compute\_row\_set\_hash ([etl/src/load.rs](https://github.com/imbue-ai/mixed_up_files/blob/19f09d64fa1994317fc06f3e2d8bd4d29dfae9b7/frankweiler/backend/etl/src/load.rs)) and the per-document source\_fingerprint / markdowns skip logic already answer "did this output change?" — which is precisely the cache layer a DAG scheduler needs to decide what to re-run. We built incremental re-execution before we built the DAG.

What is hardcoded — the actual gap:

1. No node abstraction. A "stage" is match arms over the 13-variant SourceConfig enum in [sync/src/main.rs](https://github.com/imbue-ai/mixed_up_files/blob/19f09d64fa1994317fc06f3e2d8bd4d29dfae9b7/frankweiler/backend/sync/src/main.rs), not a uniform interface. Adding a node means editing the orchestrator.  
2. Fixed linear topology. Extract → Translate → Load is wired by hand, not derived from declared dependencies. There is no topological scheduler.  
3. Load is fused into Translate — it runs as an on\_doc\_complete callback inside the translate loop, so there are really 2.5 stages.  
4. In-process, statically linked. Providers are separate crates but are linked into one binary and dispatched by match, not spawned as processes.  
5. Whole-run atomicity. One dolt commit at the end, or roll back *everything* if any source errors. This is the largest impedance mismatch with a DAG, where nodes must commit independently.

The conclusion: the data-flow model is most of the way to the target; the orchestration model is the opposite of a DAG engine. Those are separable, and only the second needs rework.

## Within-ingestion vs. the outer system: mechanics vs. contract

The required ingestion properties — incrementality, idempotency, resumability, cursors, determinism, monitorability, stoppability — split cleanly along one line:

The mechanics of a property are private to the node. The guarantee is a contract the orchestrator exploits.

The orchestrator must never need to understand that "the dedup index is the resume cursor" or how \<table\>\_bookkeeping tracks retries. **A node achieves incrementality however it likes.** But several properties only pay off if the outer system *knows the guarantee holds and acts on it*. Monitoring is one such; the full set the scheduler genuinely needs:

1. Change-detection signal (the load-bearing one). The scheduler must ask each node "did your output actually change?" to decide whether to re-run *dependents*. Incrementality inside a node is wasted if the outer system can't see its result. Today this leaks: the orchestrator reaches *into* doltlite (markdowns.source\_fingerprint, dolt\_diff\_\<table\>, scope-state snapshots) to derive it. The fix is to make it a thin declared output: a content version the node exposes, mechanics hidden (see [Output version](#node-output-in-addition-to-materializingupdating-data)).  
2. Retry-safety as an advertised guarantee. The orchestrator's failure handler is "re-invoke the node." That is only safe if the node *promises* idempotency/resumability. The orchestrator does not implement it — it relies on it (see [Failure & retry](#failure-and-retry)).  
3. Output completeness / atomicity. Before a dependent consumes an artifact, the scheduler needs to know "is this a committed checkpoint or a partial write?" Today's whole-run commit answers this for the *whole run*, not per node (see [Per-node commit](#per-node-commit)).  
4. Failure classification → retry policy. A rate-limit error → back off and retry; an auth error → fail fast; a parse error → fail this node, not the graph. The node must surface *which* (see [Failure & retry](#failure-and-retry)).  
5. Progress / monitoring — the one named in the motivation. Generalize the current Progress plumbing to a uniform per-node event stream (see [Progress](#progress-reporting-and-logging-info-warn-errors)).

So the boundary the orchestrator cares about is a thin contract: {output-version, completion-status, failure-kind+retry, progress}. Today that contract exists only implicitly, expressed as the orchestrator reaching directly into ingestion's doltlite tables. Formalizing that leaky reach into a declared node interface is the central refactor — and it generalizes, because translate, load, and every future node want to expose the same four things.

## The node contract

> **Addendum — terminology.** "Node" reads as data, but this is an action;
> other DAG runners say *task* (Airflow/Luigi/Prefect), *op* (Dagster),
> *action/rule* (Make/Bazel), *transform* (Beam). The prototype adopted
> **step** ("task" collides with `tokio::task` throughout the workspace),
> with **artifact** for the data side.
>
> The step *types* are named **download**, **render**, **grid_index**, and
> **qmd_index** throughout the codebase. The names this document's prose
> uses below — *extract*, *translate*, *load* — are the historical
> (pre-DAG) terms for the first three; they survive only in design docs
> like this one.


A node is the unit the scheduler schedules. It declares what it reads, what it writes, and how to run it; everything else is private.

```
node1:
  name: foo
  inputs:
    - artifact1 (aka folder1)
    - artifact2 (aka folder2)
  outputs:
    - artifact3 x.doltdb
    - artifact4 y.doltdb
  run:
    my_cool_data_processing_command
```

struct NodeSpec {  
    id: NodeId,  
    /// Artifacts this node reads. Edges in the DAG are derived from the  
    /// overlap of one node's \`outputs\` with another's \`inputs\`.  
    inputs:  Vec\<ArtifactRef\>,  
    /// Artifacts this node produces. A node MUST write only these.  
    outputs: Vec\<ArtifactRef\>,  
    /// How to run it. In-process today; a spawned process later — same spec.  
    run: NodeRun,           // InProcess(fn) | Subprocess { argv, env }  
}

ArtifactRef is a path or glob under data\_root. The DAG is not declared explicitly — it is derived from input/output overlap, the same way a build system derives its graph from declared deps. This keeps node definitions local: a node names its files; the scheduler computes the edges.

The contract a node honors, beyond reading inputs and writing outputs:

* Idempotent & resumable. Re-invoking a node on the same inputs is safe and cheap; an interrupted node makes forward progress next run. This is what makes retry trivial for the scheduler.  
* Outputs are content-stable. Identical inputs produce byte-identical meaningful output (modulo incidental fields excluded from the version hash). This is what makes change-detection trustworthy.  
* Atomic outputs. A node's outputs are valid-or-absent; a partial run never leaves a half-written artifact a dependent might consume.

These are exactly the ingestion properties — now stated as a *general node contract* rather than an ingestion-specific one.

### Node output, in addition to materializing/updating data

When a node runs, it says, per output artifact, whether that artifact changed.  If any of a node’s inputs changed, it gets scheduled to run, and can inspect the inputs further to decide whether to actually run.

### Failure and retry

Nodes manage their own failure and retries.  Even if a node fails, it may still have incrementally updated its output data, and signals that the output data is updated in this case.

Both intermittent and “permanent” failures can be reported to the monitoring system (below).

### Per-node commit

If a node is writing to a doltlite DB, it should manage its own commits and always be sure to commit before it says it is “done”.

### Progress reporting and logging (info, warn, errors)

The end goal is to be able to drive a “dashboard” that summarizes what’s happening, how much work has been done at various stages, and if known, how much more there is to do, per step.

Think of the Flume / Apache Beam monitoring dashboard, which exposes counters that can be incremented by the workers as they make progress, and can surface warning and error conditions.  See: [https://beam.apache.org/releases/typedoc/current/classes/transforms\_pardo.Counter.html](https://beam.apache.org/releases/typedoc/current/classes/transforms_pardo.Counter.html)

We don’t want to be as complex as the Flume dashboard, but probably just show, per running node, a running tally of “progress”, and if known, how close we are to the end.

Today progress is purely in-process: a Progress handle wrapping Arc\<dyn ProgressSink\> ([etl/src/progress.rs](https://github.com/imbue-ai/mixed_up_files/blob/19f09d64fa1994317fc06f3e2d8bd4d29dfae9b7/frankweiler/backend/etl/src/progress.rs)) is passed *into* each stage (FetchOptions { progress, .. }). A FanOut sink drives terminal bars (IndicatifSink, in-process only), structured tracing events (TracingSink, which already serialize as NDJSON on stderr), and OTLP spans to \--otlp-endpoint.

A process-boundary-friendly channel therefore already exists: TracingSink's NDJSON and OTLP both already cross process lines. The migration is small and the design choice is:

Nodes emit a progress NDJSON stream; the orchestrator ingests and renders it. Not an HTTP API. Reasons: it already exists (TracingSink), it is the Unix-y model that matches "node \= a process writing output" (no ports, no registration, no per-child auth token), and it *fixes* a coupling — today each stage's sink knows how to draw terminal bars, whereas under this design nodes only emit events and the orchestrator owns all rendering, reconstructing bars from the ingested stream. Node-side, the ProgressSink trait is unchanged; only the implementation swaps IndicatifSink for an NdjsonSink writing to a dedicated fd.

The event schema is essentially what TracingSink already emits:

{"event": "progress.length",  "node": "\<id\>", "total": 42}  
{"event": "progress.inc",     "node": "\<id\>", "delta": 1}  
{"event": "progress.message", "node": "\<id\>", "msg":   "conversations.list page 1"}  
{"event": "progress.finish",  "node": "\<id\>", "msg":   "done"}

Complementary channels, not either/or:

* OTLP already works per-process for free — each subprocess can ship spans/events to a collector independently. Keep it for production/distributed observability.  
* A control channel (orchestrator → node: cancel, pause) is the thing that genuinely needs a process-crossing mechanism, since today's control handle is in-process. That is the natural place for a small socket/HTTP endpoint or a control pipe — *not* progress, which is high-frequency and one-directional.

A side goal here is to report enough information to get a sense of USE metrics: [https://www.brendangregg.com/usemethod.html](https://www.brendangregg.com/usemethod.html)

## User YAML configuration vs. “under-the-hood” DAG representation

> **Addendum — config format.** After generalizing into the DAG runner the
> config format changes completely: the current "data sources" survive,
> but only their download step is a distinct step type; subsequent
> processing steps are instances of shared step types (a few sources with
> unique post-processing keep their own types). The shared post-download
> steps take "the output of all download steps" as input, so the format
> needs a wildcard input as long as it fits the design.
>
> *What the prototype chose:* the macro layer was dropped entirely — the
> config declares steps directly (`<source_type>.download` /
> `<source_type>.render` per source, plus shared `grid_index`/`qmd_index`
> fan-ins).
> Wildcard inputs (`*`/`**`) exist and match against declared output
> *roots* only — true tree-intersection semantics would make `**/x`
> depend on every step. In practice render turned out to be as
> provider-specific as download (each provider renders its own raw
> schema); the genuinely shared step types are `grid_index` and `qmd_index`.


The user should not typically be bothered to configure the detailed dependency graph.  Instead, they’re likely to configure “macro-like” functions that in turn create sections of the dependency graph, very similar to how Flume / Apache Beam code run functions that assemble sections of the dependency graph.

For example, we might want a single YAML stanza that assembles a “pipeline subsection” including:

* Extract data from a source and store it.  
* Render it to markdown  
* Run qmd-like indexing on the markdown

Rather than declare a sequence of 3 DAG steps explicitly, the user should be able to write a single stanza with a few knobs that control whether/how these things happen.

Similarly, a YAML stanza for processing a Google Takeout ingestion might, under the hood, be represented as several different DAG steps that could all run in parallel (a 1-2-3 chained sequence of extract-render-index on all of Google Chat, Google Voice, and Maps Reviews, with each 1-2-3 chain runnable in parallel).

## Storage layout

> **Addendum — storage layout.** Not settled; do whatever results in the
> least change for now. The prototype kept the existing by-source layout
> (`<stanza>/raw`, `<stanza>/rendered_md`, `system/…`), which already
> groups data by source as sketched above.


We currently have a global (per YAML config file) “data\_root”, in which all named “steps” write their data into known locations.

Thad: I don’t love the current layout.  One thing I really don’t like is that the YAML stanza names are currently used in multiple subsections of the data root:

* Raw  
  * Slack  
  * Email  
* Rendered\_md  
  * Slack  
  * Email

Thad: What I think I’d like is better is keeping data together by source rather than by type.  And if a YAML stanza creates a logical “grouping” of related work and artifacts, all that work

So inside data\_root (or wherever), “artifacts can 

* Slack  
  * Extract  
    * slack.doltlite\_db  
    * slack.blobs.doltlite\_db  
  * Render  
    * A tree of markdown (for now)  
  * qmd\_index  
* google\_takeout  
  * Chat  
    * Extract  
    * Render  
    * qmd\_index  
  * Voice  
    * Extract  
    * Render  
    * qmd\_index  
  * Maps Reviews  
    * Extract  
    * Render  
    * qmd\_index

## Migration, in dependency order

1. Define the node contract (NodeSpec, NodeOutcome, output-version hash) and have extract/translate/load implement it *in-process*, replacing the enum-dispatch match arms. No behavior change yet.  
2. Write the scheduler: derive edges from input/output overlap, topologically order, run ready nodes, skip nodes whose input versions are unchanged (reusing the content-hash signal already in the tree).  
3. Per-node commit: break whole-run atomicity into per-node atomic output commit; implement subtree-poisoning failure semantics.  
4. Un-fuse Load from the translate callback into a first-class node that consumes the sidecar tree like any other.  
5. Progress NDJSON: swap IndicatifSink for an NdjsonSink; move bar rendering into the orchestrator's ingest loop. (Independent of 1–4; can land any time.)  
6. Subprocess execution (optional, last): flip NodeRun::InProcess to Subprocess. The contract is unchanged; this buys isolation and language-independence and can be deferred indefinitely.

Steps 1–2 deliver a real, generic DAG over the existing stages without touching atomicity. Step 3 is the hard, invasive one. Steps 5–6 are independent and can be reordered.

## Implementation decisions (2026-07)

Decisions made while building the prototype into the real thing, in
rough dependency order:

* **Wildcard inputs match declared output *roots*, not trees.** True
  tree-intersection makes `**/x` overlap every output (a match could
  always exist deeper inside), i.e. every wildcard input would depend on
  every step. A wildcard that matches nothing resolves to the empty set
  (a starter config declares the fan-in steps before any source
  exists); an input with no producer must be a concrete path (a
  user-staged "external" artifact, content-hashed by the scheduler).
* **Per-provider step types.** Download *and* render are provider-
  specific (each render reads its own raw-store schema), so the config
  writes them as `<source_type>.<phase>` (`slack_api.download`); the
  genuinely shared step types are `grid_index` and `qmd_index`. Params carry
  `{name, source}` with no `type:` tag — the step type names the
  provider, and `source:` deserializes into that provider's own config
  struct. Provider crates expose per-wave entry points
  (`plan_download` / `plan_render`); a later per-phase split of the
  config *structs* is where render-relevant knobs still living in
  `sync:` (beeper/signal `period`, perseus `alignment_pairs`) move out.
* **Fringe steps always run.** A download step's real input is a remote
  service the scheduler can't version, so "run iff inputs changed"
  degenerates to "always invoke"; internal incrementality makes that
  cheap and the step reports whether outputs actually moved.
* **Change detection is three-tier.** A step-reported version (row-set
  hash, dolt commit) is trusted verbatim; a step reporting "unchanged"
  carries the prior version forward; otherwise the scheduler
  content-hashes the output tree (blake3, content not mtime). The
  fallback is the safety net — real steps should report logical
  versions. The grid_index step's version is its dolt commit hash.
* **Scheduler state** lives at `system/state/dag_state.json`
  (per-step input/output versions, saved after every terminal step).
  Input versions are recorded only on *success*, so a failed step stays
  dirty automatically — no explicit poisoned flag persists. A failed
  incremental step's reported partial outputs are still recorded.
* **Load is un-fused by force, not choice.** The single-writer rule
  (no two steps' output trees may overlap) makes per-source writes into
  the shared index impossible, so `grid_index` is one fan-in step driving
  `load_all` over every `.grid_rows.json` sidecar tree. Render uses its
  own sidecar tree as the prior-fingerprint store — the artifact is the
  resume state, no index-DB peeking.
* **Everything goes into the NDJSON stream** (stderr of the runner):
  `run_plan` (all step ids, topo order) opens the run, then
  `step_start` / `progress_*` / `log` / `hint` / `step_finish`, closed
  by one `run_summary` — the machine-readable run record replacing
  sync's summary JSON file (callers tee the stream to persist it).
  Subprocess steps speak the same schema on stdout with a final
  `outcome` line; unparseable stdout lines and all child stderr are
  wrapped as `log` events (tracing-JSON lines keep their own
  severity). Auth failures emit the per-provider latchkey walkthrough
  as a structured `hint` event.
* **Failure taxonomy → policy split.** Steps only *classify*
  (`transient` / `rate_limited` / `auth` / `data` / `cancelled`); the
  scheduler maps kinds to retry (transient/rate-limited retry with
  backoff, the rest fail fast) and blocks the subtree below a failure
  while siblings continue.
* **Cancellation is graceful end to end.** The runner forwards
  SIGINT/SIGTERM to running steps as SIGINT; `datalib-step` fires the
  provider checkpoint hooks (partial state gets a proper dolt commit)
  and exits 130 with a `cancelled` outcome; `kill_on_drop` plus a
  child-pid registry guarantee no orphaned downloads. The http worker
  SIGTERMs on cancel and only SIGKILLs after a grace period.
* **Subset sync** (`--sync <fringe-id>…`): selected download steps run,
  the rest are treated as up to date, downstream follows normal change
  propagation. The UI's per-source / multi-select "Sync now" maps onto
  it (`<name>.download` id convention), the whole selection as one run.
* **Run-wide `--now`** is threaded by the runner to every step (sampled
  once when omitted) so all stamped outputs agree; reset controls
  (`--reset-and-redownload`, `--refetch-blobs`) pass through to
  download steps only.
* **UI progress is the task board, not stages.** The worker consumes
  the event stream into per-task states (todo/running/done/skipped/
  failed/blocked) rendered as one cell per task; `GET /api/dag` serves
  the derived graph via the runner's own load → specs → graph chain so
  the visualization can't drift from execution.
* **Legacy configs migrate, not break**: old `sources:` files are
  detected and converted server-side into step pairs for review.
* **The data-root layout is unchanged** (`<name>/raw`,
  `<name>/rendered_md`, `system/…`), so roots move freely between the
  old and new binaries; the only addition is `dag_state.json`.

## Unresolved questions

* Edge derivation granularity. Deriving edges from path/glob overlap is simple but coarse — a node that writes rendered\_md/\<source\>/\*\* and one that reads rendered\_md/\*\* are correctly linked, but per-document parallelism within that edge is lost. Do we want sub-artifact edges, or is whole-tree granularity enough for a single-laptop workload? *(Resolved: whole-tree edges suffice — per-document incrementality stays inside the step, via fingerprints; that's the mechanics-vs-contract split doing its job.)*  
* Where the scheduler state lives. (node\_id → output\_version, status) per run needs a home. A pipeline\_runs table alongside the existing sync\_runs is the obvious candidate; whether it lives in the index DB or a dedicated control DB is open.  
* Cross-node content hashing for non-row outputs. row\_set\_hash covers GridRow outputs. A node whose output is, say, a blob tree or a derived index needs its own canonical content hash. Is there a single reusable hashing discipline, or is it per-output-type by necessity? *(Resolved: per-output-type by necessity — steps report their own logical versions; a blake3 tree hash is the generic fallback.)*  
* Dynamic graphs. This design assumes the node set is known before the run. A node that *discovers* downstream work (e.g. fan-out per conversation) currently lives inside one node. Do we ever need the scheduler itself to expand the graph mid-run?

