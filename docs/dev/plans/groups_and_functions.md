# Groups and functions: one row per source

**Status: agreed design (2026-09-09); slices 1, 2, 3a, 4a and 4b built
(2026-09-09 and 2026-09-10), slices 3b and 5 not.** Written against
`eee381c3`. Per [`AGENTS.md`](../../../AGENTS.md), don't cite this
file as a description of the tree. Where it says "today", that was
checked against that commit; where it says "will", check the slice
list under "Order of work" — slices 1, 2, 3a, 4a and 4b are in the tree,
and the places each departed from this text are recorded there.

**Reverses** the "Sources stop being a grouping" section of
[`step_identity.md`](completed/step_identity.md) and the header of
`datalib/ui/src/config/sourceSteps.ts`, which says there is no data
source "deliberately". Both are rewritten as part of this change, not
footnoted.

## The problem

A UI review found the Manage screen confusing, for a reason we already
knew: one data source is two rows in the table (`slack/raw` and
`slack/rendered_md`), each with its own name, status and size. A person
thinks of "Work Slack" as one thing. We still want ingest and render
to be separate steps under the hood, with the streaming edge between
them, and their storage side by side on disk.

The earlier fused row was removed because it was reconstructed by
splitting paths and was never a config entity. This design makes the
grouping a config entity, so the table can show it without inventing
it.

## The decisions

Each of these was argued out; the reasoning is kept short and the rule
is what matters.

**1. One container, called a group.** A `[[groups]]` entry has an `id`,
a `name`, and an optional `type`. Steps and applets carry `group = "…"`.
The word is "group" rather than "source" because the unified index is
one too: `unified_index/grid`, `unified_index/qmd` and the
`unified_index` applet are already laid out on disk as a group with two
steps and an applet under it. The wizard still says "Add source"; a
source is a group with a `type`.

**2. A step is (group, function). Its id is derived and never
written.** A step declares the group it belongs to and the function it
performs. Both are permanent — a step never changes group, and an
ingest never becomes a render — so the composed `<group>/<function>`
is stable by construction. The composed id is the directory the step
writes, the key in `system/dag_state.json`, and what `inputs` name.
Nothing anywhere splits it; the loader composes it from two written
fields and everything downstream treats it as opaque.

**3. The tree is named after the operation.** Today a step declares
*where* (`id = "slack/raw"`) and *what* (`command = "datalib-step
download slack_api"`) separately, and for the built-in steps they are
the same choice written twice. Worse, `datalib-step` ignores the
declared tree: it takes the first segment of `DATALIB_DAG_STEP` as the
source name and writes a hardcoded `<name>/raw` or `<name>/rendered_md`.
A config saying `id = "slack/foo"` with a download command has the
runner tracking `slack/foo` while the step writes `slack/raw`, and
nothing checks. Naming the tree after the function closes that bug
because there is no second string to disagree.

**4. `command` is optional, and absent means the built-in step
program.** The runner already privileges `datalib-step` by resolving it
from its own directory. One rule: a step with no `command` runs
`datalib-step`, which dispatches on the function and the group's type,
both passed in the environment. A custom step writes any function name
it likes plus a `command`, and its tree is `<group>/<function>`.

**5. `type` is the data type; the method lives in the ingest step.**
Today email is one type whose mode is selected by which params table
is present (`sync` for JMAP, `gmail_api = {}`, `mbox`), while Claude is
two types (`claude_api`, `claude_export`) sharing one raw store and one
render crate. Email's way is right, made explicit: the ingest step's
params hold one table per method, named for the method (`api`, `export`,
`jmap`, `gmail_api`, `mbox`, …), and a file-backed method carries its
own `path` instead of the shared `common.input_path`. The group's type
names the thing being mirrored (`slack`, `claude`, `email`), and the
render side is exactly a function of it. `SourceType` becomes that
list; `claude_export` leaves it, and the `_api` suffixes go with it
since they were naming the method.

A method table implies `function = "ingest"`, and the function is
still written. The two have different readers: the runner composes the
id and names the directory from `function` before anything runs, and
it never looks inside `params`, so inferring the function would mean
teaching it provider vocabulary — and only for the one function that
has methods. `datalib-step` knows both facts, so it fails a step
loudly when they disagree: a method table on a `render_markdown` step,
or an `ingest` step with no method table.

