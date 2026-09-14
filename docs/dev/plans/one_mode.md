# One mode: how every step writes its store

**Status: agreed design (2026-09-14); the render side is built, the
ingest side is not.** Of §"What changes in the tree", items 1, 3, 5,
6 and the render half of 8 landed on 2026-09-14 — the crash test, the
render store's transactions and in-store cursor, the index's cold-path
prune, the slack applet on `open_reader`. Items 2 and 4 (the mirror
measurement, wipe-at-end for ingest, `Policy::Never`,
`always_clear_before_ingest`, `--start-over`) and 7 (`render_inputs`)
are still to do; each item below says which. §"Testing it without a
provider" is how to check the rule without a real source, also not
built. This is the design that
[`render_inputs.md`](render_inputs.md) and the
[deletion-record audit](deletion_record_audit_2026_09_11.md) turned out
to be reaching for. Read this first; those two are now the mechanism
for one of its rules and the measurement that motivated it.

## The problem, in one paragraph

The pipeline has good building blocks — doltlite commits a consumer
can pin, `dolt_diff` between two of them, per-page SQL transactions,
checkpoints, an interrupt hook, a rescue commit — and assembles them
in more than one way. A normal ingest appends and commits whenever. A
reset truncates first and must not commit until the end
(`Policy::Never`). Three mirrors and two scans truncate on *every*
run and are safe only because they never report writes. The interrupt
hook commits regardless of any of that. Render keeps its cursor in a
file it writes before the commit it describes, and removes its whole
store on a version bump. Each of these is locally reasonable, and
together they produced the audit: a torn commit is one hand-run step
away, and the guards against deleting a live source number five. The
cause is that there are several modes and nobody can hold them all in
their head. This document reduces them to one.

## Two boundaries, two jobs

A store has two kinds of commit and they do different work:

- A **SQL transaction** is the *atomicity* unit. It is all-or-nothing:
  a crash mid-transaction rolls it back, and a committed one is in the
  store's working set from then on. It is also how writes are batched
  — doltlite charges about 50ms per auto-committed statement, so a
  loop of bare statements is ruinous (see the comment on
  `build_grid_index`).
- A **doltlite commit** is the *publication* unit. It names a snapshot
  a reader can pin through `dolt_at_`, and `dolt_diff` between two of
  them is the record of what changed. It seals whatever SQL
  transactions have landed in the working set since the previous one.

Many SQL transactions per doltlite commit is normal and intended. The
fact that makes the rest of this document necessary is: **a writer
does not choose which SQL transactions end up in a commit.** The
working set lives in the file and survives the process. A checkpoint
seals it. Ctrl-C seals it. A crash leaves it, and the next writer to
open the file seals it as a `rescue:` commit. The end of the run seals
it. So "don't commit during the wipe" never protected anything; it
deferred the commit to somebody who did not know what they were
committing.

The guarantee a doltlite commit can inherit is therefore exactly the
guarantee every SQL transaction gives it, and no more:

> **Every SQL transaction leaves the store in a state a consumer may
> read.** A doltlite commit is a publication point and may happen at
> any moment between transactions — checkpoint, Ctrl-C, rescue, end of
> run — without anyone checking what is in it.

"A state a consumer may read" means *truthful*: it is what this step
believes about its source so far. It does not mean *complete*. A
download interrupted after 40% of its pages is a truthful store of
40%; a render interrupted after 300 documents is a truthful store of
300. What is never truthful is a store that says "nothing" or "less"
because it is about to say "more" — a truncate whose refill has not
landed yet.

Readers never see the working set; they pin a commit. The rule is
about what *reaches* a commit, not what a concurrent reader might
glimpse.

## The one mode

Every step that writes a doltlite store — ingest, render, index —
follows the same five rules. There is no flag that switches any of
them off.

1. **Additive first.** Upsert; never delete a row you are about to
   re-insert. A transaction boundary never falls *inside* a unit of
   work — a page of an API listing, a mirrored table, a rendered
   document with its rows and edges — so a document is replaced whole
   or not at all. How many units sit between two boundaries is a
   batching choice, made for throughput (doltlite's per-statement cost
   makes big transactions the fast path), and it is not a correctness
   choice: one unit per transaction and a thousand are both fine.
