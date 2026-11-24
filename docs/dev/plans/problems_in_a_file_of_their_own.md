# Problems in a file of their own, beside each step's data

**Status: proposed 2026-11-24, nothing built.** It reverses one decision
of [`problems_as_step_events.md`](problems_as_step_events.md): that a
step with a store keeps its problems in that store's `problems` table.
Everything else there stands: one shape (`ProblemRow`), read where the
step keeps it, never copied by another step, swept by the step that
wrote it. File and line references are at `9eaa95ce1`.

## 1. The problem

A store holds what a service said: messages, events, pages. A problem
is something else: what the step ran into while mirroring it. The two
differ in every way that matters:

| | mirrored data | a problem |
|---|---|---|
| what it is | what upstream said | what our step could not do |
| how long it matters | kept, with its history | until a retry works |
| who reads it | the next step, pinned to one commit | people and the Manage screen, live |
| how big it gets | gigabytes | hundreds of rows |

Keeping them in one doltlite file costs us three ways:

- **Reading a few rows means opening a large store.** To count a step's
  problems the server opens every store in the step's folder read-only
  (`datalib_step_problems::of_step`). A 6.8 GB raw store took 7 to 19 s
  to open, and Manage rows took 8 to 11 s to build, most of it these
  reads.
- **That open blocks the store's writer.** A read-only open holds
  SQLite's SHARED lock on the store file until it has loaded the store,
  and a commit needs the file to itself (`docs/dev/doltlite.md` §"Locks
  and writers"; dolthub/doltlite#5218). During an upgrade a 4.5 GB raw
  store's schema migration failed with `database is locked` while the
  Manage screen kept reopening it.
- **A problem counts as new data.** `moves_version`
  (`etl/src/doltlite_raw.rs:385`) treats a change to `problems` as a
  change to the store's content, so a new fetch failure wakes render
  even when no record changed. Render then reopens the raw store only
  to read that store's fetch problems (`fetch_problem_entities`,
  `datalib_step/src/render.rs:1245`).

## 2. The idea

**Every step keeps its problems in `problems.sqlite` in its own folder,
and its stores hold data and the bookkeeping it decides from.** That
file already exists: a step with no store of its own keeps its problems
there (`datalib_step_problems::StepFile`). After this, every step does.

- **The file**: plain SQLite in WAL mode, the `problems` table in
  `ProblemRow`'s shape, the step its only writer. A reader never blocks
  it, and it never blocks a reader.
- **The reader** (`of_step`) reads `problems.sqlite` and nothing else.
  `datalib-http` then opens no store's rows on a refresh, except the
  blob store's, which it serves.
- **Problems show as they arise**, when the step commits them, not at
  the store's next seal.

## 3. What the stores use problems for today

A survey of every statement that reads or writes `problems` outside
tests found 40 places. Most only report, and the coupling that matters
is transactions, not decisions.

**33 only report**: they record, clear or count rows for people.
Thirteen of them write in the same SQL transaction as the record they
describe, among them:

- `bulk.rs`: clearing a record's fetch problem when it is fetched;
- `doltlite_raw::record_fetch_problem`, behind every `record_object_*`;
- `prune.rs`: forgetting the problems of pruned records;
- `file_checkpoint.rs`: a file's problem beside its `ingested_files`
  stamp;
- `indexed_markdown.rs`: a document's problems beside the document.