**6. Only the group has a name.** A step's label is derived, never
written: "Render markdown" and "Grid index" from the function, and for
an `ingest` step "Download" when any of its method tables reaches an
origin, else "Import" (decision 7). The composed id shows muted beside
it. The wizard offers one name box, on the group. A hand-editor may
still put `name` on a step; the wizard never does.

**7. Vocabulary.** `raw` becomes `ingest` and `rendered_md` becomes
`render_markdown`, because the directory is named after the function
that writes it. `render_markdown` leaves room for a second renderer
without a rename. The index steps keep their function names, so their
trees become `unified_index/grid_index` and `unified_index/qmd_index`.

There are two ways data comes in — **downloading** from a live origin
and **importing** files already on disk — and both words stay in the
vocabulary: the user guide, the Manage row, the wizard. But the
distinction is a property of the *method*, not of the step, and it
lives there. Every method table a provider accepts declares itself
`Origin` or `Local`, a closed set (`strum`, per `AGENTS.md`) in the
provider's config crate and mirrored in the catalog; a method with no
declaration does not load. That forces the distinction on every
provider anyone adds, at the one place that knows the answer, and it
cannot go stale: whether a step downloads is read off its current
params every time anything asks. Three things read it:

- the Manage row's label, "Download" or "Import";
- the wizard, which shows a latchkey / credentials section only for an
  `Origin` method — an import has nothing to log in to;
- `DATALIB_DAG_RESET_AND_REDOWNLOAD`, which already means "if you
  fetch from an origin", and now has the property to check rather than
  a convention.

The step's function is `ingest` for all of them. Two function words
were considered and dropped: a step's word is fixed with its directory,
and "reaches an origin" is a property that changes when a method table
is added (see LinkedIn under Deferred), so putting it in the identity
made it a lie waiting to happen. `ingest` is never a lie, because a
download is an ingest that reaches an origin. Today the tree uses
three words for the first stage — "download" in the crates and the CLI,
"fetch" in the UI, "ingest" in the architecture docs — and this settles
the user-facing ones. Internally: `Phase::Download` becomes
`Phase::Ingest`, `download_only!` becomes `ingest_only!`, and the UI's
`fetch` phase becomes `ingest`. The crate names, the 113 files under
`download/` module directories and the 14 `DOWNLOAD.md` files are *not*
renamed in this plan; that is a separate, purely mechanical PR (slice
5), and until it lands `AGENTS.md` says so in one sentence.

**8. No hierarchy beyond group/function, on disk or in the config.**
Organisational folders ("Work", "Sabbatical") were considered and
deferred. They would be a *mutable* relation (a source can move between
folders), which is why they must not compose into any id, and why they
are a different field from `group` if they ever come: `group` is
identity and a folder is filing. Renameable directories were also
considered and deferred; the precondition is that the index stops
storing root-relative paths (`grid_rows.qmd_path`, `markdowns.md_path`).
See "Deferred" below.

**9. No backwards compatibility.** The loader accepts only the new
shape. `datalib-migrate-config` gains one more rewrite and stays the
only place the old shape is understood. Existing data roots are not
migrated: we have not launched and never promised stable bytes at
rest, so a root written under the old layout is re-synced from
scratch. No tool renames `raw` or `rendered_md` in place.

## The config

