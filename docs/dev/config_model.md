# The config model: groups, steps and functions

What a `config.toml` is made of, the naming rules, how an ingest step
says where its data comes from, and what checks each part. The runner's
own view — what it fingerprints, what it forwards, what a bad entry
costs — is [`datalib/backend/dag/README.md`](../../datalib/backend/dag/README.md);
the contract a step command implements is
[`step_protocol.md`](step_protocol.md).
[`configs/dag_example.toml`](../../configs/dag_example.toml) is a
complete config with every line commented.

## Three kinds of entry

```toml
[[groups]]
id = "work-slack"          # the directory; permanent
name = "Work Slack"        # free text; nothing depends on it
type = "slack"             # what is mirrored — makes this a source

[[steps]]
group = "work-slack"
function = "ingest"        # id composes to work-slack/ingest
command = "datalib-step"
[steps.params.api]         # the method: Slack's own API
channels = ["chat-qi"]

[[steps]]
group = "work-slack"
function = "render_markdown"
command = "datalib-step"
inputs = ["work-slack/ingest"]

[[groups]]
id = "unified_grid_and_search_index"       # no type: it mirrors nothing

[[steps]]
group = "unified_grid_and_search_index"
function = "unified_grid_and_search_index"
command = "datalib-step"
# every source's render
inputs = ["*/render_markdown"]

[[applets]]
group = "unified_grid_and_search_index"
id = "unified_grid_and_search_index"
command = "datalib-applet unified_grid_and_search_index"
```

**A group** is one thing on the Manage screen: an `id`, an optional
`name`, an optional `type`, an optional `description`. A source is a
group with a `type`; the unified grid and search index is a group
without one. `name` and `description` are display text — never
forwarded to a step, never fingerprinted — so editing either re-runs
nothing.

**A step** is `(group, function)`. The loader composes its id as
`<group>/<function>`; the id is never written, and nothing downstream
splits it. That id is the tree the step writes, the key its state is
kept under, and what another step's `inputs` name. Every step names
the program it runs in `command`. Datalib's own steps run
`datalib-step`, which reads its function and its group's `type` from
the environment, so such a step has to be under a group. Another
program may take any function word it likes, or sit outside any group
with a verbatim `id` — the one place a step id is written.

**An applet** is a server the http gateway spawns on demand. Its `id`
is the mount prefix and a JavaScript identifier, unique on its own and
not composed; `group` on an applet only says which Manage row it is
filed under.

**A diff group** is a group of `type = "diff"` with a `source`: it
mirrors nothing itself and renders what changed in the `source` group's
raw store between two commits, as documents with the changes marked and
`grid_rows` with a `diff_status`. Its render step is `render_markdown`,
reading `<source>/ingest`, with the two commits under `params.diff`
(`from` and `to`, both required, and `max_documents`, the most
documents a side may render before the step fails, default 1000); the
rest of `params` is the source type's render config. It may also carry
an `embed`, like a source; no other function. The index steps read it
like any render step. The source's id and type reach the step as
`DATALIB_DAG_SOURCE_GROUP` and `DATALIB_DAG_SOURCE_GROUP_TYPE`.

