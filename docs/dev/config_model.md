# The config model: groups, steps and functions

What a `config.toml` is made of, the naming rules, how an ingest step
says where its data comes from, and which parts of the tree read
each of those. The runner's own view — what it fingerprints, what it
forwards, what a bad entry costs — is
[`datalib/backend/dag/README.md`](../../datalib/backend/dag/README.md);
the contract a step command implements is
[`step_protocol.md`](step_protocol.md).

## The kinds of entry

```toml
[[programs]]
command = "datalib-step"
verbs = ["reset", "prepare", "compact", "collect"]

[[locks]]
name = "index"
slots = 2

[[groups]]
id = "work-slack"          # the directory; permanent
name = "Work Slack"        # free text; nothing depends on it
type = "slack"             # what is mirrored — makes this a source

[[steps]]
id = "work-slack/ingest"   # its group is the first part
command = "datalib-step run ingest slack"
[steps.params.api]         # the method: Slack's own API
channels = ["chat-qi"]

[[steps]]
id = "work-slack/render_markdown"
command = "datalib-step run render_markdown slack"
inputs = ["work-slack/ingest"]
reads = "pinned"

[[groups]]
id = "unified_document_index"       # no type: it mirrors nothing

[[steps]]
id = "unified_document_index/grid_index"
command = "datalib-step run grid_index"
inputs = ["work-slack/render_markdown"]
reads = "pinned"
locks = ["index"]

[[applets]]
group = "unified_document_index"
id = "unified_document_index"
command = "datalib-applet unified_document_index"
```

**A group** is one thing on the Manage screen: an `id`, an optional
`name`, an optional `type`, an optional `description`. A source is a
group with a `type`; the unified document index is a group without one.
None of the four is handed to a step or fingerprinted, so editing
`name` or `description` re-runs nothing. The runner never reads `type`:
the Manage screen and the wizard do, and `datalib-step` is told it on
its command line.

**A step** is an `id` and a `command`. The id is the tree the step
writes, the key its state is kept under, and what another step's
`inputs` name; its first `/`-separated part is its group. The command
is required, and is everything the program needs to know about what to
do. Datalib's own steps run `datalib-step`, the step program it ships,
as `datalib-step run <function> [<type>]`: `<function>` is what the
step does (`ingest`, `render_markdown`, …), and `<type>` is its group's
`type`, written for every step under a typed group. Such a step's id
must be `<group>/<function>`. Another program may write any id and any
command; a step whose id's first part names no `[[groups]]` entry is a
step outside any group.

A step of `datalib-step` also writes the `reads` and `locks` it needs,
since the runner infers nothing from the program's name:

- **`reads`.** `ingest` writes none, since it reads nothing.
  `keyword_index`, `embed`, `embedding_map` and perseus's
  `render_markdown` read their inputs off disk and write
  `reads = "files"`. Every other step writes `reads = "pinned"`, even
  while its `inputs` are empty (a fan-in's are, until a source is
  added).
- **`locks`.** `qmd_aggregator` and `keyword_index` write
  `locks = ["qmd_keyword"]`, `embed` writes `locks = ["qmd_embed"]`,
  and every other step under a group with no `type` (`grid_index`,
  `embedding_map`, `time_series_index`) writes `locks = ["index"]`. A
  source's `ingest` and renders write none, and hold the runner's
  default.

The file then declares those three locks in `[[locks]]` entries
(`qmd_keyword` and `qmd_embed` with 1 slot, `index` with 2), since a
step that holds an undeclared lock is dropped, and lists the verbs
`datalib-step` takes in a `[[programs]]` entry. `datalib-step`'s own
code for all of this is `datalib_step/src/step_entry.rs`, which the
examples, `examples --append` and `check-params` share.