```toml
data_root = "~/datalib"

[[groups]]
id = "work-slack"
name = "Work Slack"
type = "slack"

[[steps]]
group = "work-slack"
function = "ingest"
[steps.params.api]
channels = ["chat-qi"]
since = "2026-06-15"
media = true

[[steps]]
group = "work-slack"
function = "render_markdown"
inputs = ["work-slack/ingest"]

[[groups]]
id = "claude"
name = "Claude"
type = "claude"

[[steps]]
group = "claude"
function = "ingest"
[steps.params.api]
conv_uuids = ["f6e7d4bb-991e-433d-91f0-e19b2c8a1e37"]

[[steps]]
group = "claude"
function = "render_markdown"
inputs = ["claude/ingest"]

[[groups]]
id = "fastmail"
name = "Fastmail"
type = "email"

[[steps]]
group = "fastmail"
function = "ingest"
[steps.params]
only_extract_labels = []
[steps.params.jmap]
hostname = "api.fastmail.com"

[[steps]]
group = "fastmail"
function = "render_markdown"
inputs = ["fastmail/ingest"]
[steps.params]
outlink_format = "fastmail"

[[groups]]
id = "pdfs"
name = "Scanned PDFs"
type = "pdf"

[[steps]]
group = "pdfs"
function = "ingest"
[steps.params.fswalk]
path = "~/Documents/scans"

[[steps]]
group = "pdfs"
function = "render_markdown"
inputs = ["pdfs/ingest"]

[[groups]]
id = "unified_index"
name = "Unified Index"

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = ["work-slack/render_markdown", "claude/render_markdown", "fastmail/render_markdown", "pdfs/render_markdown"]

[[steps]]
group = "unified_index"
function = "qmd_index"
inputs = ["work-slack/render_markdown", "claude/render_markdown", "fastmail/render_markdown", "pdfs/render_markdown"]

[[applets]]
group = "unified_index"
id = "unified_index"
command = "datalib-applet unified_index"

[[applets]]
group = "work-slack"
id = "slack_view"
command = "datalib-applet slack"
[applets.params]
tree = "work-slack/render_markdown"
workspace = "Slack"
```

