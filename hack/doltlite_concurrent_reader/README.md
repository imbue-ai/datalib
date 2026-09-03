# Can a doltlite reader consume a store a writer is still appending to?

The premise behind "our DAG runner is too conservative about
concurrency": if a consumer step can pin a consistent view of a store
while the producer step keeps committing, the runner no longer has to
run producers to completion before starting consumers.

`./run.sh` tests that against the real engine (the Bazel-built doltlite
CLI, one connection per simulated "step", matching the
`max_connections = 1` discipline the ETL pool enforces).

## Results — doltlite 0.50.3

| # | scenario | result |
|---|---|---|
| A | writer leaves rows **uncommitted**, second process reads | plain `SELECT` sees **4** where HEAD has **2**; `dolt_at_t('HEAD')` sees **2** |
| B | naive reader `SELECT`s while writer commits | view **moved 5×**: `11 21 31 41 51` |
| C | reader pins with `dolt_at_t('<hash>')` | **stable** — one value, ten samples, across the writer's whole run |
| D | reader re-pins, consumes `dolt_diff_t(old,new)` | **20** rows to process vs **41** to re-read |
| E | writer health, and reader side effects | **0** busy/locked/errors; **no** branches left behind |
| F | pin durability | readable from a fresh process; survives `dolt_gc()` |

The *value* C settles on varies between runs — the reader pins to
whatever the writer had committed at the moment it started. That it
never moves afterwards is the assertion.

**The premise holds, and the primitives are clean.** C is the design in
one row. A and B are what you get if you relax the scheduler's edges
*without* changing the readers: not a stale view but a **torn** one,
mixing committed and uncommitted rows.

## The two primitives

**`dolt_at_<table>('<commit-ish>')`** — doltlite's `AS OF`. A
table-valued function accepting `HEAD`, `HEAD~N`, or a raw commit hash,
with the table's own schema. Critically it is a **pure read**: no
branch, no `dolt_checkout`, no write to the file, nothing left behind.
And it reads *committed* state only — scenario A is the proof, where it
returns 2 while a plain `SELECT` returns 4 against the same file.

**`dolt_diff_<table>('<from>','<to>')`** — the arbitrary two-commit row
diff, as a table-valued function. Columns are
`to_*`, `from_*`, `diff_type`.

> Do not confuse these with the **unparameterized** vtabs of the same
> name. Bare `dolt_diff_<t>` is the per-commit change log, and filtering
> it on `from_commit`/`to_commit` only ever matches **adjacent
> parent→child pairs** — an arbitrary range silently returns 0 rows,
> which reads as "no changes" rather than as an error. The
> parameterized call is the one you want. (Bare `dolt_diff`, with no
> `_<table>` suffix, is a third thing again: a list of commits.)

MySQL-style `SELECT … FROM t AS OF '…'` is a **parse error** — SQLite's
grammar has no such clause. The capability is there; only the spelling
differs.

**A pin is just a hash.** Scenario F: a brand-new process with no
inherited connection reads an old pin fine, and it still reads correctly
after `dolt_gc()` reclaims 25 chunks — as does a diff spanning the gc
boundary. Nothing has to be held open to keep a pin alive, so a slow
consumer cannot have its view collected out from under it.

## What this leaves for the design

Pinning costs a reader nothing and disturbs the writer not at all, so
the reader side is essentially free. What remains is not a storage
question but a scheduling one: how a consumer *learns* there is a new
commit worth re-pinning to, and how it keeps durable offset state
between chunks.

## Environment

macOS, doltlite as pinned in `MODULE.bazel` — 0.50.3 at time of
writing, via `//third-party/doltlite:doltlite`. `run.sh` prints the
engine version it actually ran against and builds the CLI if missing.
