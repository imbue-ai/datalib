# Editing the datalib data-source config (agent guide)

You were pointed here by a "wayfinder" snippet copied out of the
datalib UI's Manage tab. It asked you to modify the data-source
config. This doc tells you how. (If your wayfinder named a component
alias instead, read `<origin>/agent/cards.md`.)

## The model

The sync pipeline is driven by `<root>/config.toml`, which holds two
kinds of entry. `[[steps]]` is the pipeline: each step has an `id`, a
shell `command`, and declared `inputs` / `outputs` (artifact paths;
wildcards allowed in inputs). `[[applets]]` is the app surface —
long-lived servers that contribute card components and the endpoints
behind them, declaring no inputs/outputs because they read what steps
wrote. This guide is about steps; for applets see
`docs/dev/applets.md`. The runner derives the execution DAG from input/output overlap
— file order does not matter. A step with no `inputs` is a **source**
(what a sync can target); every source's rendered markdown feeds the
shared `grid_index` / `qmd_index` fan-in steps:

```toml
# One source = a download step + a render step. The download step has
# no inputs (that makes it a source); the source's name comes from its
# first output ("slack/raw" → slack). `params` carries per-provider
# config; credentials never live here (latchkey provides them at
# runtime).
[[steps]]
id = "slack.download"
command = "datalib-step download slack_api"
outputs = ["slack/raw"]
# A sub-table ends the table it sits in, so `params` goes after this
# step's plain keys — and the next step starts with its own [[steps]].
[steps.params]
sync = {}

[[steps]]
id = "slack.render"
command = "datalib-step render slack_api"
inputs = ["slack/raw"]
outputs = ["slack/rendered_md"]

# Shared fan-in steps every source's rendered markdown feeds.
[[steps]]
id = "grid_index"
command = "datalib-step grid_index"
inputs = ["**/rendered_md"]
outputs = ["system/backend_index"]

[[steps]]
id = "qmd_index"
command = "datalib-step qmd_index"
inputs = ["**/rendered_md"]
outputs = ["system/qmd"]
```

Any top-level keys (`data_root`, `binary_dir`) must be written *above*
the first `[[steps]]`.

## What you do

Work through the HTTP API, not the file:

```sh
# read the current config
# (JSON: {text, path, exists, parsed_ok, error, …})
curl "<origin>/api/config"

# save a new version — send the FULL new text, not a diff
curl -X PUT "<origin>/api/config" \
  -H 'content-type: application/json' \
  -d "$(jq -Rs '{text: .}' < config.toml)"
```

The PUT validates with the real config loader before writing anything:
an invalid config returns `{ok: false, error}` and leaves the file on
disk untouched — fix and re-PUT. Only a valid config ever lands.

The config is always TOML — the server reads and writes no other
format. If `GET /api/config` comes back with `exists: false` and a
non-null `legacy_yaml_path`, that data root still has a pre-TOML
`config.yaml`, which nothing reads any more. Tell the user to convert
it once with `datalib-migrate-config <data_root>`; there is no API for
it, and you should not try to translate the file yourself.

## Adding your own step commands

A step's `command` is an ordinary command line — it is not limited to
the built-in `datalib-step` subcommands. If the user's request needs a
new program (a custom fetcher, a converter, …), write it and install
it into **`~/.datalib/bin`** — either the binary itself or a symlink
to wherever it lives:

```sh
mkdir -p ~/.datalib/bin
ln -sf /path/to/my-fetcher ~/.datalib/bin/my-fetcher
```

`~/.datalib/bin` is prepended to `PATH` whenever the UI runs the
pipeline, so a bare `command = "my-fetcher --out ."` resolves. Keep step
commands non-interactive; they run headless with their output captured
into the job log.

Steps run with the data root as their working directory, and their
declared `outputs` are what downstream steps' `inputs` match against —
a new source should ultimately produce rendered markdown under
`<name>/rendered_md` so the shared index steps pick it up.

## Checking your work

- On every successful PUT the user's config editor (the Manage tab)
  reloads automatically — there is nothing to refresh manually.
- `GET <origin>/api/dag` returns the step DAG the saved config
  produces (`{ok, error, steps: [{id, command, inputs, outputs,
  deps}]}`), in topological order — use it to confirm the wiring you
  intended.
- `GET <origin>/api/sync/sources` lists the source steps a sync can
  target, as derived from the saved config.
