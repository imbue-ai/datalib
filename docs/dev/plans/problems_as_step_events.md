# Problems in the step protocol, kept beside each step's data

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

**A step's problems are part of its output, in a file the step
protocol defines, and are read from there while the step runs.** Each
step keeps its problems in `problems.sqlite` in its own folder
(`<data root>/<group>/<function>/`). It has one writer, which is one of
two:

- **The step itself**, which is how every `datalib-step` function
  does it, through one shared module that applies the rules below.
  This is the same under the runner and in a hand run
  (`step_protocol.md` §"Running `datalib-step` by hand"): an app that
  embeds one download finds the problems beside the data, with the
  same stamps and clearing as the app would get.
- **The runner, on the step's behalf**, for a program that would
  rather not write SQLite. It sends `problem` and `covered` events
  (below), and the runner applies them to that step's file.

A step does one or the other, never both, so a file never has two
writers. The runner learns which from the step's `problems` line
(§"The events"). A program that leaves the writing to the runner and
is run by hand prints its problems and keeps none; that is the price
of not writing the file, and the protocol says so.

Nothing copies the file. `datalib-http` reads it live for the Manage
counts and "Needs you", and the problems grid reads it for its rows. A
step's file holds only that step's rows, so who found a problem needs
no working out.

| | today | after |
|---|---|---|
| a download can't fetch a message | shown after render and `grid_index` run | shown while the download runs |
| the source's render is off | never shown | shown |
| a package step's problem | grey count, opens nothing | red or yellow count, opens its rows |
| a step run by hand | rows in its store, seen by nobody until render | the same file, beside its data |
| which step a row counts against | derived from `stage` | the step whose file holds it |
| a store's `problems` table | in every store, copied downstream | gone |

## The events

What a row says is the same whichever side writes it, so it is
described here as the events a step sends. Three new lines on stdout
(or fd 3), beside `metric` and `log`
(`docs/dev/step_protocol.md` §"stdout: the event protocol"):

```json
{"event":"problems","writer":"runner"}
{"event":"problem","key":"record:messages:C01/1712345678.000100","severity":"warning","outcome":"dropped","reason":"fetch_failed","sample":"HTTP 404","about":{"kind":"message","id":"C01/1712345678.000100"}}
{"event":"covered","kind":"record","table":"messages","all":true}
```

- **`problems`** comes before any `problem` line and says who writes
  this step's file: `runner`, or `step` for a step that writes it
  itself and so sends no `problem` lines. A step that sends neither
  writes no problems, and its file is left as it is.
- **`key`** names the problem within the step, and is what a later run
  sends again for the same problem. The problem's id is minted from the
  step id and the key, so a step never needs to know its source's id
  to name its own problem.
- **`severity`, `outcome`, `reason`** are today's closed sets
  (`datalib_problems`). A package may also send a `reason` this build
  does not know; it is kept as written and shown as text.
- **`about`** names the thing the step was handling when the problem
  arose, in the terms the step already holds. **A step never names
  something it has not seen**, such as an id another step will mint
  later. Every field is optional:
  - `kind`, `id` and `account`: the upstream record, as upstream names
    it (a Slack message's channel and timestamp, a Gmail message id).
    These are the three values every grid row already carries as
    `upstream_entity_kind`, `upstream_id` and `upstream_account`, so a
    download, which sees only upstream's record, can name it exactly
    — provided both sides spell it the same way (§"What the tree does
    today").
  - `message`: the uuid of the message, for a step that wrote or read
    one (a render, a time-series render). Never a document's uuid or
    a byte range: in `messages_not_documents.md` documents become
    groupings of messages, and a message's parents (thread,
    conversation, page) are looked up from the message, never stored
    on the problem.
  - `path`: a file under the source's folder.
  - none of them: the problem is about the step's run as a whole (a
    listing or phase it could not do).
- **`covered`** says what this run re-checked, so the writer knows
  what to clear. Its kinds are today's coverage, unchanged
  (`docs/dev/data_architecture_ingestion.md` §"Error handling"): every
  listing and phase, every record of a table, every record of a table
  but a named list, every file under a folder but a named list. A step
  that finds everything again each run (a render, `render_time_series`)
  sends `{"event":"covered","kind":"all"}`.

