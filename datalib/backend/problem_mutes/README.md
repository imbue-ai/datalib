# Muting a problem row

A **problem** is one row saying what a step could not fully do to one
record (`datalib/backend/problems/`, and `etl/README.md` §"Problems flow
downstream with the data"). Many are true and cannot be fixed: a
corrupt PDF, a date written in a form nobody can parse, a file no source
reads. Every error and warning used to count toward the Dashboard's
"Needs you" and the Manage row's count forever, so "Needs you" was
almost never empty, and a problem that did need someone (an expired
sign-in, a failed fetch) was easy to miss among them.

A person can now **mute** a problem row they have seen. In the problems
grid (the one "Review" and a Manage row's count open) they select rows,
right-click, and pick **Mute**; **Unmute** undoes it.

## What a mute does

- **The row stays.** It is still in the problems grid, faded, with a
  MUTED chip whose hover says who muted it and when, and still in the
  document's banner, marked muted. Nothing is hidden for good.
  `muted:no` (or `-muted:yes`) in the grid's search bar leaves the
  muted rows out; `muted:yes` shows only them.
- **It counts toward nothing.** It comes off the count of the step that
  found it, so off the Manage row and off the group's sum. A source whose
  only problems are muted shows no count, and drops off "Needs you",
  which reads those counts (`needsYou` in `ui/src/cards/dashboard.ts`).
- **It lasts until the problem changes.** A mute is kept by the problem's
  id, which `datalib_id::problem_id` mints from what names the problem —
  the source, the stage, the record, the field, the reason, the rule —
  and never from its sample or its stamps. A run that finds the same
  problem again mints the same id, so the mute holds across syncs; a
  problem that changes into another (a different reason, another field)
  is a new id, and comes back unmuted.
- **It outlives the data.** A reset, "clear and redownload" or deleting
  a source rebuilds the problems, not the mutes: the store is under
  `datalib_curated/`, which nothing rebuilds (`docs/dev/app_stores.md`).
  A problem found again after a redownload has the same id and is still
  muted.

Muting a whole kind (every "file no feed reads" from one source) and a
mute that expires are not built.

## Where a mute lives, and who writes it

`<data_root>/datalib_curated/datalib_problem_mutes/mutes.doltlite_db`,
one table, `mutes`: the problem's id, who muted it (`muted_how`,
`manual` or `agent`, with the agent's name), and when (`muted_at_utc` and
`tz_offset`). The file is made by the first mute.

Its **one writer is the `unified_document_index` applet**, which serves
the problems grid: `POST /problems/mute` there takes
`{"problem_uuids": […], "muted": true|false, "agent": "…"?}` and commits
each call. That applet is the one the app cannot run without, and the
one that already reads the problems, so the route sits beside the rows
it changes. It opens the store with `doltlite_raw::open_curated`, as the
contacts applet opens its store, and holds it, with the file's writer
lock, from its first mute (or its first read of a store an earlier run
made) for its life. `datalib-http` only reads it.

## How a mute comes off a count

Nothing that counts problems knows about mutes. Each step still reports
`problems{severity=…}` for what it found (`etl/README.md`), and
`datalib-http` takes the mutes off as it draws the Manage rows
(`http/src/manage/problems.rs::take_off_mutes`):

1. Read the ids the mutes store has published, read-only at `main`'s
   head (`read_published`).
2. Find which of those the grid index still holds, read-only
   (`live_in_index`). The index holds every source's problems, copied
   down from its stores, so a mute whose problem was fixed is not there
   and takes nothing off.
3. Tally them by whose count they are in (`tally`;
   `datalib_problems::CountedBy`): a fetch-stage row is the source's
   `ingest` step's, the index's own row about a render store it could not
   read is `grid_index`'s, and every other row is the source's
   `render_markdown` step's. An `info` row is in no count.
4. Subtract each from its step's count. A step's count and the index
   are read at different moments (the index catches up a render and an
   index pass later), so a count stops at zero rather than going below.

So `datalib-http` reads two stores' rows here, the mutes store and the
index's `problems`; it opens both read-only and holds neither.

A mute is a commit to a store under `datalib_curated/`, which the app's
watch reports as `curated`; that, and the index moving, make the Manage
rows and an open problems grid read again.
