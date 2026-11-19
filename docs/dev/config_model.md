# The config model: groups, steps and functions

What a `config.toml` is made of, the naming rules, how an ingest step
says where its data comes from, and what checks each part. The runner's
own view — what it fingerprints, what it forwards, what a bad entry
costs — is [`datalib/backend/dag/README.md`](../../datalib/backend/dag/README.md);
the contract a step command implements is
[`step_protocol.md`](step_protocol.md).
[`configs/dag_example.toml`](../../configs/dag_example.toml) is a
complete config with every line commented.

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
id = "unified_grid_and_search_index"       # no type: it mirrors nothing

[[steps]]
id = "unified_grid_and_search_index/unified_grid_and_search_index"
command = "datalib-step run unified_grid_and_search_index"
# every source's render
inputs = ["*/render_markdown"]
reads = "pinned"
locks = ["index"]

[[applets]]
group = "unified_grid_and_search_index"
id = "unified_grid_and_search_index"
command = "datalib-applet unified_grid_and_search_index"
```

**A group** is one thing on the Manage screen: an `id`, an optional
`name`, an optional `type`, an optional `description`. A source is a
group with a `type`; the unified grid and search index is a group without one.
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

- **`reads`.** `ingest` writes none, since it reads nothing. `embed`,
  `embedding_map` and perseus's
  `render_markdown` read their inputs off disk and write
  `reads = "files"`. Every other step writes `reads = "pinned"`, even
  while its `inputs` are empty (a fan-in's are, until a source is
  added).
- **`locks`.** `embed` writes `locks = ["embedder"]`, and every other
  step under a group with no `type` (`unified_grid_and_search_index`,
  `embedding_map`, `time_series_index`) writes `locks = ["index"]`. A
  source's `ingest` and renders write none, and hold the runner's
  default.

The file then declares those two locks in `[[locks]]` entries
(`embedder` with 1 slot, `index` with 2), since a
step that holds an undeclared lock is dropped, and lists the verbs
`datalib-step` takes in a `[[programs]]` entry. `datalib-step`'s own
code for all of this is `datalib_step/src/step_entry.rs`, which the
examples, `examples --append` and `check-params` share.

`[env]` is a table of variables handed to every step, below each
step's own `env`. `datalib-step` reads three knobs from it:
`DATALIB_STEP_CHECKPOINT_CADENCE`, `DATALIB_STEP_RENDER_PAGES` and
`DATALIB_STEP_MODAL_MAX_SANDBOXES` (`step_protocol.md` § "How
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
config. It may also carry an `embed`, like a source. The fan-ins read
it like any render step.
[`plans/completed/diff_renderer.md`](plans/completed/diff_renderer.md)
has the design; `configs/dag_example.toml` has one.

A diff group's tree is worth nothing kept: both commits stay in the
source's store, so comparing them again rebuilds it. Removing a diff
group on the Manage screen therefore offers, checked by default, to
delete the tree too (`POST /api/purge` with the group ids, once they
are out of the config): the loop stops the group's steps, deletes
`<root>/<group>/` and forgets the steps in its record
([`dag/README.md`](../../datalib/backend/dag/README.md#resets-and-purges)).
Without that, a group re-added under the same id and definition would
read as up to date and write nothing.

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
  `embed`, `unified_grid_and_search_index`, `embedding_map`, `time_series_index`
  (`datalib_step/src/function.rs`, tested against the layout constants
  the render and index crates use). `unified_grid_and_search_index` and `embedding_map`
  write `unified_grid_and_search_index/<function>`, and
  `time_series_index` writes `unified_time_series_index/time_series_index`,
  and nothing else, because the applet that reads each finds it from the
  data root alone.
- A source whose type has time series (`SourceType::renders_time_series`
  in `datalib_step/src/source_type.rs`: `garmin`, `yolink`,
  `airvisual`) has a `render_time_series` step beside
  its `render_markdown`, reading its ingest, and the
  `time_series_index` reads every such step (`*/render_time_series`).
  `docs/dev/time_series.md` is the design.
- A `calendar` source may have a `render_occurrences` step beside its
  `render_markdown`, reading its ingest: every occurrence of every event
  in a window of days around the run's now, one row each, the way
  Google's `singleEvents=true` lists them. Its params are `days_back`
  (30) and `days_ahead` (400). No index reads it. The rows and their
  columns: [`raw_stores.md`](../user/raw_stores.md#a-calendars-occurrences).
- An `embed` step takes one parameter, `embed_on_modal_from_n_chunks`,
  a whole number. Absent, the source's chunks are embedded on this
  machine and never leave it. Present, a run that owes at least that
  many chunks (prompts the inference cache has no vector for) sends all
  of them to the person's own Modal account to be embedded on rented
  GPUs, and a run that owes fewer embeds them here; `0` sends every run.
  `check-params` refuses anything else. What leaves and what guards it:
  `docs/dev/security_posture.md` § "What leaves the machine for Modal";
  how the step does it: `docs/dev/plans/embed_on_modal.md`. What every
  such step shares is in `[env]`: `DATALIB_STEP_MODAL_MAX_SANDBOXES`,
  how many Modal Sandboxes one step runs at once (100 when omitted). A step spreads what it owes over that many, at least
  256 chunks each.
- The search is the exception to "a step writes its own tree": each
  source's `embed` writes its chunks and their vectors to the one
  inference cache,
  `unified_grid_and_search_index/search_index/inference_cache.sqlite`,
  and `unified_grid_and_search_index` sweeps from it the vectors no
  chunk names. So every `embed` holds the one-slot `embedder` lock
  (below), and reads `unified_grid_and_search_index` off disk, so the
  runner never runs the two at once.
- A group's `type` names the thing mirrored, never the way it is
  reached — `claude` over the API or from an export, `contacts` over
  CardDAV or from `.vcf` files — and the product a person recognizes,
  not the vendor. Where a `grid_rows.provider` tag exists for a type,
  the two spell it the same way.

## Where the data comes from: the method table

An `ingest` step's params hold one table per method, and the table's
name is read *under the type*: a product has one API, so its table is
just `api`, and a type that is not one product qualifies its sources
(email's `jmap`, `gmail`, `mbox`). A table that reads files carries
its own `path`. Each type declares its own:

| type | methods |
|---|---|
| baby_connect, garmin, github, gitlab, google_drive, linear, notion, slack, todoist, yolink | `api` |
| chatgpt, claude | `api`, `export` |
| contacts | `fastmail`, `carddav`, `vcf` |
| email | `jmap`, `gmail`, `mbox`, `apple_mail` |
| calendar | `google`, `fastmail`, `caldav`, `calendly`, `ics` |
| lightroom | `catalog`, `backups` |
| airvisual, facebook, google_takeout, linkedin | `export` |
| signal, sms_backup_restore, whatsapp | `backup` |
| file_inventory, media, pdf | `fswalk` |
| beeper `texts` · apple_messages `messages` · apple_photos `library` · claude_code `sessions` · codex `sessions` · perseus `github` | |

`datalib-step help <type>` prints one source's methods and fields.

A method is *held* when its path is written and its value is neither
`null` nor `false`: a table counts by presence (`api = {}` is a
complete selection). A provider with more than one table refuses a step
naming two, so a store is filled one way. Three pairs are the
exception, each a past and a present of one thing, so a step may name
both and the present lands on top: `lightroom`'s `backups` folder and
its `catalog`; email's `mbox` beside `gmail`, a Takeout of the account
the API starts from ([Gmail](../user/getting_your_data.md#gmail)); and
`chatgpt`'s `export` beside `api`, which fills in the conversations the
store has never held before the API runs
([ChatGPT](../user/getting_your_data.md#chatgpt)).

Knobs that apply whatever the method (email's `only_extract_labels`,
the `common.*` envelope) stay at the top level of `params`. Render
knobs go on the render step, whose config is strict: download-shaped
params on a `render_markdown` step are refused.

## Time windows: `since` and `until`

A method that can mirror only part of a source's history takes
`since`, the oldest thing to mirror; a calendar, Garmin and Baby
Connect, whose days run forward, also take `until`. Both are keys of
the method table, and every one reads the same grammar:

| written | means |
|---|---|
| `2026-09-01` | that day; where an instant is wanted, its midnight in the run's offset (the machine's local one), so an hour off across a daylight-saving change |
| `2026-09-01T09:00:00-07:00` | that instant (RFC 3339, offset required); where a day is wanted, its date in its own offset |
| `-30d`, `+21d`, `-12h`, `-2w` | that far from the run's `now` (`DATALIB_DAG_NOW`), sign required, in hours, days or weeks |

A bound is resolved once per run, against the run's `now`, and the
download is handed an absolute value; so a relative window slides
forward run by run. A day before 1970 reaches back past anything a
source holds: Slack and Gmail ask from 1970 itself. An `until` before
its `since` is refused by `datalib-pipeline check` where neither the run's
clock nor its offset is needed to tell, and otherwise when a run
resolves them. A method that cannot honor `until` has no such key, and
naming one is refused as an unknown field.

| method | keys, and unset | what the window bounds | a record the window leaves behind |
|---|---|---|---|
| slack `api` | `since`: everything | a message's time | stays |
| claude `api`, chatgpt `api` | `since`: everything | a conversation's last update | stays |
| email `gmail` | `since`: everything | `after:` on the listing walk | stays |
| todoist `api` | `since`: the day the account joined | a task's completion time | stays |
| garmin `api` | `since`: a year before the first run, recorded; `until`: today | days | stays |
| baby_connect `api` | `since`: the eldest child's birthday; `until`: today | days | stays |
| calendar `google`, `fastmail`, `caldav` | `since`, `until`: no bound | events with any part inside | **dropped**: a windowed calendar is listed whole every run, and an event the listing no longer names goes |
| calendar `calendly` | `since`, `until`: no bound | a meeting's start | **dropped**, as above |

Garmin's default is a year because it costs one request per metric per
day. An earlier `since` backfills on the next run everywhere: each
source owes what its listing now names and its store does not hold.
YoLink's per-device `start` takes a day only.

**Several things, each with its own window: `fetch`.** A method whose
source has parts worth fetching to different depths takes repeated
`fetch` sections, each saying what it selects, in that source's own
words, and how far back:

```toml fragment
[[steps.params.gmail.fetch]]
labels = ["Inbox"]

