# The incremental contract: test the framework without the providers

**Status: proposal (2026-09-14), nothing built.** Prompted by the DAG
runner, which is tested with scripted steps and no real provider
(`dag/src/scheduler.rs`, `mod tests`): the runner's rules are checked
against stubs that speak the step protocol, so a provider can be wrong
without making the runner look wrong, and the other way round. This
asks for the same separation one layer down — for the render steps
first, and for the download steps as far as it goes.

## The property

Every mechanism in incremental render — the cursor, the `dolt_diff`
scan, `global_fanout_tables`, `remove_conversation`,
`retain_documents`, the fingerprint skip, the end-of-run sweep,
checkpoints — exists so that a run does *less* work than rendering
from scratch. It is correct exactly when doing less work does not
change the answer:

> **Incremental ≡ cold.** For any history of raw-store commits
> c₀ … cₙ, and any way of cutting it into runs, the render store after
> the last run is the same as one cold render of the raw store at cₙ.

"The same" means the logical content: `markdowns` without
`rendered_at`, `grid_rows`, `edges`, `render_problems` without its two
timestamps, and the bytes of every `.md` file. Not the doltlite file
and not its commit hashes, which chain off a wall-clock initial commit
(`tests/fixtures/README.md`, "byte-stable?").

Two corollaries the property implies and a test should assert
separately, because a violation of either can hide inside a run that
still converges:

- **Truthful at every commit.** Every doltlite commit of the render
  store (checkpoint, Ctrl-C, rescue, end of run) is the cold render of
  *some* subset of the raw store's buckets at the pinned commit —
  never a document with its rows deleted and not re-inserted, never a
  store emptied ahead of a refill. This is `one_mode.md`'s rule and
  what the per-batch transactions are for.
- **The consumer converges.** `grid_index` pinning any sequence of
  those commits ends equal to the render store, deletions included.

The property is the whole specification. Everything below is how to
check it without needing a real source, and then how to check that a
real source keeps its side of the bargain.

## Layer 1: the framework, with a synthetic provider

The runner's tests build a graph of three fake steps and drive it. The
render framework's equivalent is one fake provider and a driver loop.

**The synthetic raw store.** Three tables, chosen because they are the
three shapes every real provider's bucket query has to handle:

| table | shape it stands for |
|---|---|
| `parents(id, title, author_id)` | the bucket entity: a conversation, a PR, a page |
| `children(id, parent_id, body, seq)` | rows that project to a bucket through a foreign key |
| `authors(id, name)` | a `global_fanout_tables` entry: read by every document, owned by none |

**The reference renderer.** `cold(raw@c, params) → Set<Document>`: one
document per parent, its rows one per child ordered by `seq`, the
author's name resolved into every row, an edge from a child whose
`body` mentions another parent's id. Ten to twenty lines of pure
code, obviously correct by inspection, and never used in production.

**The incremental implementation.** The same renderer wired through
the framework the way a real provider is: `scan_buckets` with a bucket
query over `dolt_diff_parents ∪ dolt_diff_children`, `authors` as the
fan-out table, `buckets_without_rows` → `remove_conversation`, a
`RenderProcessor` that reads `ctx.raw_cursor` and reports
`ctx.consumed`. It lives in a test crate beside the driver
(`datalib_step`'s render module is the thing under test), not in
`providers/`.

**The model test.** A property test over generated histories:

1. Generate a sequence of mutations — insert/update/delete a row in a
   random table, including the fan-out one — and commit the raw store
   at random points, so a commit holds anywhere from one row to a
   whole wave of changes.
2. Cut the commit sequence into runs at random boundaries. Each run
   drives the real driver (`render::run`, or the part of it that sits
   under `spawn_blocking`) against the store as it stands at that
   commit, with the checkpoint cadence set low enough to fire several
   times per run.
3. After every run, dump the render store and assert ≡ `cold(raw@cᵢ)`.
4. Between runs, sometimes bump the synthetic renderer's version, or
   change its params, so the "render everything, sweep at the end" path
   runs — and assert the same equivalence, which is what proves the
   sweep removes exactly the re-keyed documents and nothing else.
5. Sometimes fail the run partway — the fake processor returns `Err`
   after emitting *k* documents — and assert the store still ≡ the
   cold render of the previous commit's buckets plus a prefix of this
   one's (truthful, not complete), and that the next run converges.
6. Run `build_grid_index` after a random subset of checkpoints and at
   the end; assert the index ≡ the render store at the end.

Crash between two SQL transactions is already pinned by
`doltlite_two_process_test`; this layer does not need a second
process, because the driver's own rollback on `Err` is what step 5
exercises.

What this layer would have caught, from this month alone: a version
bump losing `grid_index`'s range (audit 1.1), the index's cold path
deleting nothing (1.6 / "cold start deletes nothing"), the render
driver reading through the pool inside a transaction it held the
pool's only connection for (found on 2026-09-14 by a 13-minute hang
of the fixture build rather than by any test). None of those is
visible from a provider's tests, and none needs a provider to
reproduce.

