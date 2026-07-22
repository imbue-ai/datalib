# Program B (aspirational): the processing DAG

> **Archived (2026-07): implemented.** The DAG runner shipped as
> `datalib-dag` + `datalib-step` (`frankweiler/backend/dag`,
> `frankweiler/backend/datalib_step`) under different names than this plan
> uses: `NodeSpec`/`NodeOutcome`/`frankweiler_pipeline` became
> `StepSpec`/`StepOutcome`/`frankweiler_dag`, and the extract/translate
> vocabulary became download/render/grid_index. For the current design see
> [`pipeline_dag_architecture.md`](../pipeline_dag_architecture.md); for
> the step contract see [`step_protocol.md`](../step_protocol.md).

> **Aspirational — do not start until Program A is complete and its
> `DataProcessor` trait has proven stable across all providers.** The canonical
> *vision* is `Prompt to Claude for conversion to DAG runner.md`. This document is
> narrower: it records how the DAG builds **on top of Program A**
> ([`data_processor_and_config_refactor.md`](./data_processor_and_config_refactor.md)),
> what it deliberately changes about A's design, and where the genuinely hard
> work lives — so that when we do pick it up, we start from the right place
> rather than re-deriving it.

## 0. Premise: what A gives us, what B adds

Program A delivers: every provider **owns its config**; extract and translate are
**separate single-method (`run()`) `DataProcessor`s**, built by the provider into
a `SourcePlan` and dispatched through a registry — dependency rule enforced
structurally. But A deliberately keeps today's **two global waves (extract →
translate) + load-once**, grouped coarsely (no order derived from I/O), and the
orchestrator-owns-commit model. A is a *dispatch + factoring* cleanup.

**Naming (from A's §1a).** `DataProcessor` is the one general trait. A **graph
node** *is* a `DataProcessor` placed in the DAG. In B, a **data source** is a
`DataProcessor` with no artifact inputs (it reaches the external world) and a
**data transform** has artifact inputs — same trait, the distinction is the
declared inputs. (In A that distinction was coarser: just "which wave.")
`DataSource` / `DataTransform` subtraits appear only if a concern is genuinely
source- or transform-only. The scheduler struct names below (`NodeSpec`,
`NodeOutcome`, …) keep the graph term "node"; the behavior they wrap is a
`DataProcessor`.

Program B is a *topology* change. A already gives us separate `run()` processors;
B **replaces A's coarse two-wave grouping with a real DAG** whose order is
*derived* from each processor's declared inputs/outputs, and adds the guarantees a
scheduler needs. The one-line reframe:

> A provider's builder should declare each processor's **inputs/outputs** so the
> scheduler derives the graph — instead of A's hand-grouped `{extract, translate}`
> waves.

Under A, email-extract and email-render are already two separate `DataProcessor`s,
but they run in two fixed waves. Under B they are two nodes with a **derived**
artifact edge, and the scheduler decides what runs and what is skipped
(change-detection).

## 1. What B changes (the trait shape does **not** change)

A already made the trait single-method `run()`, with extract and translate as
separate processors. B leaves that trait shape alone and adds machinery *around*
it:

- **Declared inputs/outputs.** Upgrade the provider's builder from A's
  `SourcePlan` (coarse `{extract, translate}` waves) to `Vec<NodeSpec>`, where
  each processor declares its input and output `ArtifactRef`s. The scheduler
  derives edges from output⇄input overlap; A's two-wave grouping goes away. This
  is the "macro that assembles a DAG section" from the vision doc (google_takeout
  → several parallel extract→render→index chains).
- **Structured outcome with a content-version.** Replace A's `run() -> String`
  summary with `run() -> NodeOutcome` (§2) carrying per-output content-versions +
  changed flags — the load-bearing signal a scheduler skips on. A scheduler
  cannot decide what to re-run from a log line.
- **Storage-shape generic.** A's `raw_store_path: Option<PathBuf>` privileges
  doltlite; B's `ArtifactRef` is storage-agnostic (a db, a markdown tree, a blob
  tree, an index dir), so a transform whose output is a derived index fits.

A's config-ownership, registry, capability traits (`HasEventTape`,
`HasSynthesizer`), and the orchestrator's open-pool/commit wrapper all carry
forward; B changes *the orchestration around the trait*, not the trait.

## 2. The node contract

```rust
// crate: frankweiler_pipeline (base)
pub struct ArtifactRef(pub PathBuf);        // path or glob under data_root

pub struct NodeSpec {
    pub id: NodeId,                          // "email/fastmail/extract"
    pub inputs:  Vec<ArtifactRef>,
    pub outputs: Vec<ArtifactRef>,
    pub run: NodeRun,                        // InProcess(Arc<dyn DataProcessor>) | Subprocess{argv,env}
}

/// The one general trait (§Naming). A *data source* is a DataProcessor with no
/// artifact inputs; a *data transform* has artifact inputs. Same trait.
#[async_trait]
pub trait DataProcessor: Send + Sync {
    async fn run(&self, ctx: &NodeCtx<'_>) -> Result<NodeOutcome>;
}

pub struct NodeOutcome {
    pub outputs: Vec<OutputStatus>,          // { artifact, version: ContentVersion, changed: bool }
    pub status:  Completion,                 // Complete | PartialProgress
    pub failure: Option<FailureKind>,        // RateLimited{retry_after} | Auth | Parse | Other
}
```