A problem event is **never coalesced or dropped**. Metrics are
coalesced to the newest value; problems are not, and the runner writes
them in batches inside one transaction per batch. A step that writes
its own file does the same, so a reader sees each batch whole.

## Clearing

At the end of a run the writer removes the rows that fall inside what
the run said it covered and that the run did not find again. The rules
are the ones `run_problems` applies today, moved into the one shared
module that both the runner and `datalib-step` call:

- A run that **succeeds** clears within everything it covered.
- A run that **ends `incomplete`, fails or is stopped** clears only
  the record coverage it named explicitly (`all but`) before ending;
  listing, phase and `all` coverage clear nothing, since it is not
  known that every one of them ran.
- A row sent again keeps its first-seen stamp; its changed stamp moves
  only when its severity, outcome, reason or sample changed
  (`ProblemRow::stamped`).
- A step turned off keeps its file. A step removed from the config,
  or reset (`datalib-dag --reset`), loses its folder and the file with
  it. A reset downloads everything again, so a problem that still
  holds comes back under the same id, and its mute holds again; only
  its first-seen stamp starts over.

## The file

`<group>/<function>/problems.sqlite`: plain SQLite in WAL mode, not
doltlite, so it can be read while it is written. It needs no history:
a problem describes the latest attempt, not a commit. Its one writer is
the step or the runner on its behalf (§"The idea"); readers open it
read-only.

One table, one row per `key`:

| column | what |
|---|---|
| `problem_uuid` | minted from the step id and `key` |
| `key` | the step's own name for it |
| `severity`, `outcome`, `reason`, `sample` | as written |
| `about_kind`, `about_id`, `about_account`, `about_message`, `about_path` | as written |
| `first_seen_at_utc`, `changed_at_utc`, `tz_offset` | stamps (§"Timestamp convention" in `AGENTS.md`) |
| `run_id` | the run that last wrote it, for the run's log; empty in a hand run |

The step and its group are the folder, so no column repeats them.
Mutes stay where they are (`datalib_curated/`, by problem id), and so
does the rule that a muted row counts nowhere.

## Who reads it

- **The Manage counts and "Needs you"**: `datalib-http` counts each
  step's file by severity, less the muted rows. That is one small open
  per step, a few dozen on a large root, and the answer can be kept
  until a file's modification time moves. A count's double-click opens
  the problems grid on `step:<id>`. `CountedBy`, the per-stage counter
  queries and their guarantee test go away.
- **The problems grid** (the `unified_document_index` applet) reads
  every step's file and merges the rows in memory. A root holds
  hundreds of problems, not millions, so this is cheap and the grid is
  as live as the counts. It is not one SQL query over attached files:
  SQLite attaches ten files to a connection by default. If the merge is
  ever measured slow, a copy into the index for the grid alone is the
  fallback, while the counts keep reading the files. To link a row to
  what a person can open, the grid looks up the message it is about
  (§"From what the step held to what a person opens"); a message the
  index does not hold yet shows unlinked, and links once it does.
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

## What the tree does today

Surveyed 2026-11-16 across every render crate and the downloads'
failure keys. Two questions: is a message's `data-section-uuid` its
grid row's `uuid`, and does a download name a failed record with the
same upstream values the record's row carries?

**Section uuids: yes, nearly everywhere.** Every `chat-common` source
writes the message's uuid on both. So do Garmin, the time-series
pages, Google Drive comments, Notion comments, Todoist, LinkedIn
skills and PDF pages. The exceptions, none of which a problem needs:

- GitHub and GitLab key the description's section with the document
  row's uuid; each comment's section is its own row's.
- Calendar, contacts and Lightroom write no message wrapper; their one
  section is the event's, contact's or photo's row.
- Notion's table-of-contents spans and Perseus's sentence spans have
  no rows; they anchor links only.

**Upstream values on the row: yes, but contacts.** Every provider
fills `upstream_entity_kind` and `upstream_id`, except that a contact
with an empty key gets no `upstream_id`. A time-bucketed chat's
document row (a Beeper or Signal month) has an id datalib composed;
its messages' ids are upstream's.