**Cost.** One test crate, one synthetic provider, the dump/compare
helper below, and a generator. `proptest` is not a workspace
dependency; a hand-rolled seeded generator (`rand` with a printed
seed) is enough and keeps the crate list short.

## Layer 2: the provider contract

A real provider is correct under the property if it keeps five
promises. Written as a contract, so a provider author has a list and
a harness has something to check:

1. **Pure.** The documents it emits are a function of the raw store at
   the commit it pinned and of its `render_params`. Not of the clock
   (`ctx.now` is the run's pinned instant and is only ever *stamped*,
   never *read* to decide anything), not of the filesystem outside the
   raw store, not of iteration order.
2. **The changed set is complete.** For any two commits c₁, c₂, every
   bucket whose documents differ between `cold(c₁)` and `cold(c₂)` is
   either named by its `bucket_query` over `dolt_diff(c₁, c₂)` or made
   moot by a `global_fanout_tables` hit. This is the clause every
   mass-staleness bug lives in, because a miss is silent: the run
   succeeds and the document is simply old.
3. **Removals are named or the set is complete.** A diff-narrowed
   renderer calls `remove_conversation` for every named bucket whose
   entity is gone (`buckets_without_rows`); a whole-store renderer's
   `retain_documents` set is what it *considered*, skipped documents
   included, and it returns `Skipped` when it did not look.
4. **Byte-stable.** Rendering the same bucket from the same commit
   twice produces identical output, so the fingerprint is a real
   signal and a steady-state run writes nothing.
5. **`consumed` iff it read.** It reports the commit it pinned when it
   read the store and says nothing when it could not — the driver's
   sweep on a full render trusts this.

In return the framework guarantees the property, and the three things
a provider therefore never has to think about: which documents to
delete on a version or param change, what a checkpoint may publish,
and where the cursor lives.

**The harness.** Clauses 2 and 3 are the ones worth a machine check,
and one loop checks both: **single-row mutation.** For each provider,
against its TNG raw store:

```
base   = cold render of raw@HEAD                    (the fixture as built)
for each table T in the raw store, for each row r:
    scratch = copy of raw; apply one of {update a non-key column of r,
              delete r, insert a copy of r under a fresh key}; commit
    inc    = render incrementally from base's cursor against scratch
    assert inc ≡ cold(scratch)
```

A failure names the table, the row and the mutation — "changing
`users.display_name` did not re-render thread X" — which is the fix
in one line. It needs nothing per provider beyond `plan_render` and
the raw store the fixture already builds; `render_inputs.md` step 3
sketches the same loop and, once that lands, the harness compares
against declared inputs rather than bucket queries with no other
change.

**Cost.** One incremental render per row per mutation kind. The TNG
stores are small (hundreds of rows), so this is seconds per provider,
but it is a real cost on a shared crate change: the harness runs per
provider as a `rust_test` in that provider's package, so it rebuilds
with the provider rather than with the framework. Byte-stability
(clause 4) is one more render of the unmutated store, compared to
`base`. Clause 1 has no general test; the mutation loop finds the
common breach (reading the clock to decide a period) as a
non-determinism, and the rest is review.

