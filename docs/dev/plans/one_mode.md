# One mode: how every step writes its store

**Status: agreed design (2026-09-14), nothing built.** This is the
design that [`render_inputs.md`](render_inputs.md) and the
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

1. **Verify the load-bearing assumption.** A test that opens a raw
   store, begins a transaction, inserts, and is `kill -9`ed before
   `COMMIT` — then reopens and asserts the rows are not in the working
   set (and that `rescue_dirty_working_tree` finds nothing to seal).
   `doltlite_two_process_test` measures overlap, not crash. If this
   fails, the whole model needs a different atomicity boundary, so it
   goes first.
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
3. **Render: documents in transactions, cursor in the store.** Wrap
   `apply_markdown` (and the per-document sweep in `put_document`) in
   `begin_transaction`/`commit_transaction`; add a `render_cursor`
   table (one row: `from_commit`, `params`) written in the final
   commit; delete `render_cursor.rs` and every provider's read/write
   of it; delete `discard_tree` and `tree_is_from_an_older_renderer`'s
   removal branch — the version check becomes "re-render everything".
   Closes 1.1–1.4.
4. **Ingest: wipe at the end.** `reset_and_redownload` stops
   truncating; it clears bookkeeping and scope state, and the run
   prunes at the end. Mirrors move to upsert+prune per table (from 2).
   pdf and fsindex prune after the walk instead of resetting before it
   (`fswalk` already has the seen-set). `checkpoint_policy` and
   `Policy::Never` are deleted; `always_clear_before_ingest` is
   deleted from `SourceCommon`, the config examples and the wizard.
   Add `--start-over` as the one truncating operation, committing its
   truncate before it fetches. Closes 2.1, 2.2, 2.3, 2.5.
5. **Consumers propagate.** `build_grid_index`'s cold path (no cursor,
   or unresolvable) reads the store whole *and* prunes index rows for
   documents the store no longer has — an empty committed store is an
   honest "nothing". `scan_buckets`'s unresolvable-cursor log goes to
   `warn`. Render's cold start, once `render_inputs` lands, prunes
   undeclared buckets. Closes 1.6 and the "cold start deletes nothing"
   gap.
6. **The slack applet reads through `open_reader`**, and `lint_repo.py`
   check 5 walks `applets/` and `http/` as well as `_render` crates.
   Closes 2.4.
7. **`render_inputs`**, per its own doc, now as the render-side
   implementation of rule 2 rather than a standalone proposal.
8. **Prose.** Rewrite `provider_migration_dolt_diff_and_cas_edge.md`
   §"Edge cases" and the streaming plan's §"Producer side" (the
   `Never` paragraphs, the "disabled for the whole run" claim, the
   whatsapp row). The sink contract keeps P1 and says P2 is automatic
   for doltlite sinks. AGENTS.md's one-paragraph pipeline description
   gains one sentence: every commit is readable.

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
