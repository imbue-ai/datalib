# Shared sinks: two steps writing one store

**Status: proposal, put off 2026-09-24, not built.** This is what is left
of the supervisor plan. The rest of that plan is built, and the
[dag README](../../../datalib/backend/dag/README.md) describes it; git has
the whole plan at `479b6562d`, as `docs/dev/plans/supervisor.md`.

A **sink** is what a step writes and other steps read, such as a store.
Today each step writes the one tree its id names, so a sink has one writer
for life. This plan lets two steps name the same sink, and run one at a
time.

## The design

Today the graph is steps with edges between them, and a step's id names
the one tree it writes. The new graph is **bipartite**: steps and sinks.

```toml
[[steps]]
group = "work-email"
function = "ingest_gmail"          # a step id is still <group>/<function>
writes = "work-email/raw"          # default: the step's own id

[[steps]]
group = "work-email"
function = "ingest_mbox_import"
writes = "work-email/raw"          # a second writer of the same sink

[[steps]]
group = "work-email"
function = "render_markdown"
reads = ["work-email/raw"]         # `inputs` today; names a sink now
```

`writes` defaults to the step's id and `reads` is `inputs` renamed, so
every current config is a valid new one. What changes is what the
loader refuses: **two steps may name one sink.** The rule becomes
*one writer at a time*, and the supervisor is what enforces it — it
never starts a second writer on a sink while one runs, and the sink's
own writer lock (the table below) is the backstop if something slips
past.

Why want it: the `claude` provider's api and export methods share one
store today by being one step with two methods; an mbox import beside
a live Gmail pull is the same wish for email; a backfill step beside an
incremental one is the same wish for anything. The fan-ins are *not*
the case for it: `grid_index` already streams a pass per seal from any
render, so the only thing one shared index step serializes is the pass
itself, and a pass is small. Splitting it per source is possible under
this design and not worth doing first.

## What a sink has to be


Everything above is stated for doltlite because that is what the raw
and render stores are, but the supervisor never opens a store; it
schedules against three properties, and any storage that has them is a
sink. A new kind of sink is a new implementation of these, not a change
to the tick.

| property | what the supervisor needs | why |
|---|---|---|
| **a version** | a string that is a function of the sink's *committed* content, cheap to read without opening the sink for writing | `stale()` compares it on every tick; equal strings mean nothing moved |
| **atomic publish** | a reader sees only states a writer committed whole; a writer that dies leaves nothing a reader can reach | a consumer runs against whatever is published, at any moment, with no run boundary to hide behind |
| **one writer at a time** | the supervisor never starts a second writer while one runs, and the sink refuses one if something slips past | the same rule the etl README enforces per file today |

How the sinks we have meet them:

| sink | version | atomic publish | writer lock | on a crash |
|---|---|---|---|---|
| a doltlite store (raw, render) | `main`'s head — the last *published* seal, not the writer branch's | `commit_run` at a seal: commit on `datalib_writer`, fast-forward `main`; readers pin a commit on `main` | `flock` on the sibling `.lock` | the next `open` discards the working set, and publishes a commit the crash left unpublished |
| a plain SQLite file (qmd's `index.sqlite`, `runs.sqlite`) | a version the writer records *in the same transaction* as the data — a row in a `versions` table, or `PRAGMA user_version` | the transaction; readers open read-only | SQLite's own writer lock, plus the supervisor's scheduling | the transaction rolls back by itself |
| a file tree indexed by a store (the markdown documents) | the version of the store that indexes it: a `.md` file is reachable only through a committed `markdowns` row, so the tree has no version of its own | write the file, then commit the row that names it; a file no committed row names is unreachable | the indexing store's | orphan files are junk the next writer may remove; readers never saw them |
| a plain file tree (perseus's TEI XML, written by `curl -o`) | a content hash of the tree, computed by the supervisor **once, when its writer finishes** — the runner's hash today — and held until the next writer finishes | none of its own: a reader listing the tree mid-write sees a half-written file, so the supervisor starts no writer while a reader of the sink runs (or the step writes beside and renames) | the supervisor's scheduling alone | the next completed write re-hashes the whole tree, half-written files included, so nothing is adopted silently |
| a step that declares its own version | whatever its `outcome` line says, held by the supervisor until the next report | the step's promise, under the protocol's any-path rule | the supervisor's scheduling alone | the step's promise |

The last two rows are how every non-store sink works today and stay
available; the first three are what the supervisor can read for
itself. What matters is *when* a version is computed, not how: the
tree hash is a fine version for fourteen XML files, and a poor one for
a multi-gigabyte index — so a sink whose version is expensive to
compute is a sink that should record one instead (the qmd index, a
SQLite file, gets a `versions` row). Either way the supervisor
computes or reads a version **once per completed invocation** and the
tick compares cached strings; nothing is ever hashed inside the tick.

For a doltlite sink the version is `main`'s head, read by the
supervisor after **every** writer invocation ends — success, failure or
interrupt — because a writer's `open` can publish its crashed
predecessor's last commit, so a sink can move at the *start* of an
invocation that then fails (`datalib_history` already reads a store's
log without linking `etl`). The step's `outcome` line keeps working
unchanged; it is just no longer the *only* way a sink's version can
move.

## The hazard

- **Two writers of one sink and incrementality.** Each writer keeps its
  own cursor under its own tree; the *sink's* version moves when either
  commits. A reader is unaffected. But two writers that upsert the same
  keys with different opinions will fight, and doltlite will faithfully
  record the fight. The rule for a shared sink is the one the etl
  README already states for a raw store: the primary key is the
  upstream id, so two fetchers of one upstream agree by construction.
  Two *different* upstreams into one sink need disjoint keys, and the
  doc has to say so where `writes` is introduced.

## The work

`writes` and `reads` in the config with the defaults above, the loader
allowing two steps to name one sink, and the first provider that uses it:
the email import beside the live pull.
