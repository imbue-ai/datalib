# Post-ingestion architecture

This document describes the principles we are striving towards for
everything **after the extract stage** — translate, load, indexing,
annotation, and presentation. It is the companion to
[`docs/data_architecture_ingestion.md`](data_architecture_ingestion.md),
which covers how raw data lands on disk; this one covers what the
system does with raw data once it has it, and how users (and their
tools) are meant to participate.

Like the ingestion doc, it is aspirational as much as descriptive: not
every stage honors every principle today, and several sections
describe contracts we intend to publish rather than contracts that
exist. Divergences should be either justified or fixed. The audit
measuring the code against these principles is
[`post_ingestion_audit.md`](post_ingestion_audit.md); the work plan
derived from it is
[`post_ingestion_plan.md`](post_ingestion_plan.md). Where a
question is genuinely undecided, it's listed under
[Unresolved questions](#unresolved-questions) with the constraints we
know about, rather than prematurely specified.

## Where things stand today

The current post-extract pipeline, for orientation (the ingestion doc
has the full table):

  - **Translate** (per-provider Rust,
    `frankweiler_etl_<provider>::translate`) reads the raw store and
    emits a **sidecar tree** under `rendered_md/<provider>/...`: one
    human-readable `<id>.md` per document, plus a machine-readable
    `<id>.grid_rows.json`
    ([`Sidecar`](../frankweiler/backend/index_lib/src/lib.rs)) carrying
    a header (`markdown_uuid`, `source_fingerprint`, `render_version`)
    and an array of [`GridRow`](../schemas/grid_rows.schema.json)s,
    optionally with edges.
  - **Load** (provider-agnostic,
    [`src/load.rs`](../frankweiler/backend/etl/src/load.rs)) walks the
    sidecar tree and applies it to `<data_root>/backend_index.doltlite_db`:
    the `grid_rows` union table, the
    [`markdowns`](../schemas/markdowns.schema.json) registry of
    rendered documents, the [`edges`](../schemas/edges.schema.json)
    link table (see [`docs/edges.md`](edges.md)), and the
    `markdowns_loaded` fingerprint bookkeeping.
  - **Presentation**: the UI is a stack of miller columns where every
    column is a **card** — a JS expression the user can read and edit
    (see [`docs/cards.md`](cards.md)). The built-in views query the
    backend, which issues single SELECTs against `grid_rows` and
    serves rendered markdown bodies by `markdown_uuid`.

Everything below is about which parts of that picture are load-bearing
contract, and which parts are merely the reference implementation.

## The stance: small kernel, open world

The single organizing principle of the post-extract world:

> **Be maximally opinionated about a tiny set of universals, and
> maximally unopinionated about everything else.**

The kernel is not a neutral substrate. It is a small set of strongly
enforced meanings — identity, time, links — and its strictness there
is exactly what buys agnosticism everywhere else. "No fabricated
timestamps" and "deterministic UUIDs, never autoincrement" are
policies, not mechanisms, and they are non-negotiable *because* they
are what make data from unrelated tools composable. What the kernel
refuses to have an opinion on is domain meaning: what schemas exist,
what they denote, how they're rendered, and which tools consume them.

The universals are a **protocol, not a platform**: anything that
speaks them participates fully, and nothing is required to speak
them. We cannot anticipate every AI tool a user will want to point at
their data — so the seams are designed such that we don't have to.

The companion principle, stated economically: **provide the minimal
amount of plumbing, and make that plumbing competitive with not
using it.** "Take the ingested data and run" — pointing your own
tools, agents, or a vibe-coded stack directly at the files on disk
and ignoring everything we built downstream of extract — is always a
valid option, and we treat it as the baseline to beat. Every piece
of plumbing we do provide (load, the index, the edges table, the
built-in UI) must earn its place by being genuinely better than that
baseline for some real task — never by being mandatory, and never by
being the only thing that can read the data. When a proposed feature
can't articulate what it beats the baseline *at*, it's plumbing we
shouldn't build.

A consequence we should own explicitly: this **demotes `grid_rows`
from "the output of the pipeline" to "one well-known profile among
many."** Today `grid_rows` is the most opinionated artifact in the
system — a fixed, codegen'd union schema with a curated `kind`
taxonomy. Under this stance it remains the richest conforming schema
and what the built-in chronological grid reads, but it is no longer
the only legitimate shape for translate output. Arbitrary other
tables are fully legal and get the database-explorer treatment (see
[Pay-as-you-go conformance](#pay-as-you-go-conformance)). The
ingestion doc's "shared canonical schema" families correspondingly
reframe from *requirement* to *convention you opt into for free
functionality*.

## The universals

The short list of things every participant agrees on. Everything not
in this section is explicitly **not** universal.

  - **Object identity.** Deterministic, Ship-of-Theseus UUIDs —
    upstream-provided where they exist, UUIDv5-synthesized where they
    don't. Never autoincrement, never content hashes. Inherited
    wholesale from the ingestion doc's
    [Object identity](data_architecture_ingestion.md#object-identity-ship-of-theseus-on-uuids)
    section.
  - **Time.** Event-shaped rows carry strict ISO-8601 timestamps with
    explicit offset; entities without a time-shape carry honest
    nulls, never fabricated placeholders. Inherited from
    [Time and ordering discipline](data_architecture_ingestion.md#time-and-ordering-discipline).
  - **Things and links.** Anything can link to anything — see
    [The thing model](#the-thing-model) below. This is the universal
    this document adds.
  - **Searchable text** (candidate). Some convention by which a tool
    declares "this is the text a human would search for." Today this
    is the `text` column on `grid_rows` and the rendered markdown
    body; how it generalizes to arbitrary schemas is
    [unresolved](#how-universals-are-declared).

The mechanism by which a table declares its universals — magic column
names, a per-table manifest, sqlite introspection plus convention —
is itself [unresolved](#how-universals-are-declared).

## The thing model

A **thing** is one of:

  - a **database row** (in `grid_rows` or in any other table),
  - a **document** (a rendered markdown file, identified by
    `markdown_uuid`),
  - a **span** within a document (identified by an anchor UUID, baked
    into the rendered body as `data-section-uuid`),
  - a **binary blob** (identified by its blake3 hash in the CAS).

Any thing may link to any thing, with a label. That is the entire
ontology, and the system commits to nothing richer. Links are the
fiber of the net: the built-in UI's distinguishing capability is that
it can follow them.

What exists today:

  - **Row → document**: a `GridRow` knows the `markdown_uuid` of the
    document it was shredded from; `markdowns.md_path` resolves the
    uuid to a file.
  - **Document/span → document/span**: the
    [`edges`](../schemas/edges.schema.json) table —
    `(src_markdown_uuid, src_anchor_uuid?, dst_markdown_uuid,
    dst_anchor_uuid?, label?)`, with a UUIDv5 PK over the canonical
    tuple so re-ingest is idempotent. See [`docs/edges.md`](edges.md).
  - **Row → blob**: `blob_refs` in each raw store, keyed by blake3.

What's missing, and aspirational:

  - **One reference format that can address all four kinds of
    thing.** Today edges hardcode `markdown_uuid` columns, so only
    documents and spans are linkable; a row in an arbitrary-schema
    table has no addressable identity at all. The generalized edge is
    `(src_ref, dst_ref, label, provenance)` where a *ref* is a single
    serialized form covering rows, documents, spans, and blobs. The
    format is [unresolved](#the-thing-reference-format); the
    requirement is not.
  - **Provenance on every edge** — whether a link was derived from
    raw data (disposable, rebuilt on rebake) or authored by a user or
    tool (sacred, never touched by a rebake). See
    [Derived is disposable, authored is sacred](#derived-is-disposable-authored-is-sacred).

## Pay-as-you-go conformance

Conformance to the universals is graded, not binary, and each
universal a tool adopts buys a specific capability:

| You provide                  | You get                                  |
|------------------------------|------------------------------------------|
| any table at all             | browsable in the grid / database explorer |
| stable, deterministic UUIDs  | addressable; linkable; idempotent re-runs |
| `when_ts`-disciplined stamps | rows appear in time-ordered union views   |
| declared searchable text     | full-text search finds your rows          |
| emitted edges                | graph navigation to and from your things  |

A tool that adopts none of them still produces legal, usable data — a
doltlite db the explorer can browse. Nothing is gated on full
conformance. (This is the web's graceful-degradation move: a page
with no metadata still renders; every bit of structure you add
unlocks something.)

## Translate is an open interface

We are unopinionated about how a translate step is **implemented**.
"Calling an existing external tool" or "a program a user vibe-coded
this afternoon" should be as first-class as the Rust translate steps
in this repo. The reasoning: we cannot possibly anticipate every AI
tool a user wants to apply to their data, and "modify the Rust
source of the ETL pipeline" is both limiting (you must use Rust) and
risky (a generated patch can break unrelated parts of the codebase).
The unit of extension is a *separate program speaking a documented
contract*, not a patch to ours.

The contract, not the implementation, is what's specified:

  - **Input**: the raw store (`<data_root>/raw/<name>.doltlite_db` and
    its sibling blob CAS), **read-only**. The single-writer rule from
    the ingestion doc applies: translate never writes to raw.
  - **Output**, at either of two conformance levels:
      1. **A conforming sidecar tree** — `.md` + `.grid_rows.json`
         files matching the
         [`Sidecar`](../frankweiler/backend/index_lib/src/lib.rs)
         contract. This is *full participation*, and it works
         **today** with zero new machinery: Load is
         provider-agnostic and never knows or cares whether Rust or
         a Python script wrote the files.
      2. **Its own doltlite database** under the data root, with any
         schema whatsoever. This is *explorer participation*, plus
         whatever universals the schema chooses to speak.
  - **Incrementality is offered, not demanded.** The
    `source_fingerprint` skip machinery is available to tools that
    want cheap re-runs; "rebuild everything every run" is an
    acceptable conformance level for a tool whose data is small.

What this implies and we have not yet built: a way to register an
external translate step in `config.yaml`, an invocation contract
(args, exit codes, progress reporting — the existing
NDJSON-on-stderr obs contract is the obvious thing to reuse), and a
written, Rust-free specification of the sidecar format. See
[unresolved](#registering-and-invoking-external-translate-steps).

### Reference implementations, not privileged residents

The Rust providers in this repo implement the same public contract an
external tool would. "First-class" is testable: external tools must
be **equally documented** (if the only way to learn the sidecar
format is reading Rust source, the principle is violated), **equally
invocable** (from config, not from a hand-run shell), and **equally
observable** (same progress and summary surfaces). Any capability
that only the in-repo providers can access is a bug against this
section.

### Determinism is what makes openness safe

Deterministic UUIDs and content fingerprints mean re-running any
translate step — including a buggy, half-finished, vibe-coded one —
**converges** instead of multiplying garbage. Idempotency is the
property that lets us hand the pipeline to code we don't fully
trust: the worst a bad run can do is produce wrong derived data,
which is disposable by the next principle.

## Derived is disposable, authored is sacred

There are exactly two kinds of data downstream of extract:

  - **Derived** — anything computable from the raw store: sidecars,
    `grid_rows`, rendered markdown, derived edges (e.g. Perseus
    alignment links), search indexes. All of it can be deleted and
    rebaked at will; `RENDER_VERSION` bumps and `--reset-and-redownload`
    style workflows assume this freely.
  - **Authored** — annotations, user-created edges, notes, tags:
    anything a user or an AI tool *added* that is not a function of
    raw. Authored data is a **source of truth on par with `raw/`**.
    A rebake must never touch it.

This is the first place the ingestion doc's "raw is the source of
truth; downstream is rebakeable" invariant genuinely breaks, and it
needs to break deliberately rather than by accident. Every store and
every edge must know which kind it is — hence the `provenance` field
on the generalized edge. Where the authored store physically lives,
and how it participates in backup/portability, is
[unresolved](#the-authored-data-store).

## The files are the API

Doltlite databases, markdown files, and blobs on disk are the real
interface of this system. The Rust backend and the built-in UI are
conveniences layered on top — never gatekeepers.

The falsifiable form of the rule: **every capability of the built-in
backend and UI must be achievable by reading the files on disk. The
backend holds no private state and speaks no private format.** It is
entirely legitimate to bypass our UI — and our backend — and
vibe-code an alternative frontend whose backend queries the doltlite
files directly. (This is the same spirit as the ingestion doc's
JSONL wire tape: formats over services.)

Corollary: the built-in UI's advantage must be **earned by features,
never by lock-in**. Its legitimate edge is that it understands the
universals — it knows things link to other things and can navigate
the graph; it knows `when_ts` and can interleave providers in time.
The day its edge depends on a format nobody else can read, this
section has failed.

## Presentation

The same opinionated-kernel / open-world split applies to the UI:

  - **The grid is a database explorer**, not a renderer of one
    blessed schema. It should accommodate any table in any registered
    doltlite db, using whatever universals the table speaks to
    enhance the view (timestamps → sortable timelines, uuids →
    followable links), and degrading gracefully to plain rows when
    it speaks none.
  - **Views are user-editable code.** Every miller column is a card
    whose source is a JS expression the user can read and edit in
    place ([`docs/cards.md`](cards.md),
    [`cardSource.ts`](../frankweiler/ui/src/cards/cardSource.ts)).
    Allowing arbitrary JS here is deliberate: presentation is domain
    meaning, and domain meaning lives in user space.
  - **Bring-your-own-UI is fully supported** — see
    [The files are the API](#the-files-are-the-api).

## Open to agent participation

Users will work on this system *through* coding agents — asking an
agent to build a custom translate step, to tweak a card, to analyze
the data sideways. The system should make itself open to that
participation, and be **unopinionated about the user's agent
environment**: Claude Code, another CLI agent, an IDE-embedded one,
something that doesn't exist yet. We don't ship an agent; we make
the system legible to whichever one the user already has.

Two concrete obligations follow:

  - **Provide skills for building extensions.** A user should be
    able to point any agent at "write me a translate step that does
    X" and have the agent succeed without reading our Rust source.
    That means agent-consumable skills/instructions covering the
    translate contract, the sidecar format, the conformance levels,
    and how to register the result — the Rust-free spec called for
    in [Translate is an open interface](#translate-is-an-open-interface)
    is a precondition. Documentation only a human browsing the repo
    would find is half the job.
  - **Admitted code is agent-inspectable and agent-modifiable.**
    Custom source code the system already admits — card sources in
    the columns, registered external translate steps — must be
    exposed in a form an external agent can read and edit: plain
    text, reachable from the filesystem, not opaque UI state. A card
    source editable in the column header serves the human at the
    keyboard; the agent needs a path to the same source. This is
    [The files are the API](#the-files-are-the-api) extended to
    code: user-authored code is user data too.

Where card sources should live on disk so agents can reach them is
[unresolved](#skills-and-the-agent-surface).

## Operational principles, inherited

The ingestion doc's operational principles —
[monitorable](data_architecture_ingestion.md#monitorable),
[stoppable and resumable](data_architecture_ingestion.md#stoppable-and-resumable),
[efficiently incremental](data_architecture_ingestion.md#efficiently-incremental)
— apply to every downstream stage with one honest caveat: for
external translate steps we cannot *enforce* them, only make them
easy. The harness offers the obs contract, the fingerprint-skip
machinery, and idempotent application; a good citizen uses them. The
in-repo stages have no such excuse: translate-side progress
reporting currently trails extract-side, which the ingestion doc
already flags.

## Trust model

Arbitrary JS in cards and arbitrary programs in translate are
arbitrary code execution over private data. The current trust model
is stated plainly: **user code runs with user privileges over user
data on the user's machine — the same trust as the shell.** Code an
agent writes on the user's behalf falls under the same model: it is
the user's code, reviewed or not, exactly as if they had typed it.
This is consistent with the single-user, single-laptop,
data-stays-local assumptions of the ingestion doc.

The moment card sources or translate tools are *shared* — a card
arriving in a URL, a translate step installed from someone's gist —
the model changes from "code I wrote" to "code someone sent me," and
this section must be revisited. Flagged as
[unresolved](#sharing-user-code) rather than solved now.

## Unresolved questions

### The thing-reference format

One serialized form that can address a row in an arbitrary table
(which db? which table? which pk?), a document (`markdown_uuid`), a
span (`markdown_uuid` + anchor), and a blob (blake3). Constraints:
deterministic, stable across rebakes (so it must build on the
identity universal, not on rowids or paths), cheap to index, and
writable by non-Rust tools. The generalized `edges` schema
`(src_ref, dst_ref, label, provenance)` depends on this. Nothing is
decided beyond the constraints.

### How universals are declared

Magic column names (`uuid`, `when_ts`, `text`) are the cheapest and
most vibe-code-friendly; a small per-table manifest is more honest
and handles tables that can't rename their columns. Related: how does
full-text search discover what to index in an arbitrary table?
Undecided.

### Registering and invoking external translate steps

What a `translate:` block in `config.yaml` looks like for an external
command; the invocation contract (args, env, exit codes); whether
progress reuses the NDJSON-on-stderr obs contract (probably);
where a Rust-free written spec of the sidecar format lives. Also:
the single-writer rule vs. concurrent external tools reading raw
while sync runs — what do we promise?

### The fate of the `kind` taxonomy and the schema families

If `grid_rows` is a profile rather than the output, does its curated
`kind` taxonomy stay curated or become open-vocabulary? The ingestion
doc's
[shared-schema families](data_architecture_ingestion.md#shared-schemas-across-similar-sources)
section should be reframed from requirement to convention once this
doc's stance lands.

### The authored-data store

Where annotations and authored edges physically live (a dedicated
doltlite db under the data root, presumably), how a rebake knows to
leave it alone, and how it participates in the
backup-and-portability principle (`cp -r <data_root>` must carry it).

### Skills and the agent surface

What form the extension-building skills take (a `SKILL.md` per
extension point? a `skills/` tree the user points their agent at?),
how they're versioned against the contracts they describe, and —
the sharper half — **where admitted custom code lives on disk**.
Card sources today are encoded in the column header / URL state,
which serves interactive editing but gives an external agent
nothing to open or modify. Agent-editability suggests cards (and
any registered translate steps) should live as plain files under
the data root or repo, with the UI reading from there. Undecided.

### Sharing user code

Card sources travel in URLs today
(see [`docs/cards.md`](cards.md)); sharing a URL is sharing code
execution. Fine under the current single-user trust model; needs a
real answer before any sharing or sync feature ships.

## What this document does not cover

  - The extract stage and the raw store — see
    [`docs/data_architecture_ingestion.md`](data_architecture_ingestion.md).
  - The concrete `grid_rows` / `edges` / `markdowns` schemas — see
    [`docs/grid_rows.md`](grid_rows.md), [`docs/edges.md`](edges.md),
    and [`schemas/`](../schemas/).
  - The card/view machinery in detail — see [`docs/cards.md`](cards.md).
  - qmd index internals.
  - Hosting, multi-user, replication — explicitly out of scope; this
    is a single-user, single-laptop system.