Method table names for the origin-reaching providers exist today (`gmail_api`,
`mbox`; `jmap` replaces email's `sync`). The ones for file-backed
providers (`export`, `fswalk`) are proposed here; today those read
`common.input_path`.

A custom step under a group:

```toml
[[steps]]
group = "work-slack"
function = "embed"
command = "my-embedder --model small"
inputs = ["work-slack/render_markdown"]
```

Its tree is `work-slack/embed`.

A step with no group is still legal for a custom executable, with a
verbatim top-level `id` as today. A `datalib-step` step (no `command`)
requires a group, because it needs the group's type.

## What the loader checks

Same severities as `diagnostics.rs`; the rule for each is the blast
radius.

| check | severity | effect |
|---|---|---|
| `group` names a declared group | error on the step | that step is dropped |
| a step with no `command` has a group | error on the step | dropped |
| `function` contains no `/` and is not empty | error on the step | dropped |
| a group `id` contains no `/` and is not `system` | error on the group | group and every step under it dropped |
| composed step ids are unique | error on the later one | dropped |
| applet ids are unique, as today | error on the later one | dropped |
| a group with no steps and no applets | note | nothing |
| `name` on a step | note ("label comes from the function") | nothing |
| `inputs` names an existing composed id | unchanged from today | unchanged |

An applet's `id` is not composed. It is the mount prefix
(`/applet/<id>/`) and the namespace card source calls, so it is
globally unique on its own, exactly as today. `group` on an applet
says only which row the Manage screen shows it under; an applet writes
no tree and is never an `inputs` target, so there is nothing to
compose into.

## What the runner forwards

To every step under a group, in the environment:

| variable | value |
|---|---|
| `DATALIB_DAG_STEP` | the composed id, and the tree to write (`work-slack/ingest`) |
| `DATALIB_DAG_GROUP` | the group id (`work-slack`) |
| `DATALIB_DAG_GROUP_TYPE` | the group's `type`, when it has one (`slack`) |
| `DATALIB_DAG_FUNCTION` | the function (`ingest`) |

`--params`, `--inputs`, `DATALIB_DAG_INPUTS` and the rest are
unchanged. `--outputs` is dropped: `datalib-step` never read it and the
composed id is the one tree.

**Fingerprint.** The group's `type` goes into every child step's
fingerprint material, so changing it re-runs the group. The group's
`name` is never forwarded and never fingerprinted, so a rename re-runs
nothing. Checked: `grid_rows.source_label` is the provider's name
("Claude"), not the group's, so nothing rendered bakes the human name
in.

## `datalib-step`

- Dispatch on `DATALIB_DAG_FUNCTION` and `DATALIB_DAG_GROUP_TYPE`
  instead of on argv. The `download` / `render` subcommands go;
  `probe` stays as a utility that is not a step.
- Write to the tree `DATALIB_DAG_STEP` names, and read the raw store
  from `DATALIB_DAG_INPUTS`, instead of composing `<name>/raw` and
  `<name>/rendered_md` from a parsed name.
- Delete `source_name()` in `source.rs` and `canonical_rel` in
  `dispatch.rs`. The group id comes from `DATALIB_DAG_GROUP`; the
  storage rows in `introspect.rs` use it.
- The `SourceType` enum becomes the list of data types. The Claude
  provider takes its method from `params.api` / `params.export`, the
  way email already takes `jmap` / `gmail_api` / `mbox`.

## The Manage screen

One row per group, children under a chevron. `ag-grid-enterprise` is
already a dependency (GridCard uses it), so tree data costs nothing.

**Group row aggregation:**

| column | rule |
|---|---|
| Name | the group's `name`; id muted beside it |
| Type | the group's `type` icon; blank for an untyped group |
| Status | running if any child is running; failed if any child failed; otherwise the state of the last child in pipeline order |
| Last synced | the ingest step's, else the newest child's |
| Bytes on disk | the group directory's own series (see below) |
| Actions | Run runs every step in the group; Edit opens the wizard on the group; Delete removes the group, its steps and applets, and unwires its render step from the fan-ins; Reveal opens the group directory |

**Child rows** keep today's per-step run, re-render, reveal and size,
which is what the earlier ungrouping was for. Nothing is lost; it is
one click deeper.

**Bytes and the sparkline.** The usage walker in `http/src/usage.rs`
already does one walk of the root and records a subtotal for every
wanted path on the way back up. Adding each group's directory to the
wanted set gives the group row a measured series of its own, at no
extra walk, and with no summing of two step functions that have
different sample times. It also counts anything else under the folder,
which is the right answer for "what Work Slack weighs".

**Progress.** The group row reuses the segmented bar `StepProgress.vue`
draws for a run: one segment per child in pipeline order, an
indeterminate segment pulsing. No arithmetic across children. Rate and
backlog reporting are follow-ons (below).

**Ungrouped entries** (a custom step with no group) stay top-level rows.

## The wizard

*Built as slice 4b; see "Order of work" for what it settled.*

One dialog creates a group and its two steps. Ingest-phase catalog
fields are shown on the main screen and written to the ingest step;
render-phase fields and presets (`outlink_format` for the email
providers, beeper's `period`, `signal_backup`'s knob) go under a
"Rendering" heading and are written to the render step. There is one
name box, for the group. The "also render this?" chain goes away.

`stemOf`, `renderIdFor`, `phaseOf`, `PHASE_BY_LEAF` and the
`<stem>/raw` fallback in `producerOf` are deleted. `wireIntoFanIns`
keys on the composed render id, which the wizard has because it
composed it.

## What the rename touches

Mechanical, but wide:

- `AGENTS.md` layout table and store-path cheatsheet, both `README`s
  under `etl/`, `docs/dev/grid_rows.md`, `docs/dev/step_protocol.md`,
  `docs/dev/applets.md`, `configs/dag_example.toml`, every file under
  `docs/user/config_examples/`.
- `datalib_step`: `download.rs`, `render.rs`, `synth.rs`,
  `introspect.rs`, `qmd_index.rs`, `dispatch.rs`, `source.rs`,
  `source_type.rs`, `main.rs`.
- `etl/render/src/grid_index.rs`, which walks `<source>/rendered_md`.
- The Claude provider's config crate (one type, two method tables).
- `ui/src/config/sourceSteps.ts`, `catalog.ts`, `pipelineStatus.ts`,
  `SourceWizard.vue`, `Manager2View.vue`, `api.ts`.
- `datalib-migrate-config`.
- The fixture bake (`//tests/fixtures:ingested_tng`) and the
  `schema_inventory` golden.

## Order of work

Each slice is a PR; each leaves the tree green.

1. **Loader + runner.** *Built.* `[[groups]]`, `group` and `function`
   on steps, composition, the checks above, the forwarded environment,
   the fingerprint rule. `--outputs` dropped. `datalib-migrate-config`
   rewrite. Configs and fixtures updated to the new shape with the
   *old* directory names still hardcoded in `datalib-step`, so this
   slice does not move data — which is why the functions in every
   config today are `raw`, `rendered_md`, `grid` and `qmd`, and slice 2
   renames them with the trees. Three departures from the text above:
   - `command` stays required. Decision 4 makes it optional once
     `datalib-step` dispatches on the environment; until slice 2 a
     step with no `command` would run a program that cannot read its
     own function, so the loader still refuses it.
   - `datalib-migrate-config` was cut back to a skeleton rather than
     extended: the pre-TOML YAML era (the stanza schema, the YAML
     steps schema, the tree's last YAML parser) is gone from it, and
     the one rewrite it holds is ungrouped TOML → `[[groups]]`. The
     shape stays so the next rewrite has a home. The http server's
     "stray `config.yaml`" hint and the first-run screen's migration
     branch went with it; a pre-TOML root is set up anew.
   - The wizard writes the new shape now rather than in slice 4, since
     a writer that produced the old one would have had nothing to
     produce it for. The screen is unchanged: still one row per step,
     with the fetch row labelled from the group's name and the render
     row from the same name suffixed, so no test that reads the table
     had to move. `stemOf` and its siblings stay until slice 4.

   Its review added five things keyed on the provider word still being
   on the command, which slice 2 has to revisit because that word
   leaves: `is_ungrouped_builtin` and the warning it drives in
   `accept_steps` (the retired shape becomes a step that cannot run at
   all once dispatch is on the environment, so slice 2 decides whether
   it stays a warning or becomes a rejection); the provider-word check
   in `spec_of` (deleted with the word); `phase_and_type` in
   `config_examples_test.rs` (deleted); the `stepType(command)`
   fallback in `sourceSteps.ts` (deleted — the group's `type` becomes
   the only source); and `convert::builtin_of` (updated, since the
   migrator's one rewrite becomes old shape → grouped *with the new
   function names and no command*, decision 9 allowing one rewrite at
   a time). `PREDATES_GROUPS` in `Manager2View.vue` keyed on
   `!step.group`; slice 2 deleted it, because with the retired shape
   rejected and an ungrouped step never carrying a `type`, no row could
   reach it.
2. **`datalib-step` honors the contract.** *Built.* Dispatch on the
   environment, write to the named tree, read inputs from
   `DATALIB_DAG_INPUTS`, delete `source_name`. `command` becomes
   optional (decision 4) and the provider word leaves it. Vocabulary
   rename lands here, with the fixture re-bake, because this is the
   slice that changes what is on disk. Existing roots are re-synced,
   not migrated (decision 9).

   This slice touches the UI, minimally: every writer of a function
   name or a command — `sourceSteps.ts` (`buildStep`, `PHASE_BY_LEAF`,
   `renderIdFor`), `snippets.ts`, `SourceWizard.vue` — and every test
   fixture that spells `raw` / `rendered_md` (`pipelineStatus.test.ts`
   alone has 19) move to the new names and stop writing the provider
   word. It changes what those files *say*, not how the screen works;
   that is slice 4b's job, and the reason the two are ordered.

   Where it departed from the text above:
   - The loader's check is "a step with no `command` is under a
     group", not "under a group with a `type`": the two index steps
     have no type. `datalib-step` refuses `ingest` and
     `render_markdown` without one, which is the check the table
     wanted, made where the vocabulary is known. The `Function` enum
     in `datalib_step/src/function.rs` is that vocabulary, with a test
     that its spellings are the directory names the render side and
     the index layout use.
   - The retired shape is a **rejection**, not a warning: a
     `datalib-step download|render|grid_index|qmd_index …` command,
     grouped or not, cannot run, so `accept_steps` drops the step and
     names `datalib-migrate-config`. The migrator parses that shape
     itself (the loader no longer hands it over) and its one rewrite
     is old shape → grouped, renamed, command-less, with `inputs` and
     an applet's `tree` following the renamed ids.
   - qmd fixes `qmd/index.sqlite` under whatever `XDG_CACHE_HOME` it is
     given, so the `qmd_index` step's tree is `unified_index/qmd_index`
     and the index sits at `unified_index/qmd_index/qmd/index.sqlite`.
     The step gets its own tree at the cost of one extra directory.
   - `common.raw_path` on an ingest step is refused when it names
     anywhere but the step's tree; a store on another disk is a
     symlink at `<group>/ingest`. A render reads its first input, and
     falls back to `<group>/ingest` with a warning when it declares
     none (the fixture's pre-seeded `yolink`).
   - The two fan-ins still scan the root for `*/render_markdown` rather
     than reading `DATALIB_DAG_INPUTS`; making them input-driven is a
     behaviour change (a source removed from the config would stop
     being swept) and was left for the streaming plan.
   - `Wave::Download` and the `download::` crates keep their names
     (slice 5); `Phase::Download` became `Phase::Ingest`,
     `download_only!` became `ingest_only!`, and the UI's `fetch` phase
     became `ingest`, labelled "Ingest" until slice 4 labels it
     "Download" or "Import".
3. **`type` as data type**, in two halves, because the first is small
   and unblocks a label while the second is a config-shape change
   across every provider:
   - **3a. The `Origin` / `Local` property** — *built (2026-09-10)*.
     Every method table a
     provider's config crate accepts declares itself `Origin` (reaches
     a live service) or `Local` (reads files already on disk), as a
     closed set (`strum`, per `AGENTS.md`), and the catalog mirrors
     the declaration per method. No config shape changes: today's
     tables (`sync`, `gmail_api`, `mbox`, `common.input_path`) get the
     property under their current names. Three readers land with it:
     the Manage row's child label reads "Download" or "Import" instead
     of "Ingest" (`CHILD_LABEL` in `Manager2View.vue`, derived from
     which tables the step's params hold — written config, no
     parsing); `DATALIB_DAG_RESET_AND_REDOWNLOAD` is honoured by an
     `Origin` method and ignored by a `Local` one, which is what the
     protocol doc already says in prose; and `datalib-step` refuses an
     `ingest` step whose params hold no method table at all (decision
     5's check). The wizard's latchkey section gating on `Origin` is
     4b's, since 4b rewrites that dialog.

     Where it departed from that text:
     - The declaration is `impl IngestMethods for <P>Config` in each
       config crate: a list of `IngestMethod { path, reach }`, where
       `path` is a dotted path into the params rather than a table
       name, because one method is a flag — linkedin's `fetch_photos`
       is the only thing that provider fetches from the network, so it
       is declared `Origin` and a step with it on reads "Download"
       (the shape the Deferred section describes). A method is *held*
       when its path is written and its value is neither `null` nor
       `false`. `datalib_step/src/methods.rs` maps a type to its list
       and holds the rule; `datalib_source_common` gained `strum` for
       the enum, so it is no longer serde-only.
     - The catalog does not carry the mirror by hand.
       `ui/src/config/ingestMethods.json` is generated from the
       declarations by
       `bazel run //datalib/backend/datalib_step:ingest_methods.update`,
       a test in `methods.rs` fails when it drifts, and
       `ui/src/config/ingestMethods.ts` applies the same held rule to a
       type and a params tree. 4b's latchkey gating asks `ingestReach`
       there with the params the form would write.
     - **The reset reader was not built, because the tree disagrees
       with the prose it cited.** `test_pipeline_resume_and_reset` in
       `tests/fixtures/ingested_tng_test.py` pins that a reset run
       wipes Signal's `ingested_backups` cursor, and Signal reads a
       backup on disk — a `Local` method. Every file-backed provider
       does the same: pdf and media re-hash, fsindex drops its rescan
       cursor, sms and the Claude export truncate. That is the only
       button a user has for "re-read this from scratch", so `Local`
       keeps honouring the flag; the protocol doc and
       `subprocess.rs` now say "honor it if you bring data in from
       outside the pipeline", which is what the steps do. The planned
       source carries its `reach` and the ingest driver logs it, and
       nothing else acts on it yet. Give it a real reader when 3b
       lands, or drop it then: a field that is only ever logged is
       one the unused-field lint will eventually ask about.
     - Two declarations that are not obvious from the table names:
       beeper's `sync` is `Local` (it reads Beeper Texts' own SQLite),
       and perseus declares both `sync` (`Origin`, TEI files from
       GitHub) and `common.input_path` (`Local`, the staged tree).
       google_takeout's `sync` is *not* a method — it is the feed
       toggles — so only its `common.input_path` is.
     - The step-role glyph's accessible name still says "Ingest": it
       names the phase, which is true of both words.
   - **3b. One type per data shape, one table per method.** `SourceType`
     drops the `_api` suffixes (`slack`, `chatgpt`, `github`, …) and
     `claude_export` folds into `claude`, so a group's `type` names the
     thing mirrored and the render side is a function of it. The ingest
     step's params hold one table per method, named for the method:
     `sync` becomes `api` (or `jmap` for email), `claude`'s export
     becomes `[steps.params.export]`, and every file-backed method
     (`export`, `mbox`, `fswalk` for the three `fswalk` sources, the
     backup readers) carries its own `path` instead of the shared
     `common.input_path`. That is a rewrite of every provider config
     crate, the catalog, every example and fixture config, and the
     fixture bake — and a second config-shape change, so
     `datalib-migrate-config`'s one rewrite becomes "any earlier shape
     → this one": it already parses the pre-`[[groups]]` and slice-1
     shapes, and gains the type and method-table renames. Expect a cold
     CI run: every provider crate rebuilds. `Provider` in
     `schema/src/providers.rs` (the `grid_rows.provider` tag) is a
     separate vocabulary and does not move.
4. **Manage screen and wizard**, in two halves, because the second
   rewrites the files slice 2 renames through:
   - **4a. The tree grid** — *built (2026-09-10)*: one row per group,
     the aggregation table above, the group directory in the usage
     walker's wanted set, the segmented progress bar. Reads phase
     through one helper and writes no config. Depends on slice 1 only
     and runs in parallel with 2; the only file both touch is
     `Manager2View.vue`, where 2 changes a handful of literals. Three
     things to know that the text above does not say:
     - The tree is AG Grid's tree data (`TreeDataModule`, the one
       enterprise module the Manage screen registers); a group row's
       grid key is `group:<id>`, because the `unified_index` applet
       shares its group's id and both are rows. Which groups are open
       is remembered in the browser's `localStorage`, so the remount a
       finished sync does puts the table back the way it was.
     - A step under a group is labelled by its phase — "Ingest" (since
       slice 2; "Fetch" when 4a landed), "Render markdown", "Grid
       index" — with the composed id muted
       beside it; a step's own `name =` is not shown there, which is
       what the loader's note on that key says. The "Download" /
       "Import" word waits for slice 3's `Origin` / `Local`
       declaration. A step outside any group keeps its own name.
     - A group's Edit opened its *fetch step's* form until 4b landed
       the one dialog. The aggregation rules live in
       `ui/src/config/groupRows.ts`, tested without a grid; a failed
       applet counts as a failed child, since the group row is the
       only place its health shows while the group is folded.
   - **4b. The one-dialog wizard** — *built (2026-09-10)*: group + both
     steps from one form, render fields under a "Rendering" heading,
     one name box; `stemOf`, `phaseOf`, `renderIdFor`, `PHASE_BY_LEAF`
     and the `<stem>/raw` fallback in `producerOf` deleted; the
     `sourceSteps.ts` header rewritten. It came after slice 2 because
     it rewrites `SourceWizard.vue` and `sourceSteps.ts`, which is
     where slice 2's rename landed. Where it departed from the text
     above, and what it settled that the text left open:
     - A step's phase is read off its `function` (`ingest`,
       `render_markdown`, `grid_index`, `qmd_index`; anything else is
       a custom step), never off the shape of its id. The grid's
       Source column joins `source_name` to a *group's* name the same
       way, so nothing in the UI splits an id any more.
     - The "Render to markdown" row action went with the chain: a
       source's render step is written with its ingest step, and Edit
       on the group row *or on any step under it* opens the one form.
       A hand-edited source missing one of its two steps gets it back
       on save, and the dialog says so before Save is pressed. A
       provider that renders nothing (`renderStep: false`) is written
       as one step and shows no Rendering heading.
     - Edit replaces both steps in one cut-and-append
       (`replaceSteps`); the group is renamed in place. Cutting and
       appending one step at a time would use offsets the first cut
       had shifted.
     - `producerOf` follows a render step's `inputs`, then falls back
       to the ingest step filed under the same *group* — the fallback
       `datalib-step` itself makes — rather than to a stem split.
     - The probe runs once, with the ingest step's params as the form
       would write them, and fills the render step's pickers too;
       the wizard no longer threads a `downloadParams` copy between
       two dialogs.
     - The latchkey / credentials section still shows as it did
       before 3a: gating it on `ingestReach` (`ui/src/config/ingestMethods.ts`)
       for an `Origin` method is the one piece of 4b that waited for
       3a, and the two landed side by side. It is the next UI edit.

