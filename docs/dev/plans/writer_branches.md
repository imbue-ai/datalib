# One branch per writer, merged into main when it seals

**Status: built.** Every writer works on its own branch and
fast-forwards `main` when it seals. This began as the design half of
[#647](https://github.com/imbue-ai/datalib/issues/647) (the
investigation half, which says "no production code changes"); the
measurements came out well enough to build it in the same pass, and
what is left open is at the bottom.

Every number and behaviour here was measured against doltlite 0.50.3 on
2026-09-22, in `etl/src/pin.rs`'s test module. Nothing below is
inferred from doltlite's docs or from this repo's prose about them.

## The problem this solves

Today every process opens the same store on `main`. Doltlite's working
set belongs to a branch and lives in the file, so two connections on
`main` share one working set the way two people editing one git checkout
share a working tree. Everything in `etl/README.md` § "Connection pools"
follows from that:

- **one writer per file**, enforced by a lock file, because a second
  writer's `dolt_commit('-Am', …)` would capture the first's in-flight
  rows;
- **a reader opens read-only and pins a commit**, because an ordinary
  `SELECT` reads the shared working set;
- **the `pinned_<table>` views**, because pinning has to be applied to
  every query without threading a commit through every call site.

Those are workarounds for a shared working set. A writer on its own
branch does not share one.

## What is already true, measured

**A reader on `main` cannot see a writer's work on a branch — including
the catalog.** A writer that checks out `writer`, creates a table and
inserts rows is invisible to a read-only connection on `main`: the table
is absent from that connection's `sqlite_master`, not merely empty.
It stays invisible after the writer *commits* on its branch. It appears
only when the writer publishes to `main`.

That last part is the load-bearing one, because the catalog is where
today's bugs live. `install_views` builds its view set from
`sqlite_master`, which is the working set's table list rather than the
pinned commit's, so a reader pinned to a commit asks an unpinned
question about which tables exist. Both races this repo has hit are that
one sentence:

- a consumer opening a producer's store after the file exists but before
  its first `CREATE TABLE` — fixed in this branch by making a table-less
  file unreadable, and the failure it caused is below;
- a consumer opening during a table's drop-and-recreate — still open,
  narrower, needs a schema change to reach.

Under branches neither is reachable: `main`'s catalog only ever changes
when a seal publishes, and that is one atomic step.

**Branches were already in the tree, and both `dolt_merge` and
`dolt_branch -f` work.** `fsindex` takes one branch per scan root
(`providers/fsindex/src/ingest/db.rs`), and `dirtree_diff/README.md`
records the syntax trap — doltlite exposes dolt's procedures as
*functions*, `SELECT dolt_checkout(…)`, and rejects `CALL DOLT_CHECKOUT`.

## What branches do *not* solve, measured

**A reader on `main` is not a stable snapshot.** A long-lived read-only
connection on `main` sees a publish land under it: a `COUNT(*)` on one
connection went from 1 to 2 with no reopen, at the moment another
connection moved `main`. Branches isolate *unpublished* work; they do
not freeze `main`.

**A read-only connection cannot pin itself by checking out a commit.**
`SELECT dolt_checkout('<hash>')` fails with

> dolt does not support a detached head state. To create a branch at
> this commit instead, run `SELECT dolt_checkout(<start_point>, '-b',
> <new_branch_name>)`

and the suggested alternative is a *write*, which a reader's connection
is opened `read_only` at the engine specifically to prevent. A branch
per reader would also need cleanup, and a crashed reader would leave one
behind.

So `dolt_at_<table>('<hash>')` remains the only snapshot primitive a
read-only connection has. **`pin.rs` does not go away.**

This matters because a consumer's pass makes two queries that must name
one commit: `changed_since` diffs `from_ref` → `to_ref`, and
`documents_matching` then reads the rows behind that answer. Under
streaming the producer publishes on every checkpoint, so an unpinned
consumer would diff at one commit and read at another — a tearing bug
quieter than either race above.

## What was built

`WRITER_BRANCH` is `datalib_writer`, one per file.

- **The branch is selected in `connect_pool`'s `after_connect`**, not
  once at open. sqlx replaces a connection that breaks, and a
  replacement starting on the default branch would put a writer's
  half-finished work where every reader can see it.
- **`dolt_connect_branch`, not `dolt_checkout`.** Both leave the session
  on the branch, but only one of them is free — see the cost table.
  `dolt_checkout('-b', …)` still creates the branch, once per file.
- **Sealing is `commit_run`**: `dolt_commit` on the branch, then
  `dolt_branch('-f', 'main', …)`. A force-move, not `dolt_merge` — one
  writer per file means `main` only ever moves here, so the branch is
  always a descendant and a merge would be a fast-forward anyway. It
  also keeps `main`'s history linear, which is what `dolt_diff_<table>`
  between two of its commits rests on. Two writers on one store would
  need the real merge.
- **A crash between the commit and the publication** leaves the branch
  ahead of `main`. That commit is a seal the last run meant to make, so
  `open` finishes it.