`[env]` is a table of variables handed to every step, below each
step's own `env`. `datalib-step` reads three knobs from it:
`DATALIB_STEP_CHECKPOINT_CADENCE`, `DATALIB_STEP_RENDER_PAGES` and
`DATALIB_STEP_EMBED_CONTEXTS` (`step_protocol.md` § "How
`datalib-step` fits").

**An applet** is a server the http gateway spawns on demand. Its `id`
is the mount prefix and a JavaScript identifier, unique on its own and
not composed; `group` on an applet only says which Manage row it is
filed under.

**A diff group** is a group of `type = "diff"`: it mirrors nothing
itself and renders what changed in another source's raw store between
two commits, as documents with the changes marked and `grid_rows` with
a `diff_status`. Its render step runs
`datalib-step run render_markdown diff --source-type <type>`, where
`<type>` is the type of the source it compares, and reads that
source's `<source>/ingest` as its first input. The two commits are
under `params.diff` (`from` and `to`, both required, and
`max_documents`, the most documents a side may render before the step
fails, default 1000); the rest of `params` is the source type's render
config. It may also carry a `keyword_index` and an `embed`, like a
source. The fan-ins name it like any render step.
[`plans/completed/diff_renderer.md`](plans/completed/diff_renderer.md)
has the design; `configs/dag_example.toml` has one.

Its tree is worth nothing kept: both commits stay in the source's store,
so comparing them again rebuilds it. Removing a diff group on the Manage
screen therefore offers, checked by default, to delete the tree too —
`POST /api/purge` with the group ids, once they are out of the config.
The loop stops the group's steps if they run, deletes `<root>/<group>/`
and forgets the group's steps in its record, without waiting for a sync
to end (`datalib/backend/dag/README.md` §"Resets and purges"); without that, a group re-added under the
same id and definition would read as up to date and write nothing.

A `[[packages]]` table lists the packages the library uses,
each pinned by its manifest's hash. A step or applet runs one of a
package's programs by its path under the data root, in its `command`;
`packages.md` is the rest.

`configs/dag_example.toml` is the commented, complete version;
`docs/user/config_examples/` has the shapes people start from.

## Naming rules

- A group id, and each `/`-separated part of a step id, is one
  portable filename segment: lowercase letters, digits, `.`, `_`, `-`,
  not starting with `-`, never `.` or `..`. Lower case because each
  names a folder and
  a Mac's disk does not tell `Email/` from `email/`: with one spelling
  per folder, no two ids name one folder on any platform. An applet id
  is a lowercase JavaScript identifier for the same reason (it names
  `system/frontend/<id>/`).
- `system` is reserved for the runner's and server's own state, and
  `datalib_curated` for what a person curates by hand; no step id
  starts with either.
- Step ids are unique and never nested: two steps under one tree would
  be two writers on one doltlite file.
- `datalib-step`'s functions are the last part of their step's id:
  `ingest`,
  `render_markdown`, `render_time_series`, `render_occurrences`,
  `keyword_index`, `embed`, `grid_index`, `qmd_aggregator`,
  `embedding_map`, `time_series_index`
  (`datalib_step/src/function.rs`, tested against the layout constants
  the render and index crates use). `grid_index`, `qmd_aggregator` and
  `embedding_map` write `unified_document_index/<function>`, and
  `time_series_index` writes `unified_time_series_index/time_series_index`,
  and nothing else, because the applet that reads each finds it from the
  data root alone.
- A source whose type has time series (`SourceType::renders_time_series`
  in `datalib_step/src/source_type.rs`: `garmin`, `yolink`,
  `airvisual`) has a `render_time_series` step beside
  its `render_markdown`, reading its ingest, and the
  `time_series_index` names every such step in its `inputs`.
  `docs/dev/time_series.md` is the design.
- A `calendar` source may have a `render_occurrences` step beside its
  `render_markdown`, reading its ingest: every occurrence of every event
  in a window of days around the run's now, one row each, the way
  Google's `singleEvents=true` lists them. Its params are `days_back`
  (30) and `days_ahead` (400). Nothing reads it but whoever asks, so it
  joins no index. The rows and their columns:
  `datalib/backend/etl/providers/calendar_render/src/occurrences/README.md`.
- The qmd steps are the exception to "a step writes its own tree": a
  source's `keyword_index` and `embed`, and `qmd_aggregator`, all write
  the one qmd index file in `unified_document_index/qmd_aggregator`, the
  aggregator's tree, each source's steps to its own collection. The
  runner cannot see that file as shared, so they hold one-slot locks
  instead (below).
- A group's `type` names the thing mirrored, never the way it is
  reached — `claude` over the API or from an export, `contacts` over
  CardDAV or from `.vcf` files — and the product a person recognizes,
  not the vendor. Where a `grid_rows.provider` tag exists for a type,
  the two spell it the same way. `SourceType` in
  `datalib_step/src/source_type.rs` is the closed list.

## Where the data comes from: the method table

An `ingest` step's params hold one table per method, and the table's
name is read *under the type*: a product has one API, so its table is
just `api`, and a type that is not one product qualifies its sources
(email's `jmap`, `gmail`, `mbox`). A table that reads files carries
its own `path`. There is no global vocabulary — each provider's
`<p>_config` crate declares its own two or three:

| type | methods |
|---|---|
| garmin, github, gitlab, google_drive, linear, notion, slack, todoist, yolink | `api` |
| chatgpt, claude | `api`, `export` |
| contacts | `carddav`, `vcf` |
| email | `jmap`, `gmail`, `mbox` |
| calendar | `google`, `fastmail`, `caldav`, `ics` |
| lightroom | `catalog`, `backups` |
| airvisual, facebook, google_takeout, linkedin | `export` |
| signal, sms_backup_restore, whatsapp | `backup` |
| file_inventory, media, pdf | `fswalk` |
| beeper `texts` · apple_messages `database` · apple_photos `library` · claude_code `sessions` · codex `sessions` · perseus `github` | |

That table is `ui/src/config/ingestMethods.json`, the one place to
read the list from.

A method is *held* when its path is written and its value is neither
`null` nor `false`: a table counts by presence (`api = {}` is a
complete selection). A provider with more than one table refuses a step
naming two (its config's `validate`, e.g. `claude_config`'s), so a
store is filled one way. Three pairs are the exception, each a past
and a present of one thing, so a step may name both and the present
lands on top: `lightroom`'s `backups` folder and its `catalog`; email's
`mbox` beside `gmail`, a Takeout of the account the API starts from
(`email_download_modes.md` §"Starting from a Takeout"); and
`chatgpt`'s `export` beside `api`, which fills in the conversations
the store has never held before the API runs (the provider's
`INGEST.md` §"An export and the API together").

Knobs that apply whatever the method (email's `only_extract_labels`,
the `common.*` envelope) stay at the top level of `params`. Render
knobs go on the render step, whose config is strict: download-shaped
params on a `render_markdown` step are refused.

## Time windows: `since` and `until`

A method that can mirror only part of a source's history takes
`since`, the oldest thing to mirror; a calendar and Garmin, whose days
run forward, also take `until`. Both are keys of the method table, and
every one reads the same grammar, `datalib_time::TimeBound`
(`datalib/backend/time/src/window.rs`):

| written | means |
|---|---|
| `2026-09-01` | that day; where an instant is wanted, its midnight in the run's offset (the machine's local one), so an hour off across a daylight-saving change |
| `2026-09-01T09:00:00-07:00` | that instant (RFC 3339, offset required); where a day is wanted, its date in its own offset |
| `-30d`, `+21d`, `-12h`, `-2w` | that far from the run's `now` (`DATALIB_DAG_NOW`), sign required, in hours, days or weeks |

A day or an offset is resolved once per run, against the run's `now`,
and the download is handed an absolute value; so a relative window
slides forward run by run. A day before 1970 reaches back past
anything a source holds: Slack and Gmail, whose services count seconds
from 1970, ask from 1970 itself rather than from a negative time. A config's `validate` reads
every bound and refuses an `until` before its `since` wherever neither
the run's clock nor its offset is needed to tell, so `datalib-dag
--check` names the key; a day against an offset or an instant is
checked when a run resolves them. A method that cannot
honor `until` has no such key, and naming one is refused as an unknown
field.

Leaving `since` out means everything, except where the table below
says otherwise; Garmin's year is there because it costs one request per
metric per day.

| method | keys, and unset | what the window bounds | a record the window leaves behind |
|---|---|---|---|
| slack `api` | `since`: everything | a message's time | stays |
| claude `api`, chatgpt `api` | `since`: everything | a conversation's last update | stays |
| email `gmail` | `since`: everything | `after:` on the listing walk | stays |
| todoist `api` | `since`: the day the account joined | a task's completion time | stays |
| garmin `api` | `since`: a year before the first run, recorded; `until`: today | days | stays |
| calendar `google`, `fastmail`, `caldav` | `since`, `until`: no bound | events with any part inside | **dropped**: a windowed calendar is listed whole every run, and an event the listing no longer names goes (`calendar/INGEST.md` §"A window") |

An earlier `since` backfills on the next run everywhere: each source
owes what its listing now names and its store does not hold
(`data_architecture_ingestion.md` §"What is left to fetch"). YoLink's
per-device `start` predates the grammar and still takes a day only.

**Several things, each with its own window: `fetch`.** A method whose
source has parts worth fetching to different depths takes repeated
`fetch` sections, each saying what it selects, in that source's own words,
and how far back:

```toml fragment
[[steps.params.gmail.fetch]]
labels = ["Inbox"]

[[steps.params.gmail.fetch]]
labels = ["Sent"]
since = "-90d"
```

Whatever the source, the sections mean the same:

- **What is fetched is the union.** Sections only add, so their order says
  nothing and a new one can never lose what another reaches.
- **A thing two sections select gets the wider window**, and a section with
  no `since` is the widest (`datalib_time::widest_start`).
- **A section selecting nothing named takes everything** within its window.
  A name upstream does not have selects nothing and is a `config:` problem,
  and never reads as that.
- **`fetch` replaces the method's own window and selection keys**, and the
  older keys are read as one section. A config with both is refused, since
  which of the two won would show only in what the mirror holds.

Only Gmail has it today (`labels`, `since`, `whole_threads`;
[`getting_your_data.md`](../user/getting_your_data.md) § "Different labels to
different depths" is the person's version). A new source's sections would
select in its own words, as Slack's could by `channels` and `dms`.

**No wizard form writes `fetch`.** A form's fields are single settings, and
a repeated table is none. The config editor is the way in. Reopening such a
source in its form lists `fetch` among the keys it keeps
(`untouchedSettings`), but its answers read as if there were no filter, and
choosing labels there writes the older keys beside `fetch`, which the load
refuses. `datalib/ui/tests/edit_source.test.ts` pins the keeping.

The window is a key of each method rather than one range under
`[steps.params.common]`: `common` is per step too, so it would save no
typing, and two places to say one thing would need a rule for which
wins. Kept per method, what a source cannot honor is a missing key,
which the schema and the field reference already say.

## Examples and the field reference

Each config crate carries its own stanzas, `EXAMPLES`: a minimal and a
complete one per ingest method, each a `[[groups]]` entry and its steps
under the type's own name as the group id. Its structs derive
`schemars::JsonSchema`, so the params' JSON Schema, doc comments
included, is the field reference. `datalib_step/src/source_docs.rs`
maps every `SourceType` to the two; `datalib-step examples <type>`
prints the stanzas and `datalib-step help <type> <method>` the
reference. A minimal stanza holds its method alone; a complete one may
also hold the method it pairs with (Gmail's shows its Takeout). Its
test loads and plans every stanza and holds each complete one to every
option of its method's table; the shared
`common` and `latchkey_settings` options are set only where a source
reads them. A retired field a planner refuses stays readable by serde
and leaves the schema with `#[schemars(skip)]`.

`datalib-step check-params <config>` plans the params of every step
that runs `datalib-step run` the way a sync would, and prints one JSON
line per step it would refuse. The server links no provider, so it asks:
`PUT /api/config` and `POST /api/config/check`
(`http/src/params_check.rs`) refuse to write a config a sync would
refuse a step of, and show each answer as an ordinary `rejected`
diagnostic at the misspelled key's line. `datalib-dag --check` does not
ask: the runner knows no program, so it checks only what the runner
itself reads. Another program's params are never asked about.

## `Reach`: origin or local

Every method a provider accepts declares itself `Origin` (fetches from
a live service) or `Local` (reads files already on this machine).
The enum is `Reach` and the declaration is `impl IngestMethods for
<P>Config` — a list of `IngestMethod { path, reach }` — both in
`datalib/backend/source_common/src/lib.rs`. `datalib_step/src/methods.rs`
maps a type to its list and applies the held rule; a step's reach is
`Origin` if anything held reaches one, else `Local`.

The UI reads the same declarations without a hand-kept copy:
`ui/src/config/ingestMethods.json` is generated by
`bazel run //datalib/backend/datalib_step:ingest_methods.update`, a
test in `methods.rs` fails when it drifts, and
`ui/src/config/ingestMethods.ts` applies the same held rule
(`methodsHeld`, `ingestReach`, `ingestLabel`). Two things read it:

- **The Manage row's label.** The server labels an ingest step
  "Ingest" (`http/src/manage/mod.rs::child_label`); the browser
  replaces that with "Download" for an origin method or "Import" for a
  local one (`cards/SourcesCard.ce.vue`), off the step's written params.
- **The wizard's account row.** A descriptor with a
  `credentialService` shows its latchkey controls only while the
  params the form would write reach an origin: an import has nothing
  to log in to (`SourceWizard.vue`).

`datalib-dag --reset` is *not* gated on it: it empties the step's store
whatever the method, and the next sync reads its files or its origin in
full — the only "from scratch" button a user has. `datalib-step`'s
ingest driver only logs the reach.

## What the loader checks

The rules sit in `datalib/backend/dag/src/config.rs` (`accept_groups`,
`accept_steps`, `accept_programs`, `accept_applets`) and `graph.rs`;
`diagnostics.rs` explains the severities. A bad entry is dropped and
the rest of the file runs.

| the entry | is dropped when |
|---|---|
| the whole file (`Fatal`) | it is not TOML; nothing loads |
| any entry | it does not deserialize: an unknown key, a wrong type, a missing required key |
| a top-level key | it is `checkpoint_cadence`, `render_pages` or `embed_contexts`, the shape from before `[env]` (the steps still load); an `[env]` name is not a variable name |
| a lock | its name is not one segment, it has no slots, or it is declared twice |
| a program | its `command` is more than one word, or it is declared twice |
| a group | its id is not one segment, is reserved, or is a duplicate; it carries `source`, the shape from before a diff group's steps named their source's type |
| a step | it writes `group` or `function`, or has no `command`, the shape from before every step wrote its id and its whole command; a part of its id is not one segment; its id is under `system` or `datalib_curated`, duplicated, or nested with another; its `command` is empty or has unbalanced quoting; its program is in a package directory no `[[packages]]` entry pins; its params are over 64 KiB as JSON; it holds a lock no `[[locks]]` entry declares; an input names itself |
| a step (blocked: fix elsewhere) | its group or an input was itself dropped; an input names no step; it sits on a cycle or downstream of one |
| an applet | its id is not a JS identifier, is `user`, or is a duplicate; its `command` is empty |

A refusal of a retired shape names the rewrite: the app makes it when
it next reads the file, and `datalib-migrate-config` from a terminal
(§"The retired shapes").

Warnings: a group with nothing filed under it, and an applet filed
under an undeclared group. The Manage screen's System row counts the
config's warnings and names them on hover; a double-click opens the
config.

`datalib-step` drops a few keys from its params before parsing them,
since they no longer do anything, and the step still runs: an ingest's
`common.always_clear_before_ingest`, linkedin's `export.fetch_photos`
(linkedin.com no longer shows a profile to anyone signed out) and
email's `gmail.request_concurrency`.

## What `datalib-step` refuses

The runner hands every step `DATALIB_DAG_STEP` (its id) and, when it
has params, `DATALIB_DAG_PARAMS`; nothing about its group. `datalib-step`
reads its function and its group's type off its command line
(`src/source.rs`, `main.rs`, `dispatch.rs`, `methods.rs`), and refuses,
naming the fix:

- a command with no `run <function>`, or a function it does not perform;
- a step id that is not `<group>/<function>`;
- `ingest` or a render with no `<type>` on its command line;
- `--source-type` on a step whose type is not `diff`, or a diff
  group's render without it;
- `render_time_series` of a type with no time series;
- `render_occurrences` of a type that is not `calendar`, or with a
  param other than `days_back` and `days_ahead`;
- a `type` it has no provider for — a retired spelling (`*_api`,
  `*_backup`, `claude_export`, `carddav`) names `datalib-migrate-config`;
- params still carrying `sync`, `common.input_path`, `common.raw_path`,
  `gmail_api` or a top-level `fetch_photos`, likewise naming the tool;
- an `ingest` step whose params hold none of its type's methods;
- `grid_index`, `qmd_aggregator` or `embedding_map` with any id but
  `unified_document_index/<function>`, and `time_series_index` with any
  but `unified_time_series_index/time_series_index`.

A render reads its raw store from its first input; one that declares
none reads its own group's `ingest` tree and logs a warning. A store
kept on another disk is a symlink at `<group>/ingest` — no params key
can point elsewhere.

## What keeps steps apart: `[[locks]]`, `locks`, `reads`, `start`

A `[[locks]]` entry names a lock and its `slots`; a step's `locks` names
the ones it holds (`["q"]` takes one slot, `{ q = "exclusive" }` all of
them), and one that names none holds `network` (no inputs) or `cpu`
(inputs), the two budgets every config has. A datalib config declares
three more for `datalib-step`'s steps (§"The kinds of entry"): the
one-slot `qmd_keyword`, held by `qmd_aggregator` and every
`keyword_index`, the one-slot `qmd_embed`, held by every `embed`, and
`index`, held by the index steps. qmd lets a keyword update run beside
an embed, but not two of either
([`qmd_behaviour.md`](qmd_behaviour.md), findings 4, 5 and 12).
`reads = "files"`, the default, says a step reads its inputs off disk,
so no writer of them runs beside it; `reads = "pinned"` says it reads
only the commit `DATALIB_READS` names, so a writer may go on beside it.
`start = "after_inputs_settle"` says a step waits until nothing
upstream of it is running or due, rather than starting on each seal of
a download that streams (`"on_each_seal"`, the default); no step of
`datalib-step`'s says it. None of these is in the fingerprint. The
rules are the dag README's "What keeps steps apart";
`configs/dag_example.toml` shows the syntax.

## The fan-ins read exactly their `inputs`

`grid_index` and `qmd_aggregator` take the groups they index from the
`inputs` they declare — `<group>/render_markdown` for the grid,
`<group>/keyword_index` and `<group>/embed` for the aggregator — and
the group is each input's first segment (`qmd_index::groups_from_inputs`,
shared by `grid_index.rs`). Nothing scans the root. A source removed
from the config stops being indexed on the next run even while its
rendered tree is still on disk, and `qmd_aggregator` retires the
collections no group claims.

