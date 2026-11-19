# Problems in the step protocol, read where each step keeps them

*Proposal (2026-11-16), mostly built: steps 1–5 below landed in
#1766, with the linking of a download's problem to its record (render's
`entity_items`, the reader's `datalib_step_problems::link`); the
`grid_index` inputs #1755 added are taken out by the unattended rewrite
`unread_inputs.rs`. Still to do: step 6, the roll-up through data nodes,
and step 7, the per-provider recipes for a record's upstream values.
How problems are kept now: `datalib/backend/etl/README.md` §"Problems
stay in the step that found them". It leans on `documents_to_blocks.md`,
which gives every row a place in a tree of data nodes.*

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
- **It is not part of the step protocol.** Only Datalib's own steps
  write problem rows. A package's step can send a
  `problems{severity=…}` metric, which the Manage row draws as a grey
  count that opens nothing.
- **Who found it is worked out afterwards.** The index holds rows by
  `stage`, and `CountedBy` maps a stage back to a step, so each Manage
  count is a search built from `-stage:` exclusions, with a test to
  prove the counts and searches agree. Two steps that are not renders
  needed their own stages, their own copy into the index, and config
  edges into `grid_index` that exist only to carry those rows (#1755).

What is worth keeping: a problem stays until the thing is tried again
and works; a run clears only what it re-checked; a problem found again
unchanged keeps its first-seen and changed stamps; a mute is kept by
the problem's id; and a problem links to what it is about.

## The idea

**A step's problems are part of its output, read where the step keeps
them, and never copied.** One shape, `ProblemRow`, everywhere:

- **A step with a store keeps them in that store's `problems` table**,
  as Datalib's steps already do: the download in its raw store, render
  in its render store, `render_time_series` and `render_occurrences` in
  theirs, `grid_index` its own rows in the index. That table is also
  the step's working state (a download writes a failure in the same
  transaction as the record; render skips a document it kept failed),
  so it stays where the step can use it.
- **A step with no store of its own** keeps them in `problems.sqlite`
  in its folder, plain SQLite with the same table. It writes the file
  itself (`datalib_step_problems::StepFile`), or sends `problem` and
  `covered` lines and the runner writes it for it.

A reader finds a step's problems in the stores directly in its folder:
each `.doltlite_db` read at `main`, which is what its writer last
sealed, without waiting on the writer, and each plain `.sqlite`
(`datalib_step_problems::of_step`). So a download's problem shows at
its next seal, while it runs, and render has nothing to copy.

| | today | after |
|---|---|---|
| a download can't fetch a message | shown after render and `grid_index` run | shown at the download's next seal |
| the source's render is off | never shown | shown |
| a package step's problem | grey count, opens nothing | red or yellow count, opens its rows |
| a step run by hand | rows in its store, seen by nobody until render | the same rows, beside its data |
| which step a row counts against | derived from `stage` | the step whose folder holds it |

## The lines

A step that does not write its file sends, beside `metric` and `log`
(`docs/dev/step_protocol.md` §"Problems: what you could not do"):

```json
{"event":"problem","stage":"fetch","scope_kind":"entity","scope_key":"messages:C01/1712345678.000100","outcome":"dropped","reason":"fetch_failed","sample":"HTTP 404","upstream_entity_kind":"message","upstream_id":"C01/1712345678.000100"}
{"event":"covered","kind":"records","table":"messages","all_but":["C01/1712345999.000200"]}
```

A `problem` line is a `ProblemRow` without the columns its writer
fills (its id, its source, its stamps), so its words are Datalib's
closed sets (`Reason`, `Outcome`). `scope_key` says what it is about
within the step (`<table>:<id>`, `file:<path>`, `listing:<name>`,
`phase:<name>`), and `covered` says what the run checked again: `all`,
`listings`, `records` of one table (every one but `all_but`, or
exactly `only`), or `files` under a folder. A run that ended short
clears only what it named.

**What a problem is about** is what its step held when it arose: the
upstream record, as `upstream_entity_kind`, `upstream_id` and
`upstream_account` (the names a grid row carries; three nullable
columns `ProblemRow` gains), a node's uuid in `item_uuid` when the
step had one, or a file. A step never names an id another step mints
later. The link to what a person opens is a lookup when the problem is
read, and a problem rolls up from its node to every ancestor through
the data-node tree's parent pointers.

## Who reads it

- **The Manage counts and "Needs you"**: `datalib-http` reads each
  step's problems with `of_step` (reading a store's rows is a third
  exception beside the blob store and the index), counts them by
  severity less the muted ones, and a count's double-click opens the
  problems grid on `step:<id>`. `CountedBy` and its per-stage queries
  go.
- **The problems grid** loads every step's rows into one in-memory
  table of the same shape, with the step each came from, and runs its
  search, sort, grouping and suggestions on it unchanged. A root holds
  hundreds of problems, not millions.
- **The banner** lists the problems about any node the view shows, and
  every node beneath it.

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
document row (a Beeper or Signal month) has an id Datalib composed;
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

Nothing moves: every row is already in the store of the step that
found it. `grid_index` stops copying, and the copies in the index go
with the next index build. A download's rows name no source today
(the raw store does not know its group); the download is handed its
group when its store opens, so its rows are minted under the source's
id, which is the id render's copy carried, and a mute made on a copy
holds on the row. The `grid_index` inputs #1755 added are removed by
an unattended config rewrite.

## Order of work

1. **The file and its rules** (`datalib_step_problems`): built.
2. **The protocol lines and the runner's writer**: built.
3. **The reader** (`of_step`), and the three upstream columns: built.
4. **Downloads name their source**, so their rows' ids are the ones the
   copies carried.
5. **The Manage counts, "Needs you" and the problems grid read
   `of_step`**; render stops copying download rows; `grid_index` stops
   copying; `CountedBy` goes.
6. **The banner and the roll-up**, once the data-node tree's parent
   pointers land, with the index on `(source_id, upstream_entity_kind,
   upstream_id, upstream_account)` and the query that uses it.
7. **One recipe per provider for a record's upstream values**, fixing
   the mismatches above.

## Open questions

- How many problems can one run send? A source with a broken
  credential may fail every record. Today's "N back-to-back failures
  trip a budget" rule keeps that to one `phase:` row.
- Does any search a person has typed use `stage:`? It keeps working:
  `stage` stays a column.