**What the harness cannot see.** A bucket query that is complete for
the fixture's shape and incomplete for a shape the fixture lacks — a
table the fixture never populates. `schema_inventory` lists every
table; the harness should assert every table of the provider's store
has at least one row in the fixture, or is on a per-provider "known
empty" list, so a silent gap is at least a named gap.

## The dump/compare helper both layers need

One function: `render_store::logical_dump(path) → String`, sorted
rows of the four tables with the volatile columns dropped, plus the
`.md` tree as `(relative path, blake3)`. `fixture_db_snapshot_test`
does most of this for the index (`stable_row_set_hash`,
`stable_source_url`); lift it into `datalib_etl_render` as a test
utility so both layers and that snapshot share one notion of
"the same".

## Downloads

The same property, with the roles moved one step upstream:

> **`fresh(U₂) ≡ incremental(fresh(U₁) → U₂)`**: syncing a store that
> was fetched against upstream state U₁ into state U₂ yields the raw
> store a fresh fetch of U₂ would — modulo bookkeeping (`fetched_at`,
> attempt counts, `sync_runs`) and the volatile fields each provider
> already declares.

Whether it can be checked depends on whether U₁ → U₂ can be
*expressed*:

- **Mirrors and scans** (lightroom, apple_photos, whatsapp; pdf,
  fsindex, media): yes, today. The upstream is a SQLite file or a
  directory; the harness edits it (update a row, delete a file, add
  one) and compares. The engine-level test belongs in
  `sqlite_mirror` and `fswalk`, once, with the providers inheriting it.
- **Export-shaped sources** (claude export, google_takeout, linkedin,
  contacts from `.vcf`, sms_backup_restore, signal from a backup):
  yes, with the same edits to the export tree the fixture is built
  from.
- **API sources** (claude, chatgpt, slack, github, gitlab, notion,
  email over JMAP/Gmail, beeper, yolink): only once their synthesizers
  (`providers/*/src/synthesize.rs`) can produce a *second* upstream
  state — a listing that drops a conversation, a message whose text
  changed, a new page — rather than one static fixture tree. That is
  real work per provider, and it is the work that would also give the
  `--reset-and-redownload` golden a second point to compare against.

And the guarantee is weaker by design. `one_mode.md` rule 2 says a
scope the run did not fully enumerate is not pruned — slack's recent
channels, beeper's evicting `index.db` — so for those the equivalence
holds only modulo the scopes the provider declares it does not
re-list. The contract has to name them, and the harness checks
equivalence outside them and *non-deletion* inside them. That is a
more honest statement of what incremental download promises than any
prose we have now.

## Order

1. The dump/compare helper, lifted from `fixture_db_snapshot_test`.
2. Layer 1 — the synthetic provider and the model test. Worth doing
   before the ingest half of `one_mode.md` (items 2 and 4), because
   the driver is about to be relied on harder and this is the test
   that sees it.
3. Layer 2 — the mutation harness, run against every ported render
   provider. Expect it to find something: the fan-out gap this month's
   work documented (a `users` change renders everything and probes
   no removals) is a clause-3 breach the harness names on its first
   delete of a `users` row.
4. Downloads, mirrors and scans first, then export-shaped, then the
   synthesizers.

## Relation to other documents

- [`one_mode.md`](one_mode.md) is the rule the property is stated
  against; layer 1 is that rule's test.
- [`render_inputs.md`](render_inputs.md) step 3 is layer 2's loop with
  declared inputs in place of bucket queries; it should land as the
  same harness with one comparison swapped.
- [`streaming_steps_plan.md`](streaming_steps_plan.md) §"The sink
  contract" P1/P2 are the "truthful at every commit" corollary, stated
  for consumers.
- [`data_architecture_parse_and_render.md`](../data_architecture_parse_and_render.md)
  §5 describes the mechanisms; this describes what they must add up to.