A source's `keyword_index` (input: its render) registers its own
collection and fills it with the keyword index; its `embed` (input: the
`keyword_index`) fetches the embedding model and fills the vectors.
`qmd_aggregator` reads every source's pair, so it runs after all of
them: it retires the collections of sources it does not name and
reports each collection's counts. Nothing of a source's reads the
aggregator, and that is what lets each source's steps finish on their
own: the runner holds a step's finish back while anything upstream of
it is still running, so a step downstream of a fan-in would finish
only when the slowest source did. Each per-source step reports what it
read as its version and lets qmd skip what it already has, so one source
re-rendering reruns that source's two steps and nobody else's. A
`keyword_index` the aggregator does not name is wasted work: each
aggregation retires what it indexed. Removing the aggregator turns
free-text search off; the Manage screen takes every source's qmd steps
with it. A `qmd_index` step is the shape from before the aggregator,
when that fan-in fed the per-source steps or did their work itself;
`datalib-step` has no such function and refuses it, and `datalib-http`
rewrites it as soon as it reads such a file, at boot or on any change to it, keeping the old text as
`config.toml.bak` (`http/src/config_upgrade.rs`).
`datalib-migrate-config` makes the same rewrite from a terminal.

So the lists are what decides which indexes a source reaches. A render
step named by neither fan-in renders and reaches nothing. Named by
`grid_index` alone, its rows are in the grid and its documents open and
filter, but free text will not find it by its words or its meaning, which
are read from qmd's index, only by the fields the grid holds, so leaving a
source out costs keyword search as well as semantic. Which is still a reasonable thing
to want, because embedding is the slow part of a sync — though turning
off just its `embed` step keeps keyword search.