A diff group's tree is worth nothing kept: both commits stay in the
source's store, so comparing them again rebuilds it. Removing a diff
group on the Manage screen therefore offers, checked by default, to
delete the tree too (`POST /api/purge` with the group ids, once they
are out of the config): the loop stops the group's steps, deletes
`<root>/<group>/` and forgets the steps in its record
([`dag/README.md`](../../datalib/backend/dag/README.md#resets-and-purges)).
Without that, a group re-added under the same id and definition would
read as up to date and write nothing.

Besides the three kinds of entry, a config may hold `[[locks]]`
(below), `[[packages]]` (packages the library uses, each pinned by its
manifest's hash; a step or applet runs one of a package's programs by
its path under the data root), and top-level settings: `data_root`,
`binary_dir`, `log_level`, `[checkpoint_cadence]`, `[run_history]`,
`[disk_space]`, `[render_pages]` and `[modal]`. The commented example
shows most of them; an unknown top-level key fails the whole file.

## Naming rules

- A group id and a function are each one portable filename segment:
  lowercase letters, digits, `.`, `_`, `-`, not starting with `-`,
  never `.` or `..`, no `/`. Lower case because each names a folder and
  a Mac's disk does not tell `Email/` from `email/`. An applet id is a
  lowercase JavaScript identifier for the same reason (it names
  `system/frontend/<id>/`).
- `system` is reserved for the runner's and server's own state.
- Step ids are unique and never nested: two steps under one tree would
  be two writers on one doltlite file.
- The built-in functions are the directory names: `ingest`,
  `render_markdown`, `render_time_series`, `render_occurrences`,
  `embed`, `unified_grid_and_search_index`, `embedding_map`,
  `time_series_index`. `unified_grid_and_search_index` and
  `embedding_map` write `unified_grid_and_search_index/<function>`,
  and `time_series_index` writes
  `unified_time_series_index/time_series_index`, and nowhere else,
  because the applet that reads each finds it from the data root alone.
  `datalib-step help <function>` describes each.
- A source whose type has time series (`garmin`, `yolink`,
  `airvisual`) has a `render_time_series` step beside its
  `render_markdown`, reading its ingest, and the `time_series_index`
  reads every such step (`*/render_time_series`).
- A `calendar` source may have a `render_occurrences` step beside its
  `render_markdown`, reading its ingest: every occurrence of every event
  in a window of days around the run's now, one row each, the way
  Google's `singleEvents=true` lists them. Its params are `days_back`
  (30) and `days_ahead` (400). No index reads it. The rows and their
  columns: [`raw_stores.md`](../user/raw_stores.md#a-calendars-occurrences).
- An `embed` step takes one parameter, `embed_on_modal_from_n_chunks`,
  a whole number. Absent, the source's chunks are embedded on this
  machine and never leave it. Present, a run that owes at least that
  many chunks (prompts the inference cache has no vector for) sends the
  text of all of them to the person's own Modal account (`modal setup`
  signs in) to be embedded on rented GPUs; a run that owes fewer embeds
  them here, and `0` sends every run. Set it only on a source whose text
  may leave the machine. The top-level `[modal]` table's `max_sandboxes`
  is how many Modal Sandboxes one step runs at once (100 when omitted);
  a step spreads what it owes over them, at least 256 chunks each.
  `docs/dev/security_posture.md` § "What leaves the machine for Modal"
  in the repo has what leaves and what guards it.
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
its `since` is refused by `datalib-dag --check` where neither the run's
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

`datalib-step check-params <config>` plans every built-in step's params
the way a sync would and prints one JSON line per step it would refuse.
The runner and the server link no provider, so both ask it:
`datalib-dag --check`, and `PUT /api/config` and `POST
/api/config/check`, which refuse to write a config a sync would refuse
a step of. Each answer becomes a `rejected` diagnostic at the
misspelled key's line, so every door shows it the same way. A custom
command's params are never checked.

## `Reach`: origin or local

Every method a provider accepts declares itself `Origin` (fetches from
a live service) or `Local` (reads files already on this machine). A
step's reach is `Origin` if anything it holds reaches one, else
`Local`. The UI reads the same declarations: a Manage row labels an
ingest step "Download" for an origin method and "Import" for a local
one, and the wizard shows a source's sign-in controls only while the
params it would write reach an origin.

`datalib-dag --reset` is *not* gated on it: it empties the step's store
whatever the method, and the next sync reads its files or its origin in
full — the only "from scratch" button a user has.

## What the loader checks

A bad entry is dropped and the rest of the file runs.
`datalib-dag --check <config>` lists what went and why.

| the entry | is dropped when |
|---|---|
| the whole file (`Fatal`) | it is not TOML; nothing loads |
| any entry | it does not deserialize: an unknown key, a wrong type, both `id` and `group` |
| a lock | its name is not one segment, it has no slots, or it is declared twice |
| a group | its id is not one segment, is `system`, or is a duplicate; a source takes an id reserved for an index group (`unified_grid_and_search_index`, `unified_time_series_index`, `unified_index`, `unified_document_index`); it is a `diff` group with no `source`, or whose `source` names no group, a typeless group or another `diff` group; it carries `source` without being a `diff` group |
| a step | its function is not one segment; its group is undeclared; it runs `datalib-step` and has no group; its `command` is the retired `datalib-step download\|render\|grid_index\|qmd_index …` shape; it is a built-in `qmd_index`, `keyword_index`, `qmd_aggregator` or `grid_index`, retired functions (the message names `datalib-migrate-config`); its id is under `system`, duplicated, or nested with another; its `command` is empty or has unbalanced quoting; it holds a lock no `[[locks]]` entry declares; an input names itself; under a `diff` group it is not `render_markdown` or `embed`, or it is the render and has inputs and the first is not `<source>/ingest` |
| a step (blocked: fix elsewhere) | its group or an input was itself dropped; an input names no step; it sits on a cycle or downstream of one |
| an applet | its id is not a JS identifier, is `user`, or is a duplicate; its `command` is empty |

Warnings, which drop nothing: a group with nothing filed under it; a
`name` on a grouped step (the label comes from the group and the
function); an applet filed under an undeclared group; a top-level
`embed_contexts` or `embed_search_index`; and a built-in step's
`period` or `backup.period`, `common.always_clear_before_ingest`,
linkedin's `export.fetch_photos` or email's `gmail.request_concurrency`.
None of those does anything; `datalib-step` drops the params ones
before parsing, so the step still runs. The Manage screen's System row
counts the config's warnings and names them on hover.

## What the runner forwards and what `datalib-step` refuses

Every grouped step gets `DATALIB_DAG_STEP` (the composed id),
`DATALIB_DAG_GROUP`, `DATALIB_DAG_FUNCTION`, and `DATALIB_DAG_GROUP_TYPE`
when the group has one. The group's `type` is in every child step's
fingerprint, so changing it re-runs the group.

`datalib-step` refuses, naming the fix:

- no `DATALIB_DAG_GROUP`, or a function it does not perform;
- a step id that is not `<group>/<function>` as the environment
  spells them;
- `ingest` or `render_markdown` under a group with no `type`;
- `render_time_series` under a group with no `type`, or of a type with
  no time series;
- `render_occurrences` under a group whose type is not `calendar`, or
  with a param other than `days_back` and `days_ahead`;
- a `type` it has no provider for — a retired spelling (`*_api`,
  `*_backup`, `claude_export`, `carddav`) names `datalib-migrate-config`;
- params still carrying `sync`, `common.input_path`, `common.raw_path`,
  `gmail_api` or a top-level `fetch_photos`, likewise naming the tool;
- an `ingest` step whose params hold none of its type's methods;
- `unified_grid_and_search_index` or `embedding_map` under any group but
  `unified_grid_and_search_index`, and `time_series_index` under any but
  `unified_time_series_index`.

A render reads its raw store from its first input; one that declares
none reads its own group's `ingest` tree and logs a warning. A store
kept on another disk is a symlink at `<group>/ingest` — no params key
can point elsewhere.

## What keeps steps apart: `[[locks]]`, `locks`, `reads`, `start`

A `[[locks]]` entry names a lock and its `slots`; a step's `locks` names
the ones it holds (`["q"]` takes one slot, `{ q = "exclusive" }` all of
them). A step that names none holds one of the budgets every config
has: `network`, `cpu` or `index`, or, for a built-in `embed`, the
one-slot `embedder`, so one embedding model is loaded at a time.
`reads = "files"` says a step reads its inputs off disk, so no writer of
them runs beside it. `start = "after_inputs_settle"` says a step waits
until nothing upstream of it is running or due, rather than starting on
each seal of a download that streams (`"on_each_seal"`, the default);
no built-in step says it. None of these is in the fingerprint. The
rules:
[`dag/README.md`](../../datalib/backend/dag/README.md#what-keeps-steps-apart-locks).

## The fan-ins read exactly their `inputs`

An input is a step id, or a pattern that names every step of one
function: `*/render_markdown` is `<group>/render_markdown` for every
group that has one, those added later included. The loader expands a
pattern to the ids it names, in the file's order, and never to the step
itself; everything past the loader, the step's own `--inputs` included,
sees ids only. A pattern names the steps that can run: a source whose
step the loader drops (its ingest has a bad key, say) drops out of the
pattern, where an input naming it by id blocks the step that names it.
`datalib-dag --check` prints what each pattern stood for.

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
is missing, and only once it has something to embed. Each source's
`embed` finishes on its own: the runner holds a step's finish back
while anything upstream of it runs, so a step downstream of a fan-in
would finish only when the slowest source did.

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
never run while a sibling has. Last synced and last success are the
ingest step's, else the newest child's; bytes are the group directory's
own; a sync of the group starts at its steps with no inputs (for a diff
group, at what its step reads). A child step is labelled by its
function, with the composed id muted beside it. The grid's Source
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
`--check` only says whether it is. Each group, with its steps and
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

`datalib-migrate-config` is the one place the oldest config shape is
understood: `datalib-step download …` command lines become groups and
steps whose command is `datalib-step`, retired type words become the
current ones, and `sync` / `common.input_path` / `common.raw_path`
become method tables with their own `path`. The rewrite is value-level,
so comments do not survive. The loader and `datalib-step` recognize the
old shapes only well enough to name the tool.

Every later shape is rewritten without asking by `datalib-http`, at
boot, on any change to the file, and on a save through `PUT
/api/config` before it answers. Each rewrite is a text edit that keeps
comments, and refuses a result that would drop an entry the original
ran, other than one it exists to take out. Its rewrites and its saves
take turns. In order:

1. **Lower-case ids**: every id, function, diff `source`, step `group`
   and `inputs` and applet `group` with a capital in it. At boot the
   server moves each folder to the new spelling and carries whether
   each step was turned off; the steps run again as new ones.
2. **The qmd steps**: an old `qmd_index` becomes the shape rewrites 9
   and 10 then take out; the file is left in data-flow order.
3. **The index group's name**: a group named `unified_index` or
   `unified_document_index`, its steps, the `inputs` naming them and
   its applet become `unified_grid_and_search_index`. At boot the server
   renames `<root>/<old name>/` to `<root>/unified_grid_and_search_index/`,
   keeping the grid store and the vectors, and carries each step's row
   in `system/supervisor.sqlite` (turned off, last success, version) to
   the new id. `datalib-applet` still runs under either old name, and
   the gateway still answers `/applet/unified_index/…` and
   `/applet/unified_document_index/…`.
4. **The time-series steps**: a config with a time-series source and no
   `unified_time_series_index` group gets each such source's
   `render_time_series`, the index group with its `time_series_index`,
   and its applet, at the end of the file. Once the group is there the
   steps are the person's, and one taken out is not put back.
5. **The time-series embeds**: a time-series source loses its `embed`,
   since its markdown is daily summaries of numbers, which keyword
   search finds. The wizard never offers one.
6. **The step commands**: a step with no `command` gets
   `command = "datalib-step"`, which is how the loader reads it anyway.
7. **Todoist's window**: `api.completed_since` becomes `api.since`.
8. **The conversation settings**: ChatGPT's `api.max_pages`,
   `api.limit` and `api.sleep_between` become `api.max_listing_pages`,
   `api.max_conversations_per_run` and `api.seconds_between_requests`;
   Claude's `api.refresh_most_recent_n_chat_count` becomes
   `api.refetch_newest_conversations`.
9. **The keyword fold**: each `keyword_index` step goes; an input that
   named one reads the index step instead; and each source the index
   reads that had no `keyword_index` is listed in its
   `params.not_searched`, so nothing comes into search that was out of
   it. A config with no index step gets one reading every render.
10. **The aggregator**: each `qmd_aggregator` step goes, and the
    embedding map, which read it, reads `*/embed`.
11. **The index step's name**: a step whose function is `grid_index`
    becomes `unified_grid_and_search_index`, and every `inputs` naming
    it names the new id. At boot the server renames the old step's tree
    to the new one, keeping the grid index's file with its cursors and
    search tables, and carries its row in `system/supervisor.sqlite`.
    The step and every `embed` run once at the next sync, since an id
    is in its fingerprint, but read on from what was kept.
12. **The input patterns**: an index step whose `inputs` list every
    step of the function it reads gets the pattern instead
    (`*/render_markdown`). A list that leaves a step out is left as
    written.
13. **The unread inputs**: the index step stops naming
    `render_time_series` and `render_occurrences` steps, which only
    made it run again after each of them. One that runs a program other
    than `datalib-step` is left as written.
14. **The search locks**: `qmd_embed` in a step's `locks` becomes
    `embedder`, and `qmd_keyword` is dropped; a step left holding
    nothing loses the key. A `[[locks]]` entry for `qmd_embed` resizes
    `embedder`, and one for `qmd_keyword` goes.

Where a rewrite moves a folder and both the old and the new exist, the
server leaves both and logs an error naming them. The loader refuses
the old functions and keys rewrites 7 to 11 take out, and a lock
nothing declares.