- **The name is the signal.** `publish_to_main` fires only for a
  connection on `WRITER_BRANCH`, so `fsindex` — one branch per scan
  root, publishing none of them — is untouched. `core/app_store.rs` is
  a plain pool and never enters the scheme at all.
- **`publish_to_main` is public** for the one shape `commit_run` cannot
  express: a commit that needs an argument of its own, such as the
  `--date` the yolink fixture generator pins. That caller commits by
  hand and then publishes.

Readers are unchanged: read-only, pinned, `pinned_<table>`.

### What this cost, measured

| | |
|---|---|
| 20 seals × 500 rows, plain | 29563 ms, 55,386,416 bytes |
| the same on a branch | 27196 ms, 55,851,157 bytes |
| a read-only connect | +0 bytes |
| `dolt_connect_branch` onto the branch | **+0 bytes** |
| `dolt_checkout` onto the same branch | +507 bytes |
| `dolt_checkout` back to `main` | +498 bytes |

Time is a wash, and so is space — but only because of which function
selects the branch. `dolt_checkout` persists a working set for the
branch it leaves and the one it enters, which put ~500 bytes into an
untouched store on **every** open; 20 opens of a settled store grew it
by 10,140 bytes, all of it reclaimable by `dolt_gc` and none of it
visible to `dolt_log`, `dolt_status` or any step's counts.
`dolt_connect_branch` writes nothing — in the source it loads the
branch's working set and sets the session's branch and head, and
serializes no refs and commits nothing.

So `reopening_an_untouched_store_does_not_grow_it` still asserts **zero
bytes**, as it did before this change. It was briefly relaxed to a
ceiling plus a gc check while `dolt_checkout` was the mechanism; that
was the one guarantee this change traded away, and it did not have to.

### Which branch a fresh connection starts on

The rule is **the file's stored default branch**, not `main`
unconditionally. `zDefaultBranch` lives in the persisted refs block;
seeding sets it to the branch it created, `csEnsureDefaultBranch` falls
back to `main` only for a file carrying none, and the SQL function
`dolt_default_branch(x)` moves it. Nothing in this repo calls that, so
ours stay `main` — but the safety of every reader rests on it, because
a default pointing at `WRITER_BRANCH` would put every reader on a
writer's uncommitted working set. `a_fresh_connection_starts_on_main`
asserts the default itself for that reason, not just the branch a
connection happened to land on.

### What it bought

`what_a_reader_sees_while_a_writer_deletes_and_reloads` is the
measurement. A writer on `main` that had `COMMIT`ed at the SQL level but
not yet `dolt_commit`ed showed its whole uncommitted batch to any reader
in any process — most of why readers pin at all. That phase now shows a
reader nothing, across two real processes.

Every test that broke was reading state no reader in production could
see, and passed only because writer and reader shared a working set.
None of it was arbitrary test debt.

### Still to delete

Nothing here has been removed yet, and each wants its own change:

| could go | why |
|---|---|
| `carries_committed_schema` | its job is spotting a half-built store; `main` never carries one now, because the schema apply lands atomically at the seal |
| the `WHERE 0` branch in `install_views` | a table in `main`'s catalog is a table in `main`'s commits |
| the view set coming from `sqlite_master` | drive it from the pin's `dolt_at_*` modules, which is the question the reader is actually asking |

`Pin`, `install_views`, the `pinned_` prefix and the `Reads::Own` /
`Reads::At` parameter all stay: a reader still needs one commit to hold
still for a whole pass, and `dolt_at_` is the only way it can get one.

## Open questions, in the order they should be answered

1. **Merge cost.** A branch with N changed rows merged into `main`, on a
   fixture store and on a real-size raw store. Does it touch the whole
   prolly tree or only changed chunks, and how does the file grow
   (cf. #489)? This decides whether commit-then-merge is affordable at
   streaming checkpoint cadence, which is the whole design's hinge.
2. **A pinned reader during a merge.** Does the pin hold, does the merge
   wait, does either fail? The two measurements above were taken with an
   *unpinned* reader; the pinned case is the one production runs.
3. **`doltlite_two_process_test` with the writer on a branch**, then with
   two writers on two branches merging concurrently. Nothing here is real
   until that suite has run with it — and #400 (a reader's `dolt_status`
   breaking a writer's commit) needs re-measuring under branches, since
   it may be the same shared-working-set bug.
4. **A crash between "sealed on branch" and "merged into main."** What is
   left, and can the next open find and finish it? Today
   `discard_dirty_working_tree` resets the working set; the branch
   equivalent has to decide between replaying the merge and dropping the
   branch.
5. **The doltlite README's Concurrency section**, read directly rather
   than through this repo's summary of it — #647 notes a newer excerpt
   mentioning "snapshot pins", which may be a supported primitive that
   makes half of `pin.rs` unnecessary after all.

## Why this is not a rider on the flake fix

The change touches every provider's open path, the streaming checkpoint
boundary, `--reset`, the migration ladder and the two-process
allowlist. The bug that started this conversation
(`no such table: pinned_markdowns`, a consumer reading a store 21ms
before its first `CREATE TABLE`) is fixed in one predicate, with a test.
This is the structural version of that fix and belongs in its own PR,
behind measurement 1.
