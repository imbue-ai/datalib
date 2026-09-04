# datalib as a library

Two proposals, written 2026-09-03, about turning datalib from an
application that mirrors personal data into something other people —
and other people's agents — can build on. **Read them in order; the
second depends on the first.**

1. [**`data_handling_practices.md`**](data_handling_practices.md) —
   what we change about our *own* data architecture and handling:
   the practices we adopt, how we audit the twenty providers we
   already shipped, how we retrofit them, and what a new provider has
   to do from now on.
2. [**`toolchain_for_agents.md`**](toolchain_for_agents.md) — what we
   then ship: three surfaces an agent could use, why a static binary
   and a JSON protocol rather than a crate, and how to check the result
   against a corpus we didn't design without overfitting to it.

Plus one measurement, downstream of the first:

3. [**`render_audit_2026_09_03.md`**](render_audit_2026_09_03.md) — doc
   1's audit actually run against the tree. Unlike the other two it is
   mostly *descriptive*: what the render code does today, measured, with
   a file and line per claim. Its proposals are the design work the
   findings forced, chiefly the R1 problem sink as a per-source doltlite
   table. Start here if you want evidence rather than intent.

The ordering is the argument, not a formality. An agent that adopts a
primitive inherits its defects and cannot see them, so a surface is
ready to offer only once the code behind it has a problem sink, a
published lossiness table, and a check against source. The one
exception is the DAG runner, which schedules processes and carries none
of our render behaviour — that can ship first.

**Both are proposals. Nothing in either is built.** Treat every "we
should" as unbuilt and every "we do" as a claim to verify against the
tree, per [`AGENTS.md`](/AGENTS.md) §"Prose can be stale."

## Where this came from

Both docs were prompted by
[`imbue-ai/default-workspace-template#534`](https://github.com/imbue-ai/default-workspace-template/pull/534),
which adds a `data-pipeline-builder` skill: a recipe that leads an
agent to build, in ten minutes and ~600 lines of stdlib Python, an
ingestion tool with incremental loading, backfill, a problem log and
bounded retention.

Read next to [`data_architecture_ingestion.md`](../data_architecture_ingestion.md),
the **storage core converges** — upstream identity as the primary key,
one complete upsert shape, one transaction per batch, a raw layer
everything else is derived from. The **data-quality surface does not**,
and there the skill is ahead of us on seven counts, all of them about
the record we cannot store. Doc 1 §1 has the scorecard.
