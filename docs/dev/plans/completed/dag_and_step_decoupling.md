# Decoupling datalib-dag from datalib-step

**Status: landed in #1894 (2026-11-19), kept as the record.** The
protocol below is described as it is now in
[`docs/dev/step_protocol.md`](../../step_protocol.md), and the runner's
rules in [`datalib/backend/dag/README.md`](../../../../datalib/backend/dag/README.md).
The binary has since been renamed `datalib-pipeline` (#1921). What this
plan left for later is in
[`decoupling_followups.md`](../decoupling_followups.md).

## The goal

`datalib-dag` is a general pipeline supervisor: it starts processes,
each of which owns one directory under the data root, runs a step again
when what it reads changes, keeps steps apart when running them
together would be unsafe, and records what each step reports. It never
opens a step's files.

**The runner gives `datalib-step` nothing another program cannot ask
for with the same config keys.** Everything the runner does with a step
depends on that step's config entry and on facts every step has, never
on the program's name or the step's id. New runner behaviour is a
protocol extension (a config key, a verb, an event or an environment
variable) that any program may adopt, and `datalib-step` is then taught
to use it.

## The protocol

### A step's config entry

| key | what the runner does with it |
|---|---|
| `id` | Names the directory `<root>/<id>/`. Only this step's processes may write there; the loader refuses ids that are equal or nested. The first part of the id is the step's group. |
| `command` | Required. An argv, split shell-style and started with `execvp`, in its own process group, with the data root as the working directory, `/dev/null` on stdin and the runner's pipe on fd 3. Everything the program needs to know about what to do goes here. |
| `inputs` | Step ids. The step may read `<root>/<input>/`, waits for each input's first output, and runs again when an input's version changes. |
| `params` | Any TOML, passed as JSON in `DATALIB_DAG_PARAMS`. The loader refuses params over 64 KiB. |
| `env` | Variables added to the child's environment. |
| `reads` | `"files"` (the default): no producer of an input runs while this step does, in either order. `"pinned"`: the step may run while a producer writes, and reads only the version `DATALIB_READS` names. |
| `start` | `"on_each_seal"` (the default) or `"after_inputs_settle"`. Only matters with `reads = "pinned"`. |
| `locks` | Named counting semaphores; the step holds one slot of each (or all, `"exclusive"`) while its process lives. A step that names none holds `network` if it has no inputs and `cpu` if it has some. |
| `code_version` | An opaque string in the fingerprint. |

The fingerprint covers `id`, `command`, `inputs`, `params`, `env` and
`code_version`. It does not cover `reads`, `start` or `locks`.

### Top-level entries

| entry | meaning |
|---|---|
| `[[groups]] id, name, type, description` | Display metadata for every step whose id starts with `<id>/`. The runner checks that an id is unique and does nothing else with it; `type` is opaque. |
| `[[locks]] name, slots` | A semaphore. The runner itself defines only `network` (4 slots) and `cpu` (4). |
| `[[programs]] command, verbs` | The verbs a program takes. A step's program is the first word of its `command`, compared as written. |
| `[env]` | Variables handed to every step, below each step's own `env`. |
| `log_level`, `[disk_space]`, `binary_dir`, `data_root` | The runner's own settings, as now. |

### What runs

```
cd <root> && <command…> [--<verb>]
```

The environment:
- `DATALIB_DAG_STEP` (the id) and `DATALIB_DAG_DATA_ROOT`;
- `DATALIB_DAG_PARAMS`, when the step has params;
- `DATALIB_DAG_INPUTS`, `DATALIB_DAG_CHANGED_INPUTS`,
  `DATALIB_DAG_FAILED_INPUTS` and `DATALIB_READS`;
- `DATALIB_DAG_ATTEMPT`, `DATALIB_DAG_RUN_ID`, `DATALIB_DAG_NOW` and
  `DATALIB_PARENT_PIPE`;
- `RUST_LOG`, `PATH`, `TMPDIR`, the allowlist, `[env]` and the step's
  `env`.

What the step sends back (exit status, the NDJSON events on stdout,
stderr) and how it is stopped (SIGINT, then SIGKILL after 15 s) do not
change. One event field is added: `capabilities` may carry `version`,
the program's own commit, which the run store records for the process
in place of the runner's guess.

### Verbs

The runner appends a verb only for a step whose program's
`[[programs]]` entry lists it. A program that ignores its arguments is
never sent one.

| verb | sent when | if the program does not take it |
|---|---|---|
| `--reset` | someone resets the step | the runner deletes `<root>/<id>/` itself |
| `--prepare` | each time the runner starts, before the step's first run | nothing |
| `--compact`, `--collect` | someone asks to free disk space | nothing |

`--prepare` replaces `--migrate`. The step answers in its outcome:
nothing to do; brought its output to this program's shape (reports the
new version); `needs_rerun`; or a failure of kind `incompatible`, when
its output was written by a newer build. The runner turns a step that
refused off, and turns it back on at a later start once it no longer
refuses.