The wizard maintains all of it (`ui/src/config/sourceSteps.ts`):
`wireIntoFanIns` on create, `unwireFromFanIns` on delete and when a
render step is removed, and `setQmdSteps` for the two qmd tickboxes
under Rendering in Advanced options. "Keyword-index the markdown" adds or removes the
source's `keyword_index` and its edge into `qmd_aggregator`; "Embed it
for search by meaning", which needs the first, does the same for its
`embed`. Removing any step takes every step that reads it (a fan-in loses
the input instead), and removing the aggregator every source's qmd
steps (`removedWith`). A hand edit has
to remember, and the Manage screen flags a render step nothing consumes.

## What the Manage screen and the wizard make of it

One row per group, its steps and applets under a chevron. The group
row's rules are in `http/src/manage/group.rs`: children in pipeline
order; status running if any child is, else waiting, else failed, else
queued, else stopped, else the last step's, passing over a child turned
off (the group reads off only when every child is); last synced and last
success are the ingest step's instants, else the newest child's; bytes
are the group directory's own measured series, never a sum across
children; a sync of the group starts at its steps with no inputs (for
a diff group, at what its step reads). A child step is labelled by the
rest of its id after the group (its function, for a step of
`datalib-step`), with the whole id muted beside it. The grid's Source
column joins `source_id` to a group's `name`.