Three of those (`prune::delete_owned_in_tx`, DAV's `forget_collection`,
email's `delete_emails_in_tx`) find the problems to delete with a
subquery on the data tables they are about to delete from.

**Three decide something every run:**

1. **Render keeps a document it could not render again**, and
   `render_versions` (`indexed_markdown.rs:535`) leaves such a document
   out of the renderer versions it checks, by excluding the markdowns
   that have a `render_failed` or `no_document` problem. No bookkeeping
   holds "kept on purpose" today; the problem row is the only record.
2. **Render links a download's problems to grid rows** by reading the
   raw store's fetch problems at the commit it rendered
   (`fetch_problem_entities`) and writing the pairs into its own
   `entity_items`.
3. **`moves_version`** counts `problems` as content, so that a new
   problem wakes render to link it.

**Eight migration rungs** read or rewrite problems once, when a store
climbs past them (Claude and ChatGPT attachments, Gmail's listed
messages, Notion's comments, Slack, YoLink, the forge sources, and
calendar and contacts over DAV). Four of them decide something from a
problem row. For three, every store past the rung keeps the fact in
bookkeeping: `held_version` and `last_error` on `*_bookkeeping`, and
`listed_messages`. Notion's rung decides whether to seed its comments
from a `listing:comments` problem, once; nothing after it needs that
fact.

Nothing that decides what a download still owes reads problems:
`datalib_etl_web::owed` and `failed_ids` read only the bookkeeping
sidecars.

## 4. What changes

**State moves into the store's bookkeeping; reports move out.**

- **A document kept on purpose** gets a column of its own, `kept` on
  `markdowns` (or a small table of kept units), written in the same
  transaction as the keep. `render_versions` reads it. The problem row
  stays, as the report of it.
- **Linking a download's problem to its grid row** becomes the
  reader's job, as `problems_as_step_events.md` §"What a problem is
  about" already intends: a problem names its record's upstream values,
  every grid row carries them, and the reader looks the row up in the
  grid index. Until that plan's per-provider recipes land (its step 7),
  render keeps building `entity_items`, reading the download's
  `problems.sqlite` instead of its raw store.
- **`moves_version` drops `problems`.** A problem no longer wakes
  render. Until the reader links by upstream values, a new fetch
  problem shows unlinked until render next runs for another reason.

**Every write moves to the step's file, and goes first.** The store and
the file can no longer commit together, so a step changes its problems
*before* the data they are about, never after:

| a crash between the two | what is left | what heals it |
|---|---|---|
| a fetch problem cleared, the record not yet stored | no problem, the record still owed | the next run fetches it, and records the problem again if it fails again |
| a fetch problem recorded, the failure not yet in bookkeeping | the problem, the record still owed | the next run retries and sweeps it |
| a document's problems replaced, the document not yet sealed | problems for a document version not in the store | the document is still changed, so the next run renders it again and replaces them |
| a pruned record's problems forgotten, the record not yet deleted | the record, without its problems | the next run prunes it again |

In each case the file is at most one batch ahead of the store, and the
store, which the step decides from, is never wrong. The other order
would leave a problem that no later run touches: the record is held, so
nothing tries it again. The three deletes that find their rows with a
subquery read the ids from the data first, delete the problems, then
the data.

The helpers already gather these writes in a few places: the
`record_object_*` family, `bulk.rs`, `prune.rs`, `file_checkpoint.rs`,
`download_problems::apply` and `RunProblems`, and the render store's
`put_*` and sweep functions. Each takes the step's problems file
instead of the store's transaction.

## 5. Before and after

| | today | after |
|---|---|---|
| a Manage refresh while a large store is written | reopens the store, seconds each | reads a small file |
| a long migration or write beside an open Manage screen | can fail with `database is locked` | the server never opens the store |
| when a problem shows | at the store's next seal | when the step commits it |
| a new fetch failure | moves the raw version, wakes render | moves nothing downstream |
| a new fetch failure's link to its row | at render's next run, which it triggers | at render's next run for another reason, until linking moves to the reader |
| where a step's problems are | its stores, or `problems.sqlite` | `problems.sqlite`, always |
| a problem's history | in the store's commit log | none: it is current state |

What a person notices: Manage stays fast while a large source syncs or
migrates; a sync does not fail because the Manage screen is open;
problems show sooner. A new fetch failure may sit unlinked a little
longer, as above.

## 6. Forward path for existing data

Every root has problems in its stores now, and mutes are kept by a
problem's id (`problem_mutes`), so the rows must move, not be found
again:

- **Raw stores: a rung on the shared ladder** (`SHARED_LADDER`,
  `etl/src/doltlite_raw.rs`) copies the store's `problems` rows, ids
  and stamps unchanged, into the step's `problems.sqlite` and drops the
  table. It sits above every provider rung that reads `problems`, so
  those run first on a store that climbs from far below. A copy that
  fails leaves the store as it was and the step refusing to prepare,
  the way any rung does, rather than dropping rows.
- **Render stores are rebuilt.** Dropping `problems` and adding `kept`
  changes a derived store's shape, and a derived store in another
  shape is rebuilt from its inputs (`etl/README.md` §"Schema
  self-healing"),
  at the cost of one full re-render per source. Its problems are found
  again with the same ids, since an id is a function of what the
  problem is about, so mutes hold; their first-seen stamps restart.
  Copying them first would keep the stamps, if that is worth a rung.
- **The time-series and occurrences stores**, which keep their own
  problems today (`step_problems::replace`), are derived too: rebuilt
  the same way.
- **The grid index's own row** ("render store unreadable") moves to
  the index step's `problems.sqlite`, and is found again on its next
  pass.
- `--prepare` opens every store a new build has not opened, so the
  first start of the build climbs every rung.

## 7. Order of work

1. **The writer.** `datalib_step_problems::StepFile` becomes the one
   writer for every step, with the sweeps the stores' helpers do now
   (by scope, by prefix, by stage). Helpers take it instead of a
   transaction, writing before the data. A test per row of the crash
   table above: kill the step between the two writes, run again, and
   check what the file and the store hold.
2. **The state.** `kept` on `markdowns`, and `render_versions` reading
   it.
3. **The raw rung**, with the shapes every release left
   (`datalib_step/raw_shapes/`) migrated in a test, ids and stamps
   compared; and the render store's new shape, which rebuilds it.
4. **The readers.** `of_step` reads only `problems.sqlite`; render's
   `entity_items` reads the download's file; `moves_version` drops
   `problems`.
5. **The docs.** `etl/README.md` §"Problems stay in the step that found
   them", `docs/dev/step_protocol.md` §"Problems: what you could not
   do", `docs/dev/app_stores.md`, and `AGENTS.md`'s sync-pipeline
   paragraph, which lists the server's exceptions to "never reads a
   store's rows".
6. **Linking by upstream values**, with `problems_as_step_events.md`'s
   step 7, after which render stops building `entity_items`.

## 8. Open questions

- **Is "problems first" right for every helper?** The survey found the
  writes; it did not check each caller's order of operations. Step 1's
  crash tests are where that is settled.
- **Does anything read problems from a store's history?**
  `datalib_history` lists `problems` among Datalib's own tables
  (`DATALIB_TABLES`), so the history panel may show its changes. Nothing
  found decides from it.
- **How many rows can a run write?** A source with a broken credential
  fails every record, but the back-to-back failure budget already keeps
  that to one `phase:` row.
- **The render's own reads of its raw store** still open a large store
  beside the download that writes it, and can block its seal the same
  way. That is doltlite's to fix (dolthub/doltlite#5218).