2. **Prune at the end, scoped to what you enumerated.** In one
   transaction, delete what this run walked *completely* and did not
   see. The scope is the provider's to name: claude prunes an org's
   conversations against that org's listing; a mirror's source table
   is its own seen-set; `fswalk` knows every path it visited; render
   prunes the documents of every bucket it declared. A scope the run
   did not fully enumerate — a slack sweep that stopped at recent
   channels, beeper's evicting `index.db` — is not pruned. That is
   strictly better than a wipe: you keep what you cannot re-list.
3. **The cursor lives in the store and moves in the same commit as the
   work it describes.** `grid_index`'s `source_cursors` already does
   this. Render's `_render_cursor.json` goes away. A cursor can then
   never claim more than its store holds, and there is no separate
   file to lose or to write non-atomically.
4. **Commit whenever.** Checkpoint cadence, Ctrl-C, rescue, end of run
   are the same call. Committing on Ctrl-C is *right* — it keeps the
   download or render effort — and under rules 1–3 it is safe.
   `Policy::Never` is deleted; nothing consults a policy.
5. **Consumers propagate faithfully, deletions included.** A consumer
   reads `dolt_diff` from the commit its cursor names to the commit it
   pinned, and acts on every row, `removed` rows included. A store
   with a committed schema and no rows is an honest answer — the
   source holds nothing — and a consumer of it deletes accordingly.
   The only thing a consumer skips is a store it *cannot read*: no
   file, no commit, no doltlite extension. That is P1 of the
   [sink contract](streaming_steps_plan.md#the-sink-contract), and it
   is the one guard that stays.

Two things stop being modes:

- **Streaming.** A consumer may run at any commit, because every
  commit is readable. `streams_output` stays meaningful only for a
  sink that is not a doltlite store (qmd's FTS5, rewritten in place);
  for every doltlite sink P2 is a property of the engine, not of the
  step.
- **Cold start.** A render with no cursor, a changed renderer version,
  or changed params is "put every bucket in the re-render set", with
  the cursor kept. The end-of-run prune removes whatever the new
  version or params no longer produce. `discard_tree` goes away.

## The operations

Every user-visible operation is the one mode with different inputs.

| operation | what it does | what a consumer sees |
|---|---|---|
| **sync** | walk from the cursor; upsert; prune enumerated scopes; commit | the upstream delta |
| **re-verify** (today's `--reset-and-redownload`, renamed to say it does not wipe) | clear the bookkeeping that lets a walk skip — cursors, `fetched_at`, scope state — then sync | the upstream delta, and nothing else: an unchanged row upserts to itself and `dolt_diff` shows no change |
| **start over** | in one transaction, truncate every entity and bookkeeping table; commit it as its own commit (`start over: N rows dropped`); then sync | everything removed, then everything added back as it arrives. Expensive downstream — qmd re-embeds — and honest: the user asked for the store to be empty, so the derived stores are empty until it is not |
| **version bump / param change** (render) | every bucket goes in the re-render set; cursor kept; prune at the end | the documents that changed, and the ones the new version no longer produces |
| **Ctrl-C** | seal the working set; report `cancelled` | a truthful partial store; the next run resumes from it |
| **crash** | nothing; the next writer's `open` seals the working set as `rescue:` | the same |

"Start over" is the only operation that publishes an empty commit,
and it is the only one where empty is true. It truncates rather than
`DROP TABLE`s so that `dolt_diff_<t>` never has to cope with a table
absent at one ref; the mirror engine's drop-and-recreate is diffable
too (lightroom `INGEST.md`), so that is a preference, not a
requirement. It is a command — `datalib-step ingest --start-over`, and
a button that says what it will do — never a config flag, because a
flag that wipes on every run is the shape that needed `Never`.

"Start over" touches the raw store only. The render store, the index
and qmd empty by propagation — the same `dolt_diff` path as any other
deletion — with one more commit in each history ("everything
removed"). Render never learns the operation happened, which is the
point: an operation only the ingest knows about is one that cannot be
half-applied downstream.

`always_clear_before_ingest` goes. It was "prune what this run did not
see, every run", which under rule 2 is what every snapshot-shaped
source (an export, an mbox, a mirror, a scan) does inherently, and
what a partial-walk source must never do. `all_sources.toml` already
says it is "the wrong fix" for beeper; it was the only fix, and now it
is not needed.

## What is a "unit of work"

Rule 1 says each unit is a transaction, and the audit's 2.4 shows
why it matters: render's `apply_markdown` today runs `DELETE FROM
grid_rows WHERE markdown_uuid = ?` and the inserts as separate
auto-committed statements, so a commit landing between them seals a
document with no rows. The units, per step:

| step | unit | today |
|---|---|---|
| API ingest (claude, chatgpt, slack, email, github, gitlab, notion, …) | one page: its entity rows, edge rows, bookkeeping | already a transaction (`bulk_upsert_in_tx`; #7's "per-page transaction discipline") |
| mirror ingest (lightroom, apple_photos, whatsapp) | one table: upsert from the source, prune not-in-source | **not**: drop-all is its own transaction, then create+copy per table |
| scan ingest (pdf, fsindex, media) | one file's rows | reset before the walk is its own transaction |
| render | one document: `markdowns`, `grid_rows`, `edges`, `render_problems`, `render_inputs` | **not**: bare statements under the write lock |
| render, end of run | the prune and the cursor | the prune is bare statements; the cursor is a file |
| grid_index | the whole load, prune, cursors | already one transaction |

A run that spans tables leaves a cross-table partial state visible to
a consumer pinned mid-run: this run's messages against last run's
chats, a thread whose replies are not in yet. That is already true of
per-page ingest today, renderers already tolerate a dangling reference
(`render_problems` records it), and "what the step believes so far" is
a truthful description of it. Accepted, and said here so nobody
re-litigates it as a bug.

It can be made smaller by ordering, and providers should: **fetch the
globally-joined tables first** — `users`, `orgs`, `workspaces`,
`channels`, `recipients`, `me`, whatever every document's header
reads — before the entities that reference them. Then a consumer
pinned at any commit of the run sees new messages with their authors
resolvable, rather than a wave of "unknown user" documents that
re-render when the users land. It is a hint rather than a rule
because some sources cannot honour it (a listing that yields authors
only as it goes), and `render_inputs`' "record the lookup, not the
hit" rule is what makes the other order merely wasteful rather than
wrong.

## What this closes

Against the audit, finding by finding:

| finding | under the one mode |
|---|---|
| 1.1 `discard_tree` loses both ranges | gone — a version bump is a full re-render with the cursor kept and a prune at the end |
| 1.2 param change drops the range | gone — same path |
| 1.3 cursor written before the commit; no transaction | gone — rule 3, rule 1 |
| 1.4 cursor write not atomic | gone — no file |
| 1.5 recipe says wipe the cursor on reset | recipe rewritten with the mode |
| 1.6 unresolvable cursor logged at `info` | still `warn` — it means the file was replaced by hand, which is now the only way to lose a range |
| 2.1 Ctrl-C commits regardless of policy | *correct* — every commit is safe |
| 2.2 rescue commits a crashed wipe | correct — there is no wipe to crash inside |
| 2.3 hand-run render reads a torn commit | reads a truthful partial store; nothing is mass-deleted |
| 2.4 slack applet opens the render store writably | still a bug (a reader must never rescue-commit or run `dolt_status` on a store it does not own); the damage shrinks from a torn document to lost checkpoint rows once documents are transactions |
| 2.5 wipers protected by omission | nothing to protect |
| 2.6 streaming ingests already honour it | they are the model for everyone else |

And against `render_inputs.md`: the one route it could not fix — "a
checkpoint taken mid-wipe" — is closed, because there is no mid-wipe.
Its `render_inputs` table becomes the mechanism for rule 2 on the
render side: the buckets a run declared are "what I enumerated", and
the prune is every document under a declared bucket that was not
emitted. Its "asked-but-undeclared bucket is an error" rule stands.

## What changes in the tree

In dependency order. Each item is a PR; the first two are prerequisites
and small.

1. **Verify the load-bearing assumption.** *Built.* A test that opens a
   raw store, begins a transaction, inserts, and is `kill -9`ed before
   `COMMIT` — then reopens and asserts the rows are not in the working
   set (and that `rescue_dirty_working_tree` finds nothing to seal).
   `doltlite_two_process_test` measures overlap, not crash. It passed
   (`a_transaction_a_killed_writer_never_committed_leaves_no_rows_behind`,
   with its twin for the SQL-committed half); the model stands.
2. **Measure upsert-into-existing for the mirror engine.** Against the
   lightroom fixture: `INSERT … SELECT … ON CONFLICT DO UPDATE` into an
   existing table plus `DELETE WHERE pk NOT IN (SELECT pk FROM src)`,
   per table in one transaction, versus today's drop-all + create+copy.
   Doltlite's content-addressing should make an unchanged-row upsert
   nearly free; that is the claim to check, along with whether the
   doltlite blob bug lightroom's `INGEST.md` records reappears. If it
   is too slow, the fallback is copy-into-staging then swap in one
   transaction, which needs a check that doltlite handles `ALTER TABLE
   … RENAME` with its history intact. Keyless tables cannot upsert and
   are already undiffable: refuse them at DDL time.
3. **Render: documents in transactions, cursor in the store.** *Built.*
   the documents between two checkpoints share one SQL transaction
   (`begin_batch`/`commit_batch`, closed right before each
   `dolt_commit`), each written whole inside it — `put_document` and
   `remove_document` join the open batch, or run as a transaction of
   their own outside one, so the driver's end of run — sweep, storage
   report, cursor — is one more;
   the `render_cursor` table (one row: `raw_commit`, `params`) is
   written there by the driver; `render_cursor.rs` and every
   provider's read/write of the file are gone — a provider reads
   `RenderCtx::raw_cursor`, declares its knobs through
   `RenderProcessor::render_params`, and reports the commit it pinned
   through `RenderCtx::consumed`; `discard_tree` is gone, and a
   version or param change is "render everything, fingerprints off,
   sweep what the walk did not produce" — the sweep runs only when
   every processor reported a consumed commit, since one that read no
   store said nothing about what should exist. The step reports the
   store's HEAD as its output version. Closes 1.1–1.4.
4. **Ingest: wipe at the end.** `reset_and_redownload` stops
   truncating; it clears bookkeeping and scope state, and the run
   prunes at the end. Mirrors move to upsert+prune per table (from 2).
   pdf and fsindex prune after the walk instead of resetting before it
   (`fswalk` already has the seen-set). `checkpoint_policy` and
   `Policy::Never` are deleted; `always_clear_before_ingest` is
   deleted from `SourceCommon`, the config examples and the wizard.
   Add `--start-over` as the one truncating operation, committing its
   truncate before it fetches. Closes 2.1, 2.2, 2.3, 2.5.
5. **Consumers propagate.** *Built, except the last sentence.*
   `build_grid_index`'s cold path (no cursor, or unresolvable) reads
   the store whole *and* prunes index rows for documents the store no
   longer has — an empty committed store is an honest "nothing".
   `scan_buckets`'s unresolvable-cursor log goes to `warn`. Render's
   cold start, once `render_inputs` lands, prunes undeclared buckets.
   Closes 1.6 and the "cold start deletes nothing" gap.
6. **The slack applet reads through `open_reader`**, and `lint_repo.py`
   check 5 walks `applets/` and `http/` as well as `_render` crates,
   and matches `open_derived` too. *Built.* Closes 2.4.
7. **`render_inputs`**, per its own doc, now as the render-side
   implementation of rule 2 rather than a standalone proposal.
8. **Prose.** The render half is done: the migration recipe's cursor
   section and §"Edge cases", `parse_and_render.md` §5, and AGENTS.md's
   pipeline paragraph (which says which side is readable at every
   commit and which is not yet). Still to do with item 4: the
   streaming plan's §"Producer side" (the `Never` paragraphs, the
   "disabled for the whole run" claim, the whatsapp row), and the sink
   contract saying P2 is automatic for doltlite sinks.

## Testing it without a provider

The DAG runner is tested with scripted steps and no real provider
(`dag/src/scheduler.rs`, `mod tests`): the runner's rules are checked
against stubs that speak the step protocol, so a provider can be wrong
without making the runner look wrong, and the other way round. The one
mode wants the same separation one layer down — for the render steps
first, and for the download steps as far as it goes. None of this is
built.

### The property

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
  store emptied ahead of a refill. This is the rule above and
  what the per-batch transactions are for.
- **The consumer converges.** `grid_index` pinning any sequence of
  those commits ends equal to the render store, deletions included.

The property is the whole specification. Everything below is how to
check it without needing a real source, and then how to check that a
real source keeps its side of the bargain.

### Layer 1: the framework, with a synthetic provider

*Built (2026-09-14): `datalib_step/src/render_model_test.rs`, driving
the real `render_source`, `IndexedMarkdownStore`, `scan_buckets` and
`build_grid_index`; the only fake is the provider. Its first run found
a real hole: a `global_fanout_tables` hit made the scan return "render
everything" and the providers then probed nothing for removal, so a
conversation deleted in the same range as a user rename stayed in the
store for good — in every diff-narrowed provider. `DiffScan` now
carries the set to render and the set the diff named separately.*

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

**What it is.** A `#[cfg(test)]` module in `datalib_step`, since the
driver's core (`render_source`, split from the step shell for exactly
this) lives there. Twelve seeds, ten runs each, one to three commits
of one to five mutations per run, a version bump or a param change one
run in six, a failure one run in four, the checkpoint cadence at zero
half the time so every document is a checkpoint, the index run after
half the runs and at the end. About five seconds. A failure prints the
seed and the whole history. The generator is a seeded xorshift; no
new dependency.

### Layer 2: the provider contract

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

### The dump/compare helper both layers need

One function: `render_store::logical_dump(path) → String`, sorted
rows of the four tables with the volatile columns dropped, plus the
`.md` tree as `(relative path, blake3)`. `fixture_db_snapshot_test`
does most of this for the index (`stable_row_set_hash`,
`stable_source_url`); lift it into `datalib_etl_render` as a test
utility so both layers and that snapshot share one notion of
"the same".

### Downloads

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

And the guarantee is weaker by design. Rule 2 says a
scope the run did not fully enumerate is not pruned — slack's recent
channels, beeper's evicting `index.db` — so for those the equivalence
holds only modulo the scopes the provider declares it does not
re-list. The contract has to name them, and the harness checks
equivalence outside them and *non-deletion* inside them. That is a
more honest statement of what incremental download promises than any
prose we have now.

### Order

1. The dump/compare helper, lifted from `fixture_db_snapshot_test`.
2. Layer 1 — the synthetic provider and the model test. Worth doing
   before the ingest half of this plan (items 2 and 4 above), because
   the driver is about to be relied on harder and this is the test
   that sees it.
3. Layer 2 — the mutation harness, run against every ported render
   provider. Expect it to find something: the fan-out gap this month's
   work documented (a `users` change renders everything and probes
   no removals) is a clause-3 breach the harness names on its first
   delete of a `users` row.
4. Downloads, mirrors and scans first, then export-shaped, then the
   synthesizers.

## Open questions

- **A per-row "seen this run" mark.** Rule 2's prune needs to know
  what the walk saw. The bookkeeping sidecar has `fetched_at` but no
  per-run mark; the choices are a run id column on the sidecar, or a
  temp table of seen ids built during the walk and joined at the
  prune. The temp table is simpler and leaves no trace in history;
  the column survives a crash and lets a resumed run continue a prune
  scope. Decide when building item 4; the mirror engine needs neither
  (its source table is the seen-set).
- **The UI during start over.** The grid will empty and refill. The
  Manage screen should say "starting over: N of M re-downloaded" rather
  than let the grid quietly go blank. Not a blocker for the backend
  work.