The wizard (`SourceWizard.vue`, writers in `sourceSteps.ts`) edits a
source as one thing: the group plus its `ingest` and `render_markdown`
steps from one form, one name box for the group. The form is the
catalog entry's `sections`, with the render fields and everything else
no section names under Advanced options
([`wizard_design.md`](wizard_design.md)). Editing renames the group in place and
rewrites both steps where the first of them stood. A source missing one of its
two steps gets it back on save. A download-only provider
(`rendersDocuments: false` in `ui/src/config/catalog.ts`) still gets its
render step: it renders no documents, but it writes the storage report and
carries the download's problems into the index, so the Manage row's counts
lead somewhere. The form offers it no Rendering section, since there is
nothing to render or search.

The screen lists groups, and the steps under each, in file order, and a
header click's sort is undone by a third click. So the writers keep the
file in the order data flows (`insertEntries`): a new source goes after
the last one and above the `unified_document_index` group, its qmd steps right
after its render, and each step below the steps it reads. A file already
out of that order where no place fits gets the new entries at the end.
The runner itself reads only `inputs`.

Dragging a group by the grip at the left of its row is how a person
changes that order (`moveGroup`). The group's `[[groups]]` entry and
every step and applet filed under it move together, comments included,
to just above the group it was dropped on — the block `topo-sort-config`
moves a group as. A drop that would put a group above one it reads, or below one
that reads it, is refused and the card says why (`moveAgainstDataFlow`),
so a file in data-flow order stays in it.

