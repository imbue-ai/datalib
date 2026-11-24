# A step publishes its problems at each seal, and the server reads only that

**Status: proposed 2026-11-24, nothing built.** It changes one part of
[`problems_as_step_events.md`](problems_as_step_events.md): where the
server reads a step's problems. Where a step keeps them, and every
rule about how they are swept, stays as that plan and
`datalib/backend/etl/README.md` §"Problems stay in the step that found
them" have it.

## 1. The problem

To show a step's problem count, the server reads the `problems` table of
every store in the step's folder (`datalib_step_problems::of_step`). For
a doltlite store that means opening the store read-only at `main`. On a
small store that costs milliseconds. On a large one it does not, and it
happens on every Manage refresh whose store changed since the last one:

- **Opening is slow.** A doltlite open reads the chunk index saved at
  the store's last checkpoint and replays everything written since. A
  6.8 GB raw store took 7 to 19 s to open, to read a table of a few
  hundred rows. With the server re-reading it, Manage rows took 8 to
  11 s to build, most of it these reads.
- **An open blocks the store's writer.** A read-only open holds
  SQLite's SHARED lock on the store file until it has loaded the store,
  and a commit needs that file to itself, so it waits for the open and
  fails with `database is locked` once its busy timeout runs out
  (`docs/dev/doltlite.md` §"Locks and writers", fact
  `locks::a_commit_waits_out_a_read_only_open_and_fails_past_its_timeout`;
  upstream dolthub/doltlite#5218). During an upgrade, a 4.5 GB raw
  store's schema migration failed that way: the migration changes the
  file constantly, so the server's read cache missed on every refresh
  and the server opened the store every few seconds.

Both come from one choice: **to read a few hundred rows, the server
opens a store that can hold gigabytes.**

## 2. What not to do

- **Skip a step's problems while it runs.** That hides problems exactly
  while a long run finds them, and it is a rule the next reader of a
  store has to know about.
- **Move the `problems` table out of the store.** The table is part of
  how a step works, not only what it reports: `bulk.rs` deletes a
  record's problem in the transaction that writes the record; the
  Claude and ChatGPT downloads decide which attachments are still owed
  by joining it (`schema_raw.rs`); Gmail's owed messages read it
  (`listed.rs`); render skips a document it kept failed
  (`datalib_step/src/render.rs`). About thirty statements in the
  downloads and render rely on it sitting in the same file, in the same
  transaction, as the records. Moving it would split each of those
  writes across two files.
- **Retry a commit that a reader's open made busy.** It hides the
  reader rather than removing it.

## 3. The idea

**The step that owns a store publishes its problems at each seal, in a
small plain-SQLite file beside the store, and the server reads only
plain files.**

A seal (`doltlite_raw::commit_run`) already ends by publishing:
it fast-forwards `main` so readers see the commit. Publishing the
problems is one more thing it does at the same moment, by the same
process, from the same commit:

- **The file.** `<store stem>.problems.sqlite` in the step's folder
  (`entities.problems.sqlite` beside `entities.doltlite_db`,
  `indexed_markdown.problems.sqlite` beside the render store). It holds
  the `problems` table in `ProblemRow`'s shape, render's
  `entity_items` where the store has one, and a one-row table naming
  the commit it was published from.
- **When it is written.** After `main` moves, and only when the
  `problems` or `entity_items` table changed between the old `main` and
  the new one. `publish_on` already asks `dolt_diff_summary` that
  question for `datalib_content` (`content_moved`); this asks it for two
  tables.
- **How it is written.** Whole, to a temporary file in the same folder,
  then renamed over the old one. A reader opens either the old file or
  the new one, never a half-written one, and nothing locks anything.
  `embedding_map.rs` writes its JSON the same way.
- **When the step dies between the seal and the rename.** The file
  names an older commit than `main`. The next open of the store,
  which already finishes a seal a killed writer left unpublished
  (`a_seal_a_killed_writer_committed_but_never_published_is_published_by_the_next_open`),
  publishes the problems too.

The server's reader (`datalib_step_problems::of_step`) then reads every
plain `.sqlite` file in a step's folder that holds a `problems` table, and
no `.doltlite_db` file. That is the published copies, the
`problems.sqlite` a step with no store of its own already keeps, and the
grid index's own file. `datalib-http` then opens no store's rows on a
refresh, except the blob store's, which it serves.

## 4. Before and after

| | today | after |
|---|---|---|
| a Manage refresh while a large store is written | reopens the store, seconds each | reads a small file, milliseconds |
| a long migration or write beside an open Manage screen | can fail with `database is locked` | the server never takes the store's lock |
| when a problem shows | at its step's next seal | the same |
| who writes the problems | the step, in its store | the same, and the step publishes them |
| what the server reads | every store in the step's folder | plain files only |

What a person notices: Manage stays fast while a large source syncs or
migrates, and a sync no longer fails because the Manage screen is open.
Problems show at the same moment they do today.

## 5. The rule this changes

`etl/README.md` says of the `problems` table that "nothing copies it".
That rule is about one step copying another's problems (render copying
a download's rows, `grid_index` copying everyone's), which made
problems late and stale. This copy is made by the owner, from its own
commit, at the seal, the way `main` is the published view of
`datalib_writer`. No other step reads or writes it. The README section
says so when this lands.

## 6. Forward path for existing data

Nothing in a store changes. A root from before this has no published
files, so every step publishes its file the next time its store opens.
`--prepare` opens each store a build has not opened yet, so the first
start of the new build writes them all.

Until a step's file exists, its problems are unread, and the Manage row
says so rather than showing none. Today an unread store is only logged
(`datalib_step_problems::say_unread`), and `OfStep::older_shape` is
collected and shown nowhere; this is the place to make both visible, so
that no step looks clean only because its problems were not read.

## 7. Order of work

1. **Publish at the seal.** `commit_run` writes the file when the two
   tables moved; `open` writes it when the file names an older commit
   than `main` or is missing. A two-process test: a reader of the file,
   beside a writer sealing as fast as it can, never sees a half-written
   file or a commit older than one it already read.
2. **Read only plain files.** `of_step` drops its `.doltlite_db` branch
   and its size-and-time cache; the server's read becomes a small file
   per step. A step with no file yet shows as unread on its Manage row.
3. **The docs.** `etl/README.md` §"Problems stay in the step that found
   them", `docs/dev/app_stores.md` (a new file per step and its one
   writer), and `AGENTS.md`'s sync-pipeline paragraph, which lists the
   server's exceptions to "never reads a store's rows".

## 8. Open questions

- **How big can a published file get?** A seal that changes the table
  rewrites all of it. A source with a broken credential fails every
  record, but the back-to-back failure budget already keeps that to one
  `phase:` row. A count of rows per published file in the run log would
  show whether any root comes close to mattering.
- **Is a step that is turned off asked to `--prepare`?** If not, its
  file appears only when it next runs, and its problems show as unread
  until then.
- **The render's own reads of its input store** open the same large raw
  stores, and can block a download's seal the same way. That is
  doltlite's to fix (dolthub/doltlite#5218); this plan does not
  address it.