**A download's failure key: rarely the row's values.** Today a failed
record is named `<table>:<id>` (`record_object_error`) or
`record:<table>:<id>` (`RecordProblem`): a raw table name, never the
row's `upstream_entity_kind`, and the id is the raw store's key, which
often is not spelled as render spells it:

| source | download names | the row says | |
|---|---|---|---|
| ChatGPT, Claude, Google Drive, Notion pages, Apple Mail, Takeout Maps photos | the upstream id | the same id | match, given the table → kind name |
| Slack | `{team}#{channel}#{ts}` | `{channel}#{ts}`, team in `upstream_account` | split differently |
| GitLab | `{path}!{iid}` | `{path}#{iid}` | spelled differently |
| Gmail | upstream's hex id | 16 digits, zero-padded, lowercase | normalised differently |
| Calendly | the event id | `{calendar}#{event}` | composed differently |
| PDF | the file's path | the file's blake3 | a different thing |
| Claude Code, Codex | the transcript's path | the session id | a different thing |
| Beeper, Signal, WhatsApp, AirVisual, Lightroom | a media file, snapshot or backup | no row at all | names a file |

So the plan adds one rule: **a record's upstream values are spelled
by one recipe in the provider's ingest crate, and both the download's
problem and the render's row call it.** The recipes already belong on
the ingest side (`AGENTS.md` §"Ingest and render are separate
crates"). The rows of the table above that say "spelled",
"normalised" or "composed differently" each move their render onto
the download's recipe or the reverse; which way is the one that keeps
the row's uuid, since a uuid is minted from these values and moving
it is a re-key. A problem about a file names it by `path`, and links
to nothing until something shows files. YoLink's downloads, Garmin's
day and activity keys, Takeout's other modules and Claude's project
docs were not read.

## Forward path for existing data

A root that works today has problem rows in its raw stores, render
stores, time-series and occurrences stores and the index, and mutes
keyed by today's ids.

- **Rows.** The launch's migrate pass reads every store's `problems`
  table once and writes each row into the file of the step that found
  it (the last use of today's stage-to-step map), with its stamps. A failed fetch is not tried again until its record
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

`messages_not_documents.md` rewrites render and `grid_index` and adds
the conversation and thread rows a problem links through. The steps
below that touch neither go first; the rest wait for it.

Now:

1. **The file and the shared module**: the table, the clearing rules,
   the stamps. Tested on its own, on made-up rows.
2. **The protocol**: the runner parses `problems`, `problem` and
   `covered` and writes the file for a step that asks it to. Tested
   with a shell-script step, as the protocol's other events are.
3. **The Manage counts and "Needs you"** read the files. The grey
   metric-only count goes.
4. **Downloads write their own file.** `run_problems` writes through
   the shared module instead of the raw store's table, and in the same
   change render stops copying the raw store's fetch-stage rows, so no
   problem is counted twice. A download's problem shows while it runs.
5. **One recipe per provider for a record's upstream values**, fixing
   the mismatches in §"What the tree does today". This is ingest-side,
   and `messages_not_documents.md` needs it too.

After `messages_not_documents.md` has its conversation and thread rows
and its text in the render store:

6. **Render, `render_time_series` and `render_occurrences` write their
   own file**; `step_problems.rs` goes.
7. **The problems grid and the banner read the files**, linking
   through the message.
8. **The migrate pass**, the mute rewrite and the config rewrite.
9. **Remove the copies**: `grid_index`'s `replace_source_problems` and
   `copy_own_store_problems`, `Stage::OWN_STORE`, `CountedBy`,
   `item_of_entity`, and the docs that describe them.

Steps 1 to 3 are useful alone: a package step gets real counts at
once.

## Open questions

- How many problems can one run send? A source with a broken
  credential may fail every record. Today's
  "N back-to-back failures trip a budget" rule keeps that to one
  `phase:` row; the shared module may also want a cap that turns the
  overflow into one row saying how many it did not keep.
- `stage` becomes a property of the step (download, render, index)
  rather than of the row. Does any search a person has typed use
  `stage:`? If so it keeps working as an alias over the step's
  function.