The DAG is **derived**, not declared: edges come from one node's `outputs`
overlapping another's `inputs`, the way a build system derives its graph. Node
definitions stay local — a node names its files; the scheduler computes edges.

Node contract (these are today's ingestion properties, generalized): idempotent &
resumable; content-stable outputs; atomic outputs (valid-or-absent); a node that
writes a doltlite db commits before reporting `Complete`.

## 3. The two hard cores (why B is not "just more A")

These are the reasons B is a real research-and-engineering effort, not a
mechanical follow-on.

1. **Trustworthy output-versions / change-detection.** The scheduler skips a node
   when its inputs' versions are unchanged. A *wrong* skip produces silent stale
   output — the worst failure mode. Today's change-detection is three different
   shapes fitted to their output types (`row_set_hash` for GridRows,
   per-doc `source_fingerprint`, `dolt_diff_<table>` render cursors). B must
   define a canonical `ContentVersion` discipline per output type and make the
   skip logic provably correct. This is the load-bearing intellectual work.

2. **Per-node commit + failure semantics + interrupt decoupling.** A keeps the
   orchestrator owning each pool so the Ctrl-C handler can commit against the live
   pool. B breaks that: each node commits its own outputs atomically and reports a
   `FailureKind`; the scheduler implements **subtree poisoning** (a failed node
   fails its dependents, not the whole graph). This is the vision doc's "hardest,
   most invasive" step and touches the interrupt machinery directly.

Do not begin B until there is appetite for *these two*, specifically.

## 4. The rest of B (each independent-ish)

- **Un-fuse Load.** Lift Load out of the translate `on_doc_complete` callback into
  a first-class node consuming the sidecar tree as a normal input.
- **Progress as a per-node NDJSON stream.** Swap `IndicatifSink` for an
  `NdjsonSink` on a dedicated fd; move bar rendering into the orchestrator's
  ingest loop; reconstruct bars from the stream. Node-side `ProgressSink` trait
  unchanged. Independent of everything else; could even land during/after A.
  Keep OTLP per-process. A **control** channel (cancel/pause) is the only thing
  needing a real process-crossing mechanism later.
- **Storage grouped by source, not by type.** `data_root/<source>/{extract,render,
  index}/…` instead of `raw/<source>` + `rendered_md/<source>`. Natural once
  `plan()` receives a `Layout` and declares its own `ArtifactRef`s. Do it with a
  read-old/write-new compatibility shim; golden fixtures catch unintended diffs.
- **Subprocess execution (last, optional).** Flip `NodeRun::InProcess` to
  `Subprocess`. Contract unchanged; buys isolation + language independence; defer
  indefinitely. The single-laptop framing (no cluster, no scheduler service)
  still holds — this is an in-process-then-local-subprocess scheduler over disk
  artifacts.

## 5. Migration sketch (assumes A complete)

1. `frankweiler_pipeline` crate: `NodeSpec`, `Node`, `NodeOutcome`, `ArtifactRef`,
   `ContentVersion`. Types only.
2. Give one provider (email — its render path is already unified and its config is
   already clean from A) a `plan()` emitting 2 nodes; run them through a minimal
   scheduler; keep others on A's two-wave `SourcePlan` path.
3. Real scheduler: derive edges from I/O overlap, topo-order, skip on unchanged
   input-versions (reuse existing content hashes); migrate providers to `plan()`.
4. Un-fuse Load (§4).
5. Per-node commit + failure classification (§3.2) — the hard one.
6. Progress NDJSON, storage-by-source (§4) — independent.
7. Subprocess (§4) — optional, last.

## 6. Open questions

- **Edge-derivation granularity.** Whole-tree (`raw/<name>/**` →
  `rendered_md/<name>/**`) is simple but loses per-document parallelism within an
  edge. Sub-artifact edges, or is whole-tree enough for a single-laptop workload?
- **Output-version hashing discipline.** One reusable canonical hash across output
  types, or per-output-type by necessity? (See §3.1.)
- **Scheduler state home.** `(node_id → output_version, status)` per run — a
  `pipeline_runs` table beside `sync_runs`, in the index db or a dedicated control
  db?
- **Dynamic graphs.** A node that discovers downstream work (fan-out per
  conversation) does it *inside* one node today. Do we ever need the scheduler to
  expand the graph mid-run, or is intra-node fan-out enough?
- **User YAML vs derived DAG.** Users configure "macro-like" stanzas that expand
  into DAG sections (per the vision doc), not raw node graphs. Confirm `plan()` is
  the right expansion seam and that one stanza → many nodes stays legible.

## 7. Relationship to the other docs

- `Prompt to Claude for conversion to DAG runner.md` — the canonical vision and
  motivation. **Authoritative for intent.**
- `data_processor_and_config_refactor.md` — **Program A, do first.** B's `plan()`,
  owned config, registry, and dependency architecture all assume A is done.
- This doc — the bridge: how B reshapes A's trait and where B's hard cores are.