`datalib-step topo-sort-config <root>/config.toml` puts a whole file in
that order (`dag/src/config_order.rs`), keeping the old text as
`config.toml.<UTC time>.bak`; `--check` only says whether it is. Each group, with
its steps and applets, goes after the groups it reads, and inside a
group each step after the steps it reads. Nothing else moves, and the
comments above an entry move with it. Where two groups read each other
it orders the steps one by one instead.

The UI and the qmd upgrade edit the text rather than re-serializing it.
An `inputs` array is edited where it stands (`dag/src/config_array.rs` on the
server, `ui/src/config/tomlText.ts` in the UI): one id per line stays one
id per line, with its indentation, trailing comma and comments, and a
one-line array stays on one line. Both cut the text with the same small
lexer (`dag/src/config_lex.rs` beside the sorter; the UI keeps a copy),
and refuse an edit that does not read back as what was asked for.

## The retired shapes

`datalib-migrate-config` (`datalib/backend/migrate_config/`) is the
one place an earlier config shape is understood: `datalib-step
download …` command lines become groups and steps, retired type words become the current ones, and `sync` /
`common.input_path` / `common.raw_path` become method tables with
their own `path` (`convert.rs`). The rewrite is value-level, so comments do not
survive. The loader and `datalib-step` recognize the old shapes only
well enough to name the tool. Eight rewrites are made without asking,
all text edits that keep comments, and each refuses a result that would
drop an entry the original ran, other than one it exists to take out. `datalib-http` makes them in this order
(`http/src/config_upgrade.rs`), at boot, on any change to the file, and
on a save through `PUT /api/config` before it answers. Its rewrites and
its saves take turns, since a rewrite that read one save and wrote after
the next would put the first back:

1. Ids in lower case (`upgrade_lowercase_ids`, `lowercase_ids.rs`):
   every group, step and applet id, every function, a diff group's
   `source`, a step's `group` and `inputs` and an applet's `group` with
   a capital in it become lower case. At boot the server then moves
   each folder the config names to that spelling, one path segment at a
   time (`move_case_folders`), and carries whether each step under it
   was turned off; the steps themselves run again as new ones, so the
   grid and the search index take the source under its new id. If both
   spellings of a folder exist it leaves both and logs an error naming
   them.
2. The qmd steps' (`upgrade_qmd_steps`, `qmd_steps.rs`), which also
   leaves the file in data-flow order.
3. The document index's name (`upgrade_document_index`,
   `document_index.rs`): the untyped `unified_index` group, every step
   filed under it or writing under it, every `inputs` entry naming one,
   and the applet's `group`, `id` and `datalib-applet unified_index`
   subcommand become `unified_document_index`. At boot the server then
   moves `<root>/unified_index/` to `<root>/unified_document_index/` with
   one rename, so the grid store and the qmd embeddings are kept, and
   carries the loop's per-step rows in `system/supervisor.sqlite` (a step
   turned off, its last success, its tree's version) to the new ids. If
   both trees exist it leaves both and logs an error naming them.
   `datalib-applet unified_index` still runs, and the gateway still
   answers `/applet/unified_index/…`, for a config or a card script
   written before the rename.