5. **Mechanical rename** (optional, after 3b): crate names,
   `download/` module directories, `DOWNLOAD.md` files, and the
   `AGENTS.md` section "Download and render are separate crates", all
   to "ingest". `git mv` plus `sed`, no logic, reviewed as "does it
   build". Slice 3b already rebuilds every provider crate, so the CI
   cold-run cost is paid either way; isolating this slice is about
   review noise, not build time. After 3b rather than before it, so
   the `git mv` does not land on files 3b is rewriting.

With 1, 2, 3a, 4a and 4b in the tree, what is left runs as: the
wizard's latchkey gating (above), then **3b**, then **5**. 3b is the
last config-shape change, so the migrator's rewrite is settled once it
lands. When 3b renames the tables, the `IngestMethods` lists are where
the new names go, and the mirror is regenerated rather than edited.

## Deferred

Recorded so the reasoning is not lost, not so it is built.

- **Bootstrap from an export, then sync by API.** Not config. A step
  describes a steady state, and "read this export once, then never
  again" is a one-time action; encoding it as a params table with a
  `once` key puts state where config belongs. The repo already has the
  right shape for one-shot operations — `DATALIB_DAG_RESET_AND_REDOWNLOAD`
  and `REFETCH_BLOBS` are flags on a run, not keys in the file — and a
  bootstrap is the same kind of thing. So: the group is configured for
  its steady state only (`[steps.params.api]`), and seeding it is a
  job, run from the Manage screen or as
  `datalib-step import --into claude/ingest --from ~/export`, while
  the pipeline is not running so the one-writer rule holds. It adds
  what is missing and never prunes; afterwards the ordinary ingest
  step continues from what is now in the store. The `export` method
  table stays for the person who only has an export: that is a steady
  state, the export is the truth, and pruning to its snapshot is
  correct there. The destructive edge in Claude's `DOWNLOAD.md`
  ("Bootstrapping from an export") only exists because one code path
  serves both jobs today.