## What changes, from the tree as it is

Paths are relative to the repo root; the line numbers are from
2026-11-18.

**The runner** (`datalib/backend/dag`)
1. `command` is required; the `datalib-step` default goes
   (`src/config.rs:1932`, `:1985`, `:615`).
2. The defaults inferred from program and function name go:
   `UNPINNED_BUILTINS`, `BUILTIN_LOCKS` and `migrates`/`compacts`
   (`src/config.rs:1938-1953`, `:2017-2029`).
3. `qmd_keyword`, `qmd_embed` and `index` stop being locks every config
   has (`src/supervisor/locks.rs:65-93`), and the default lock depends
   on inputs only.
4. `reads` defaults to `"files"`.
5. Group `type` is opaque. The diff rules, the reserved group ids, the
   time-series constants and `[[groups]] source` go from the loader
   (`src/config.rs:380`, `:741-754`, `:1286`, `:1332-1370`,
   `:1467-1490`).
6. The checks of `datalib-step`'s own configuration go
   (`src/config.rs:1573`, `:1663`, `:1680`, `:1709`, `:1781`, `:1802`),
   and so does `--check` running `datalib-step check-params`
   (`src/bin/datalib_dag.rs:450-465`).
7. `render_pages`, `embed_contexts` and `checkpoint_cadence` leave the
   top level (`src/config.rs:64`, `:85`, `:90`); `[env]` is new.
8. Two backup secrets stop being passed to `datalib-step` only
   (`src/subprocess.rs:289`), and the `builtin` flag that gave the
   runner's commit to `datalib-step` processes goes
   (`src/scheduler.rs:297-300`).
9. The starter config moves to `datalib-http` (`src/starter.rs`), and
   help text names no program.
10. The launch's newer-build check (`src/bin/datalib_dag.rs:127`) goes;
    each step's `--prepare` refuses instead.
11. `--params-file`, `--inputs`, `DATALIB_DAG_GROUP`,
    `DATALIB_DAG_GROUP_TYPE`, `DATALIB_DAG_FUNCTION` and
    `DATALIB_DAG_SOURCE_GROUP{,_TYPE}` go; `DATALIB_DAG_PARAMS` is new.
12. `[[programs]]` and the verb rules are new; the migrate pass becomes
    a prepare pass run at every start.
13. `group` and `function` on a step go; a step writes `id`, and its
    group is the id's first part.

**`datalib-step`** reads its function and its group's type from argv:

```
datalib-step run <function> [<type>] [--source-type <type>]
```

`<type>` is its group's `type`, written for every step under a typed
group; `--source-type` is the source's type, under a diff group. It reads its
params from `DATALIB_DAG_PARAMS`, its knobs from
`DATALIB_STEP_RENDER_PAGES`, `DATALIB_STEP_EMBED_CONTEXTS` and
`DATALIB_STEP_CHECKPOINT_CADENCE`, and the two backup secrets from
the variables its params name (`backup.key_env_var`,
`backup.aep_env_var`), which the runner hands every step whose params
name a variable. `datalib-http` also hands both variables, when it was
started with them, to every step it runs, so a source the wizard wrote
with the default names works in the app. It takes `--prepare` in place
of `--migrate`; an ingest's answer first compares the raw store's
`_datalib_meta` with this build's, so a store already in shape costs one
read. Its checks of its own configuration stay where they run now:
`check-params`, which `datalib-http` runs on save.

## Existing configs

`datalib-http`'s config upgrade, and `datalib-migrate-config` from a
terminal, rewrite a config in the old shape. A config is in the old
shape when any of these holds:
- a `[[steps]]` entry has `group` or `function`;
- a step's `command` is `datalib-step` (or a path ending in
  `/datalib-step`) without `run` as its next word, or is missing;