4. The time-series steps (`upgrade_time_series_steps`,
   `time_series_steps.rs`): a config with no `unified_time_series_index`
   group and a source whose type has time series gets each such source's
   `render_time_series` step, the index group with its
   `time_series_index`, and the `unified_time_series_index` applet that
   serves it, appended at the end of the file. Once the group
   is there the time-series steps are the person's, and a step they took
   out is not put back.
5. The time-series embeds (`upgrade_time_series_embeds`,
   `time_series_embeds.rs`): a source whose type has time series loses
   its `embed` step, with the comments above it, and every `inputs`
   that named the step stops naming it. Its markdown is daily summaries
   of numbers, which the keyword index already finds, so the wizard
   never offers the step (`embedAllowed` in
   `ui/src/config/sourceSteps.ts`). This rewrite is the only one of the
   eight that is meant to take an entry out; it still refuses a result
   that loses any other entry.
6. The step commands (`upgrade_step_commands`, `step_commands.rs`):
   a step that writes no `command` was read as `datalib-step`, the
   shape from before every step named its program. The rewrite writes
   `command = "datalib-step"` on the line after its `function`, for
   rewrite 8 to complete.
7. Todoist's window (`upgrade_todoist_since`, `todoist_since.rs`): a
   Todoist ingest's `api.completed_since` becomes `api.since`, its value
   as it was. The loader refuses the old key.
8. What the runner used to infer for `datalib-step`
   (`upgrade_explicit_steps`, `explicit_steps.rs`), the shape from
   before the runner stopped knowing that program. A file is in that
   shape when a step writes `group` or `function`; a step's `command`
   is `datalib-step` (or a path ending in `/datalib-step`) without
   `run` as its next word, or is missing; the file has a top-level
   `checkpoint_cadence`, `render_pages` or `embed_contexts`; or a
   `[[groups]]` entry has `source`. Each step's `group` and `function`
   become `id = "<group>/<function>"`. A step of `datalib-step` gets
   `command = "datalib-step run <function>"`, then ` <type>` when its
   group has a type, then ` --source-type <t>` under a diff group,
   `<t>` being the type of the group its `source` named; the program
   word is kept as written. Where a step writes no `reads` or `locks`,
   it gets the ones the runner used to give it (§"The kinds of entry");
   a custom step with inputs gets `reads = "pinned"`, and
   `locks = ["index"]` when its group has no type. The file gets a
   `[[programs]]` entry for each spelling of `datalib-step` its steps
   use, a `[[locks]]` entry for each of `qmd_keyword`, `qmd_embed` and
   `index` some step holds and the file does not declare, and an
   `[env]` table holding `DATALIB_STEP_CHECKPOINT_CADENCE`,
   `DATALIB_STEP_RENDER_PAGES` and `DATALIB_STEP_EMBED_CONTEXTS` in
   place of the three top-level keys; a group loses its `source`. Every
   step's fingerprint changes with its `command`, so every step runs
   once at the next sync; a `datalib-step` step does only what is new.

The wizard never gives a source `unified_index`,
`unified_document_index` or `unified_time_series_index` as its id
(`RESERVED_IDS` in `ui/src/config/sourceSteps.ts`): those are the index
groups' ids, which have no `type`. The runner does not refuse one.