- **A download that consumes an ingest.** LinkedIn ingests an
  export's CSVs and then, when `fetch_photos` is on, fetches each
  connection's public profile photo into the same raw store
  (`download/photos.rs`). Under decision 7 that is one `ingest` step
  whose row reads "Download" whenever the photos method is on, which
  is true. The reason to factor it anyway: `linkedin/ingest` (CSVs to
  entities) and a second step, say `linkedin/photos`, with
  `inputs = ["linkedin/ingest"]` and one `Origin` method, reading
  connections from a cursor and writing its own entity store (the
  `contact_photos` edges) and its own blob CAS. That makes the "only
  copy" property per tree and lets reset-and-redownload hit the photos
  without re-reading the export. A provider-specific function name
  means `datalib-step` dispatches on more than the four it knows
  today, which is part of what this defers.
  The same shape covers any enrichment fetched for ingested data
  (link previews, avatars). It needs a render step that reads two
  input trees; the runner already passes `DATALIB_DAG_INPUTS` as a
  list, but `datalib-step` render assumes one raw store per source.
- **Folders.** A `parent_id` on a *group*, mutable, never composed
  into any id, shown as a tree in the Manage screen. Different field
  from `group` because it is a different kind of relation.
- **Renameable directories.** Needs the index to hold
  `(step id, tree-relative path)` instead of root-relative paths, a
  `dir` field on the group that the app owns, and a marker file in the
  tree for repair. Until then a group's directory is fixed at creation,
  exactly as today, and a stale folder name is a stale hint rather than
  a bug.
- **Rate and backlog.** `progress_length` gains a unit so a step can
  say it counts bytes, entities or pages, and the runner keeps a short
  rate window per step: a running step with a flat counter is a stall.
  A consumer reports `progress_backlog {pending}` from the count it
  already has in hand when it scans `dolt_diff` from its cursor. That is
  the USE view of a streaming pipeline: utilisation is a moving counter,
  saturation is the backlog ahead of each consumer, errors are
  `FailureKind`. The backlog must be "rows past my cursor", never
  "absent versus empty" (see the sink contract in
  [`streaming_steps_plan.md`](streaming_steps_plan.md)).
- **ETA.** `LastRun` already records `started_at` and `finished_at`, so
  a queued step's last duration is known. Not built until rate and
  backlog have shown what they cover.