[[steps.params.gmail.fetch]]
labels = ["Sent"]
since = "-90d"
```

Whatever the source, the sections mean the same:

- **What is fetched is the union.** Sections only add, so their order
  says nothing and a new one can never lose what another reaches.
- **A thing two sections select gets the wider window**, and a section
  with no `since` is the widest.
- **A section selecting nothing named takes everything** within its
  window. A name upstream does not have selects nothing and is a
  `config:` problem.
- **`fetch` replaces the method's own window and selection keys**, and
  the older keys are read as one section. A config with both is
  refused.

Only Gmail has it (`labels`, `since`, `whole_threads`;
[Different labels to different depths](../user/getting_your_data.md#different-labels-to-different-depths)
is the person's version). **No wizard form writes `fetch`**: edit the
config. Reopening such a source in its form keeps `fetch`, but the form
reads as if there were no filter, and choosing labels there writes the
older keys beside `fetch`, which the load refuses.

The window is a key of each method rather than one range under
`[steps.params.common]`, so what a source cannot honor is a missing
key, which the schema and the field reference already say.

## Examples and the field reference

Each source type carries its own example stanzas: a minimal and a
complete one per ingest method, each a `[[groups]]` entry and its steps,
with the type's own name as the group id. `datalib-step examples
<type>` prints them (`--append` adds the minimal one to a config), and
`datalib-step help <type> <method>` prints the field reference, the
params' JSON Schema with each field's description. A minimal stanza
holds its method alone; a complete one sets every option of its method
and may also hold the method it pairs with (Gmail's shows its Takeout).
A test loads and plans every stanza.

`datalib-step check-params <config>` plans the params of every step
that runs `datalib-step run` the way a sync would, and prints one JSON
line per step it would refuse. The server links no provider, so it asks:
`PUT /api/config` and `POST /api/config/check`
(`http/src/params_check.rs`) refuse to write a config a sync would
refuse a step of, and show each answer as an ordinary `rejected`
diagnostic at the misspelled key's line. `datalib-pipeline check` does not
ask: the runner knows no program, so it checks only what the runner
itself reads. Another program's params are never asked about.

## `Reach`: origin or local

Every method a provider accepts declares itself `Origin` (fetches from
a live service) or `Local` (reads files already on this machine). A
step's reach is `Origin` if anything it holds reaches one, else
`Local`. The UI reads the same declarations: a Manage row labels an
ingest step "Download" for an origin method and "Import" for a local
one, and the wizard shows a source's sign-in controls only while the
params it would write reach an origin.

`datalib-pipeline reset` is *not* gated on it: it empties the step's store
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
| a top-level key | it is `checkpoint_cadence`, `render_pages`, `[modal]`, `embed_contexts` or `embed_search_index`, the shape from before `[env]` (the steps still load); an `[env]` name is not a variable name |
| a lock | its name is not one segment, it has no slots, or it is declared twice |
| a program | its `command` is more than one word, or it is declared twice |
| a group | its id is not one segment, is reserved, or is a duplicate; it carries `source`, the shape from before a diff group's steps named their source's type |
| a step | it writes `group` or `function`, or has no `command`, the shape from before every step wrote its id and its whole command; a part of its id is not one segment; its id is under `system` or `datalib_curated`, duplicated, or nested with another; its `command` is empty or has unbalanced quoting; its program is in a package directory no `[[packages]]` entry pins; its params are over 64 KiB as JSON; it holds a lock no `[[locks]]` entry declares; an input names itself |
| a step (blocked: fix elsewhere) | its group or an input was itself dropped; an input names no step; it sits on a cycle or downstream of one |
| an applet | its id is not a JS identifier, is `user`, or is a duplicate; its `command` is empty |

A refusal of a retired shape names the rewrite: the app makes it when
it next reads the file, and `datalib-migrate-config` from a terminal
(§"The retired shapes").

Warnings: a group with nothing filed under it; an applet filed under an
undeclared group; and a verb a `[[programs]]` entry lists that the
runner does not know, which is ignored. The Manage screen's System row counts the config's
warnings and names them on hover; a double-click opens the config.

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
- `unified_grid_and_search_index` or `embedding_map` with any id but
  `unified_grid_and_search_index/<function>`, and `time_series_index`
  with any but `unified_time_series_index/time_series_index`.

A render reads its raw store from its first input; one that declares
none reads its own group's `ingest` tree and logs a warning. A store
kept on another disk is a symlink at `<group>/ingest` — no params key
can point elsewhere.

## What keeps steps apart: `[[locks]]`, `locks`, `reads`, `start`

A `[[locks]]` entry names a lock and its `slots`; a step's `locks` names
the ones it holds (`["q"]` takes one slot, `{ q = "exclusive" }` all of
them), and one that names none holds `network` (no inputs declared) or
`cpu` (inputs declared), the two budgets every config has. A Datalib
config declares two more for `datalib-step`'s steps (§"The kinds of
entry"): the one-slot `embedder`, held by every `embed`, so one
embedding model is loaded at a time, and `index`, held by the index
steps.
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

An input is a step id, or a pattern that names every step of one
function: `*/render_markdown` is `<group>/render_markdown` for every
group that has one, those added later included. The loader expands a
pattern to the ids it names, in the file's order, and never to the step
itself; everything past the loader, the step's own `--inputs` included,
sees ids only. A pattern names the steps that can run: a source whose
step the loader drops (its ingest has a bad key, say) drops out of the
pattern, where an input naming it by id blocks the step that names it.
`datalib-pipeline check` prints what each pattern stood for.

The index steps read by pattern: `unified_grid_and_search_index` reads
`*/render_markdown`, `embedding_map` reads `*/embed`, and
`time_series_index` reads `*/render_time_series`. No index reads a
`render_occurrences` step. `unified_grid_and_search_index` indexes the
groups its inputs name (each input's first segment) and passes over a
`render_time_series` or `render_occurrences` input, which an older
config may still name. Nothing scans the root: a source removed from
the config stops being indexed on the next run even while its render
store is still on disk.

After the grid, `unified_grid_and_search_index` puts each source it
reads in the keyword search (SQLite's FTS5, no embedding model), except
those its `params.not_searched` lists, and drops from the search index
every source it no longer searches. A source's pass is skipped when
neither its render store's commit nor the index text's version has
moved since its last finished pass. It then sweeps the vectors no chunk
names from the inference cache, and reports each searched source's
documents and how many are not embedded yet.

A source's `embed` (input:
`unified_grid_and_search_index/unified_grid_and_search_index`) cuts its
documents into chunks with the model's tokenizer, files them in the
inference cache and fills their vectors; it fetches the model when it
is missing, and only once it has a document to cut or a chunk to
embed. After the keyword passes `unified_grid_and_search_index` sweeps the vectors no
chunk names from the inference cache, and reports each searched
source's documents and how many of them are not embedded yet; each
`embed` reports what it left of its own. Nothing of a source's reads a step that reads
every source, and that is what lets each source's `embed` finish on its
own: the runner holds a step's finish back while anything upstream of
it is still running, so a step downstream of a fan-in would finish only
when the slowest source did. Search by meaning is off for a source with
no `embed`, or with its `embed` turned off; removing `unified_grid_and_search_index` takes
every `embed` with it. A `qmd_index`, `keyword_index` or
`qmd_aggregator` step is an older shape of this work: `datalib-step`
refuses it, and `datalib-http` rewrites it (§"The retired shapes").

So every source reaches the grid and the keyword search unless the
config says otherwise:

- **Out of search**: its id in `unified_grid_and_search_index`'s
  `params.not_searched`. It stays in the grid, and its documents open
  and filter, but free text finds it only by the fields the grid holds.
- **Out of search by meaning only**: no `embed` step, or its `embed`
  turned off. Keyword search still works; embedding is the slow part of
  a sync.
- **Out of the grid too**: list the other sources' renders one by one
  in place of the pattern.

Removing `unified_grid_and_search_index` takes every `embed` with it.

The wizard keeps the fan-ins in step: it wires a new source in (adding
nothing where a pattern already names its step), unwires one it
deletes, and offers two tickboxes under Rendering in Advanced options:
"Keyword-index the markdown" (in or out of `not_searched`) and "Embed
it for search by meaning" (adds or removes its `embed`; needs the
first). Removing any step takes every step that reads it (a fan-in
loses the input instead). A fan-in that names its inputs one by one has
to be edited by hand, and the Manage screen flags a render step nothing
reads.

## What the Manage screen and the wizard make of it

One row per group, its steps and applets under a chevron. A group row's
status is running if any child is, else waiting, else failed, else
queued, else stopped, else the last step's, passing over a child turned
off (the group reads off only when every child is) and one that has
never run while a sibling has (it reads never run only when nothing
under it has run); last synced and last
success are the ingest step's instants, else the newest child's; bytes
are the group directory's own measured series, never a sum across
children; a sync of the group starts at its steps with no inputs (for
a diff group, at what its step reads). A child step is labelled by the
rest of its id after the group (its function, for a step of
`datalib-step`), with the whole id muted beside it. The grid's Source
column joins `source_id` to a group's `name`.

The wizard edits a source as one thing: the group plus its `ingest` and
`render_markdown` steps from one form, one name box for the group.
Render fields and everything no section of the form names go under
Advanced options. Editing renames the group in place and rewrites both
steps where the first of them stood; a source missing one of its two
steps gets it back on save. A download-only source (Linear, media,
Lightroom, Apple Photos) still gets its render step: it renders no documents, only
the source's storage report, which is what puts the source in the grid.
Its form offers no Rendering section.

The screen lists groups, and the steps under each, in file order. So
the app keeps the file in the order data flows: a new source goes after
the last one and above the `unified_grid_and_search_index` group, and
each step below the steps it reads, so a source's `embed` goes below
`unified_grid_and_search_index`. Where a file is already out of order
and no place fits, new entries go at the end. The runner itself reads
only `inputs`.

Dragging a group by the grip at the left of its row changes that order.
The group's `[[groups]]` entry and every step and applet filed under it
move together, comments included, to just above the group it was
dropped on. A drop that would put a group above one it reads, or below
one that reads it, is refused, and the card says why.

`datalib-step topo-sort-config <root>/config.toml` puts a whole file in
that order, keeping the old text as `config.toml.<UTC time>.bak`;
`datalib-pipeline check` only says whether it is. Each group, with its steps and
applets, goes after the groups it reads, and inside a group each step
after the steps it reads. Nothing else moves, and the comments above an
entry move with it. Where two groups read each other it orders the
steps one by one instead.

The app and the config upgrades edit the text rather than
re-serializing it: an `inputs` array is edited where it stands, one id
per line staying one id per line with its indentation, trailing comma
and comments, and a one-line array staying on one line. An edit that
does not read back as what was asked for is refused.

## The retired shapes

`datalib-migrate-config` (`datalib/backend/migrate_config/`) is the
one place an earlier config shape is understood: `datalib-step
download …` command lines become groups and steps, retired type words become the current ones, and `sync` /
`common.input_path` / `common.raw_path` become method tables with
their own `path` (`convert.rs`). The rewrite is value-level, so comments do not
survive. The loader and `datalib-step` recognize the old shapes only
well enough to name the tool. Fifteen rewrites are made without asking,
all text edits that keep comments, and each refuses a result that would
drop an entry the original ran, other than one it exists to take out. They are listed, in this
order, in `datalib_migrate_config::UNATTENDED`, which `datalib-http` runs
(`http/src/config_upgrade.rs`) at boot, on any change to the file, and
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
3. The index's name (`upgrade_index_name`, `index_name.rs`): the
   untyped group under one of its earlier names, `unified_index` or
   `unified_document_index`, every step filed under it or writing under
   it, every `inputs` entry naming one, the applet's `group`, `id` and
   `datalib-applet` subcommand, and the name and banner comment the
   scaffold gave it become `unified_grid_and_search_index`. At boot the
   server then moves the old `<root>/<name>/` to
   `<root>/unified_grid_and_search_index/` with one rename, so the grid
   store and the search index's vectors are kept, and carries the loop's
   per-step rows in `system/supervisor.sqlite` (a step turned off, its
   last success, its tree's version) to the new ids. If both trees exist
   it leaves both and logs an error naming them. `datalib-applet` still
   runs under either old name, and the gateway still answers
   `/applet/unified_index/…` and `/applet/unified_document_index/…`, for
   a config, a card script or an agent written before the rename.
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
   `ui/src/config/sourceSteps.ts`).
6. The step commands (`upgrade_step_commands`, `step_commands.rs`):
   a step that writes no `command` was read as `datalib-step`, the
   shape from before every step named its program. The rewrite writes
   `command = "datalib-step"` on the line after its `function`, for
   rewrite 15 to complete.
7. Todoist's window (`upgrade_todoist_since`, `todoist_since.rs`): a
   Todoist ingest's `api.completed_since` becomes `api.since`, its value
   as it was. `datalib-step` refuses the old key.
8. The conversation settings (`upgrade_conversation_settings`,
   `conversation_settings.rs`): a ChatGPT ingest's `api.max_pages`,
   `api.limit` and `api.sleep_between` become `api.max_listing_pages`,
   `api.max_conversations_per_run` and `api.seconds_between_requests`,
   and a Claude ingest's `api.refresh_most_recent_n_chat_count` becomes
   `api.refetch_newest_conversations`, each value as it was. The two
   sources' API settings now share their names. `datalib-step` refuses the
   old keys.
9. The keyword fold (`upgrade_keyword_fold`, `keyword_fold.rs`): each
   built-in `keyword_index` step goes, with its comments; every input
   that named one (an `embed`, the embedding map) reads `unified_grid_and_search_index`
   instead; and each source `unified_grid_and_search_index` reads that had no
   `keyword_index` is listed in its `params.not_searched`, so nothing
   out of search comes into it. A config with no `unified_grid_and_search_index` gets one
   reading every render. `datalib-step` refuses the old step.
10. The aggregator (`upgrade_aggregator_drop`, `aggregator_drop.rs`):
   each built-in `qmd_aggregator` step goes, with its comments, and an
   input that named one (the embedding map's) reads `*/embed`.
   `unified_grid_and_search_index` sweeps the vectors and reports what search holds.
   `datalib-step` refuses the old step.
11. The index step's name (`upgrade_index_step_name`,
   `index_step_name.rs`): a built-in step whose function is `grid_index`
   takes its group's name, `unified_grid_and_search_index`, so its id is
   `unified_grid_and_search_index/unified_grid_and_search_index`, and
   every `inputs` entry naming it (each source's `embed`) names the new
   id. At boot the server then moves
   `<root>/unified_grid_and_search_index/grid_index/` to the new id's
   tree with one rename (`move_index_step_dir`), so the grid index's
   file, with its cursors and the search tables, is kept, and carries the
   step's rows in `system/supervisor.sqlite` (turned off, its last
   success, its tree's version) to the new id. The step and every
   `embed` still run once at the next sync, since a step's id and inputs
   are in its fingerprint; the step reads from the cursors the moved file
   kept, and an embed finds its vectors in the inference cache.
   If both trees exist it leaves both and logs an error naming them.
   `datalib-step` refuses the old function.
12. The input patterns (`upgrade_input_patterns`, `input_patterns.rs`):
   an index step whose `inputs` name every step of a function it reads
   (`FAN_INS`) gets the pattern in their place, `*/render_markdown` for
   every render, so it reads the same steps and, from then on, every
   source added. A list that leaves a step out is left as written, since
   whoever wrote it may have meant it. The loader still reads the lists.
13. The unread inputs (`upgrade_unread_inputs`, `unread_inputs.rs`):
   the index step stops naming `render_time_series` and
   `render_occurrences` steps, by pattern or by id. A config was given
   them so the step could copy their problem rows; problems are now read
   in each step's own store, the step passes over those inputs, and all
   they did was run it again after every such render. An index step that
   runs a program other than `datalib-step` is left as written. It runs
   after the rename above, so it finds the step by its new function.
14. The search locks (`upgrade_lock_names`, `lock_names.rs`): a step's
   `locks` that named `qmd_embed` name `embedder`, in a list or a
   table, and `qmd_keyword` is dropped from them; a step left holding
   nothing loses the key and holds its default lock. A `[[locks]]`
   entry for `qmd_embed` resizes `embedder` instead, and one for
   `qmd_keyword` goes, with its comments. The loader refuses a step
   holding a lock nothing declares, which both once were by default.

15. What the runner used to infer for `datalib-step`
    (`upgrade_explicit_steps`, `explicit_steps.rs`), the shape from
    before the runner stopped knowing that program. It runs last, so it
    reads the shape every rewrite above leaves. A file is in that shape
    when a step writes `group` or `function`; a step's `command` is
    `datalib-step` (or a path ending in `/datalib-step`) without `run`
    as its next word, or is missing; the file has a top-level
    `checkpoint_cadence`, `render_pages`, `[modal]`, `embed_contexts` or
    `embed_search_index`; or a `[[groups]]` entry has `source`. Each
    step's `group` and `function` become `id = "<group>/<function>"`. A
    step of `datalib-step` gets `command = "datalib-step run
    <function>"`, then ` <type>` when its group has a type, then
    ` --source-type <t>` under a diff group, `<t>` being the type of the
    group its `source` named; the program word is kept as written. Where
    a step writes no `reads` or `locks`, it gets the ones the runner used
    to give it (§"The kinds of entry"); a custom step with inputs gets
    `reads = "pinned"`, and `locks = ["index"]` when its group has no
    type. The file gets a `[[programs]]` entry for each spelling of
    `datalib-step` its steps use, a `[[locks]]` entry for each of
    `embedder` and `index` some step holds and the file does not
    declare, and an `[env]` table holding
    `DATALIB_STEP_CHECKPOINT_CADENCE`, `DATALIB_STEP_RENDER_PAGES` and
    `DATALIB_STEP_MODAL_MAX_SANDBOXES` in place of `checkpoint_cadence`,
    `render_pages` and `[modal]`; `embed_contexts` and
    `embed_search_index`, which no step reads any more, go, with the
    comments above them; a group loses its `source`. A WhatsApp or
    Signal ingest gets the variable it reads its backup secret from
    named in its params. Every step's fingerprint changes with its
    `command`, so every step runs once at the next sync; a
    `datalib-step` step does only what is new.

The wizard never gives a source `unified_index`,
`unified_document_index`, `unified_grid_and_search_index` or
`unified_time_series_index` as its id (`RESERVED_IDS` in
`ui/src/config/sourceSteps.ts`): those are the index groups' ids, which
have no `type`. The runner does not refuse one.
