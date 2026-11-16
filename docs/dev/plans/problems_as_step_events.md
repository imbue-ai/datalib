# Problems as step events, kept by the runner

*Proposal (2026-11-16). Nothing here is built. It replaces the problem
rows that each store keeps and that render and `grid_index` copy
downstream (`datalib/backend/etl/README.md` §"Problems flow downstream
with the data"). It leans on
`messages_not_documents.md` (being written beside it), which makes
the message, not the document, the unit the index stores.*

## The problem

A problem is something a step could not fully do to one record: a
message it could not fetch, a file it could not read, an event whose
occurrences it could not list. Today it travels a long way before a
person sees it:

- **It is late, and sometimes never arrives.** A download writes its
  problem into its raw store. Render copies it into the render store,
  minting it again under the source's id. `grid_index` copies it into
  the index, and only then does a Manage count or "Needs you" show it.
  A failed fetch shows two steps after the download that hit it, and
  not at all while the source's render is turned off or failing.
- **It is not part of the step protocol.** Only datalib's own steps
  write the `problems` table. A package's step can send a
  `problems{severity=…}` metric, which the Manage row draws as a grey
  count that opens nothing.
- **Who found it is worked out afterwards.** The index holds rows by
  `stage`, and `CountedBy` maps a stage back to a step, so each Manage
  count is a search built from `-stage:` exclusions, with a test to
  prove the counts and searches agree. Two steps that are not renders
  (`render_time_series`, `render_occurrences`) needed their own stages,
  their own copy into the index, and config edges into `grid_index`
  that exist only to carry those rows (#1755).
- **Every store carries a `problems` table**, and every consumer of a
  store has to copy it on.

What is worth keeping: a problem stays until the thing is tried again
and works; a run clears only what it re-checked; a problem found again
unchanged keeps its first-seen and changed stamps; a mute is kept by
the problem's id; and a problem links to the message it is about.

## The idea

**A problem is an event a step sends, like a metric; the runner keeps
it.** The runner already reads every step's events and already knows
which step it is talking to. It writes problems into one store, keyed
by the step that found them, as they arrive. A Manage count is then a
count of that step's rows, live while the step runs, for any program
that speaks the protocol.

| | today | after |
|---|---|---|
| a download can't fetch a message | shown after render and `grid_index` run | shown while the download runs |
| the source's render is off | never shown | shown |
| a package step's problem | grey count, opens nothing | red or yellow count, opens its rows |
| which step a row counts against | derived from `stage` | the step that sent it |
| a store's `problems` table | in every store, copied downstream | gone |

## The events

Two new lines on stdout (or fd 3), beside `metric` and `log`
(`docs/dev/step_protocol.md` §"stdout: the event protocol"):

```json
{"event":"problem","key":"record:messages:C01/1712345678.000100","severity":"warning","outcome":"dropped","reason":"fetch_failed","sample":"HTTP 404","about":{"kind":"message","id":"C01/1712345678.000100"}}
{"event":"covered","kind":"record","table":"messages","all":true}
```

- **`key`** names the problem within the step, and is what a later run
  sends again for the same problem. The runner mints the problem's id
  from the step id and the key, so a step never needs to know its
  source's id to name its own problem.
- **`severity`, `outcome`, `reason`** are today's closed sets
  (`datalib_problems`). A package may also send a `reason` this build
  does not know; it is kept as written and shown as text.
- **`about`** names the thing the step was handling when the problem
  arose, in the terms the step already holds. **A step never names
  something it has not seen**, such as an id another step will mint
  later. Every field is optional:
  - `kind` and `id`: the upstream record, as upstream names it (a
    Slack message's channel and timestamp, a Gmail message id). This
    is the pair every grid row already carries as
    `upstream_entity_kind` and `upstream_id`, so a download, which
    sees only upstream's record, can name it exactly.
  - `message`: the uuid of the message, for a step that wrote or read
    one (a render, a time-series render). Never a document's uuid or
    a byte range: in `messages_not_documents.md` documents become
    groupings of messages, and a message's parents (thread,
    conversation, page) are looked up from the message, never stored
    on the problem.
  - `path`: a file under the source's folder.
  - none of them: the problem is about the step's run as a whole (a
    listing or phase it could not do).
- **`covered`** says what this run re-checked, so the runner knows
  what to clear. Its kinds are today's coverage, unchanged
  (`docs/dev/data_architecture_ingestion.md` §"Error handling"): every
  listing and phase, every record of a table, every record of a table
  but a named list, every file under a folder but a named list. A step
  that finds everything again each run (a render, `render_time_series`)
  sends `{"event":"covered","kind":"all"}`.

A problem event is **never coalesced or dropped**. Metrics are
coalesced to the newest value; problems are not, and the runner writes
them in batches inside one transaction per batch.

## Clearing

At the end of a run the runner removes that step's rows that fall
inside what the run said it covered and that the run did not send
again. The rules are the ones `run_problems` applies today, moved from
the download into the runner:

- A run that **succeeds** clears within everything it covered.
- A run that **ends `incomplete`, fails or is stopped** clears only
  the record coverage it named explicitly (`all but`) before ending;
  listing, phase and `all` coverage clear nothing, since it is not
  known that every one of them ran.
- A row sent again keeps its first-seen stamp; its changed stamp moves
  only when its severity, outcome, reason or sample changed
  (`ProblemRow::stamped`).
- A step removed from the config has its rows removed on the next
  config load. A step turned off keeps its rows.

## The store

`system/problems.sqlite`: plain SQLite in WAL mode, not doltlite. It
needs no history: a problem describes the latest attempt, not a
commit. Its one writer is the process holding `runner-lock`
(`datalib-http`'s supervisor, or `datalib-dag` run by hand), which is
already the one writer of `system/supervisor.sqlite`. Readers open it
read-only.

One table, one row per `(step_id, key)`:

| column | what |
|---|---|
| `problem_uuid` | minted from `step_id` and `key` |
| `step_id`, `group_id` | who found it; the group is the source |
| `key` | the step's own name for it |
| `severity`, `outcome`, `reason`, `sample` | as sent |
| `about_kind`, `about_id`, `about_message`, `about_path` | as sent |
| `first_seen_at_utc`, `changed_at_utc`, `tz_offset` | stamps (§"Timestamp convention" in `AGENTS.md`) |
| `run_id` | the run that last sent it, for the run's log |

Mutes stay where they are (`datalib_curated/`, by problem id), and so
does the rule that a muted row counts nowhere.

## Who reads it

- **The Manage counts and "Needs you"**: `datalib-http` counts rows by
  `step_id` and severity, less the muted ones. A count's double-click
  opens the problems grid on `step:<id>`. `CountedBy`, the per-stage
  counter queries and their guarantee test go away.
- **The problems grid** (the `unified_document_index` applet) reads
  the store read-only. To link a row to what a person can open it
  looks up the message it is about (§"From what the step held to what
  a person opens"); a message the index does not hold yet shows
  unlinked, and links once it does.
- **The document banner** lists the problems about any message the view
  shows, for whatever grouping holds them (thread, conversation,
  page).

## From what the step held to what a person opens

A problem names what its step was holding; the link to what a person
opens is a lookup when the problem is read, never something a step
fills in. Today render fills `item_uuid` on a download's problem
through `RenderProcessor::item_of_entity`, which only five providers
implement, and that is why a download's problem waits for render.

After, the problems grid and the banner join a problem to the
message whose grid row has the same source, `upstream_entity_kind` and
`upstream_id`, or whose uuid is the problem's `message`. A problem
whose record is not in the index yet (a message that could not be
fetched at all) is shown at once, unlinked, and links when a later
run brings the message in. `item_of_entity` and `upstream_of_entity`
go.

## Forward path for existing data

A root that works today has problem rows in its raw stores, render
stores, time-series and occurrences stores and the index, and mutes
keyed by today's ids.

- **Rows.** The launch's migrate pass reads every store's `problems`
  table once and writes each row into `system/problems.sqlite` under
  the step that found it (the last use of today's stage-to-step map),
  with its stamps. A failed fetch is not tried again until its record
  is owed again, so these rows cannot simply be left for the next run
  to find. The `problems` tables are then left alone, and a later
  release drops them from the DDL.
- **Mutes.** The same pass writes a mute for each new id whose old id
  was muted.
- **Config.** The `grid_index` inputs #1755 added carry no data once
  the rows stop flowing through `grid_index`. An unattended rewrite in
  `datalib_migrate_config` removes them, so `grid_index` stops
  re-running whenever a time-series render does.

## Order of work

1. The events and the store: the runner parses `problem` and
   `covered`, writes the table, clears at the end of a run. Tested
   with a shell-script step, as the protocol's other events are.
2. The Manage counts and "Needs you" read the store. The grey
   metric-only count goes.
3. `datalib-step`'s steps send events instead of writing tables:
   `run_problems` and the render's `problems` become senders,
   `step_problems.rs` goes.
4. The problems grid and the banner read the store, linking through
   the message.
5. The migrate pass, the mute rewrite and the config rewrite.
6. Remove the copies: render's re-mint, `grid_index`'s
   `replace_source_problems` and `copy_own_store_problems`,
   `Stage::OWN_STORE`, `CountedBy`, and the docs that describe them.

Steps 1 and 2 are useful alone: a package step gets real counts at
once.

## Open questions

- Is a message's `data-section-uuid` always its grid row's `uuid`, in
  every provider? `messages_not_documents.md`'s survey is checking.
  The banner's lookup depends on it.
- Does every grid row carry `upstream_entity_kind` and
  `upstream_id`, and does a download always know the same pair for
  the record it failed on? Where it does not, that provider's
  problems show unlinked until it does.
- How many problems can one run send? A source with a broken
  credential may fail every record. Today's
  "N back-to-back failures trip a budget" rule keeps that to one
  `phase:` row; the runner may also want a cap that turns the
  overflow into one row saying how many it did not keep.
- `stage` becomes a property of the step (download, render, index)
  rather than of the row. Does any search a person has typed use
  `stage:`? If so it keeps working as an alias over the step's
  function.