- the file has a top-level `checkpoint_cadence`, `render_pages` or
  `embed_contexts`;
- a `[[groups]]` entry has `source`.

On each step:
- `group` + `function` become `id = "<group>/<function>"`.
- A step that runs `datalib-step` (or has no `command`) gets
  `command = "datalib-step run <function>"`, followed by ` <type>`
  when its group has a `type`, and by ` --source-type <t>` when that
  type is `diff`, `<t>` being the type of the group its `source` names.
  The program word is kept as written.
- Where the step writes no `reads`: a `datalib-step` step other than
  `ingest` gets `reads = "files"` when its function is `keyword_index`,
  `embed` or `embedding_map`, or it is `render_markdown` under a group
  of type `perseus`, and `reads = "pinned"` otherwise, whether or not
  its `inputs` are empty yet (a fan-in's are, until a source is added).
  A custom step with `inputs` gets `reads = "pinned"`.
- Where the step writes no `locks`: a `datalib-step` step gets
  `locks = ["qmd_keyword"]` when its function is `qmd_aggregator` or
  `keyword_index`, and `locks = ["qmd_embed"]` when it is `embed`. Any
  other `datalib-step` step under a group with no `type` other than an
  `ingest` gets `locks = ["index"]`, inputs or not; a custom step with
  inputs whose group has no `type` (or that had no group) gets
  `locks = ["index"]`. Every other step keeps the default.

On the file:
- a `[[programs]]` entry `command = "datalib-step"`,
  `verbs = ["reset", "prepare", "compact", "collect"]`, for each
  spelling of the `datalib-step` program the steps use;
- `[[locks]]` entries `qmd_keyword` (1 slot), `qmd_embed` (1 slot) and
  `index` (2 slots) for each one some step holds and the file does not
  declare;
- `[env]`: `DATALIB_STEP_CHECKPOINT_CADENCE` (the seconds, from
  `checkpoint_cadence.at_most_every_secs`),
  `DATALIB_STEP_RENDER_PAGES` (`"<buckets>,<blob_mb>"`, either side
  empty when left out) and `DATALIB_STEP_EMBED_CONTEXTS`, from the
  three top-level keys, which are removed;
- a group's `source` is removed;
- a WhatsApp or Signal ingest whose `[steps.params.backup]` names no
  variable gets the default one written there
  (`key_env_var = "WHATSAPP_BACKUP_DECRYPTION_KEY"`,
  `aep_env_var = "SIGNAL_BACKUP_PASSPHRASE"`), so `datalib-dag` run
  from a terminal still hands the step its secret.

A custom step loses only `group` and `function` for `id`, and gets the
`reads` and `locks` above, so it behaves as before. One change it may
notice: `--reset` is sent only to a program a `[[programs]]` entry
says takes it, and has no `store` argument any more; any other step's
directory is deleted instead.

**The upgrade and the runner change land together.** The loader
refuses the old shape, naming the rewrite, so no config reaches a
runner that no longer infers locks without having them written. If it
did, qmd steps would race on one index file and corrupt it.

Every step's fingerprint changes once (its `command` gains words), so
every step runs once at the next sync. `datalib-step` works from its
own cursors and ignores `DATALIB_DAG_CHANGED_INPUTS`, so each does only
what is new.

## Proving it

- A test runs `datalib-dag` on a config of shell-script steps with no
  `datalib-step` anywhere: a run, a reset of a step whose program takes
  no verbs, and a start.
- The upgrade of every config in `configs/`, `docs/user/config_examples/`
  and the providers' `examples/` produces the locks, reads and verbs
  the old inference gave the original.
- `//:lint_repo` refuses `datalib-step` in `datalib/backend/dag/src`
  outside tests.

## Set aside

- **The qmd index writes outside its steps' directories.** Every
  source's `keyword_index` and `embed` write
  `unified_document_index/qmd_aggregator/qmd/index.sqlite`. Nothing
  enforces "write only under your own directory", so the rule stays a
  rule of the protocol and qmd is a known exception until it is
  replaced. The upgrade writes its locks.
- **Checking params before a run** stays a `datalib-step` subcommand
  that `datalib-http` runs: [`decoupling_followups.md`](../decoupling_followups.md)
  item 1.
