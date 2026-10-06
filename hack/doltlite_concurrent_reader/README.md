# Can a doltlite reader consume a store a writer is still appending to?

The premise behind "our DAG runner is too conservative about
concurrency": if a consumer step can pin a consistent view of a store
while the producer step keeps committing, the runner no longer has to
run producers to completion before starting consumers.

`./run.sh` tests that against the real engine (the Bazel-built doltlite
CLI, one connection per simulated "step", matching the
`max_connections = 1` discipline the ETL pool enforces). It prints the
engine version it ran against.

## Results — doltlite 0.50.13 (same as on 0.50.3)

| # | scenario | result |
|---|---|---|
| A | writer leaves rows **uncommitted**, second process reads | plain `SELECT` sees **4** where HEAD has **2**; `dolt_at_t('HEAD')` sees **2** |
| B | naive reader `SELECT`s while writer commits | view **moved 5×**: `11 21 31 41 51` |
| C | reader pins with `dolt_at_t('<hash>')` | **stable** — one value, ten samples, across the writer's whole run |
| D | reader re-pins, consumes `dolt_diff_t(old,new)` | **20** rows to process vs **41** to re-read |
| E | writer health, and reader side effects | **0** busy/locked/errors; **no** branches left behind |
| F | pin durability | readable from a fresh process; survives `dolt_gc()` (25 chunks reclaimed) |

The *value* C settles on varies between runs — the reader pins to
whatever the writer had committed at the moment it started. That it
never moves afterwards is the assertion.

**The premise holds.** C is the design in one row. A and B are what you
get if you relax the scheduler's edges *without* changing the readers:
not a stale view but a **torn** one, mixing committed and uncommitted
rows. What `dolt_at_<table>` and `dolt_diff_<table>` do, and the other
ways to read one commit, are in
[`docs/dev/doltlite.md`](../../docs/dev/doltlite.md) § "Three ways to
read one commit" and § "Diffs".

## What the runner built on it

A consumer reads its producer at a pinned commit and holds no lock on
it, and may start on a producer still running when that producer
declares `streams_output`, reading each seal as it lands
([`datalib/backend/dag/README.md`](../../datalib/backend/dag/README.md)
§ "What keeps steps apart: locks"; the reader side is
`doltlite_raw::open_reader`, which opens the commit read-only and
detached — [`docs/dev/doltlite.md`](../../docs/dev/doltlite.md#three-ways-to-read-one-commit)).
