# Editing the datalib data-source config (agent guide)

You were pointed here by a "wayfinder" snippet copied out of the
datalib UI's Manage tab. It asked you to modify the data-source
config. This doc tells you how. (If your wayfinder named a component
alias instead, read `<origin>/agent/cards.md`.)

## Authentication (do this first)

Every `/api/*` route requires the server's API token — the same scheme
Jupyter uses, and for the same reason: without it any web page the user
has open could rewrite this config and run the commands in it. (This
guide itself is public, so you can read it before you have the token.)

The token is minted per server process and published to a file:

```sh
TOKEN=$(cat <data root>/system/api-token)
curl -H "Authorization: Bearer $TOKEN" "<origin>/api/health"
```

Your wayfinder names the exact path. **Read it fresh** rather than
caching it: a 401 from any call below almost always means the server
restarted and minted a new one.

## The model

The sync pipeline is driven by `<root>/config.toml`, which holds three
kinds of entry. `[[groups]]` is what a person sees as one thing: an
`id` (one directory name), a `name`, and for a source a `type`.
`[[steps]]` is the pipeline: each step names its `group` and the
`function` it performs there and the `inputs` it reads; its id is
composed as `<group>/<function>` — the one tree it writes — and is
never written. A step with no `command` is a built-in one (the
functions `ingest`, `render_markdown`, `grid_index` and `qmd_index`,
run by `datalib-step`); a custom step names a shell `command`. `[[applets]]` is the app surface —
long-lived servers that contribute card components and the endpoints
behind them, filed under a group but declaring no inputs because they
read what steps wrote. This guide is about groups and steps; for
applets see `docs/dev/applets.md`. Edges are the declared `inputs`,
which name steps by composed id — file order does not matter. A step
with no `inputs` is a **source step** (what a sync can target); every
source's rendered markdown feeds the two fan-in steps under the
`unified_index` group:

```toml
# One source = a group with a `type`, plus an ingest step and a
# render step under it, neither with a `command`. The ingest step has
# no inputs (that makes it a source step). `params` carries
# per-provider config; credentials never live here (latchkey provides
# them at runtime).
[[groups]]
id = "slack"
name = "Work Slack"
type = "slack"

[[steps]]
group = "slack"
function = "ingest"
# A sub-table ends the table it sits in, so `params` goes after this
# step's plain keys — and the next entry starts with its own [[…]].
[steps.params]
sync = {}

[[steps]]
group = "slack"
function = "render_markdown"
inputs = ["slack/ingest"]

# The shared fan-in steps every source's rendered markdown feeds. Add a
# source's render step id to both `inputs` lists.
[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = ["slack/render_markdown"]

[[steps]]
group = "unified_index"
function = "qmd_index"
inputs = ["slack/render_markdown"]
```

Any top-level keys (`data_root`, `binary_dir`) must be written *above*
the first `[[…]]` header.

## What you do

Work through the HTTP API, not the file:

```sh
# read the current config
# (JSON: {text, path, exists, parsed_ok, error, …})
curl -H "Authorization: Bearer $TOKEN" "<origin>/api/config"

# save a new version — send the FULL new text, not a diff
curl -X PUT "<origin>/api/config" \
  -H "Authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d "$(jq -Rs '{text: .}' < config.toml)"
```

The PUT validates with the real config loader before writing anything:
an invalid config returns `{ok: false, error, diagnostics}` and leaves
the file on disk untouched — fix and re-PUT. Only a valid config ever
lands, so a successful PUT means the whole file is good.

**Read `diagnostics`, not just `error`.** It is the full list, one entry
per problem, each with `severity`, `message`, `help`, and a `line` —
so one round-trip tells you everything wrong with the file instead of
one problem per attempt. `error` is only the first of them.

To check text without saving it, `POST /api/config/check` with the same
body; it returns the same shape and writes nothing.

From a terminal, `datalib-dag --check <data_root>/config.toml` prints
the same diagnostics as `file:line:col: severity: message`, with the
offending line and a `help:` line under each. Exit 0 clean, 1 if the
file is not a config at all, 2 if some entries were dropped.

### A file already on disk is treated more leniently than your PUT

Worth knowing, because the two doors deliberately disagree. The loader
that *reads* `config.toml` keeps whatever loads: an entry it cannot use
is dropped, named in `diagnostics`, and everything else still runs.
That exists so one stray key cannot cost the user their whole app.

The PUT does not do that — it refuses anything with a problem. So if
`GET /api/config` shows a non-empty `diagnostics` with `parsed_ok:
true`, you are looking at a file someone hand-edited into a partly
broken state; the app is running on the rest of it, and your PUT will
not be accepted until you fix the entries it names.

`app_ready` on `GET /api/config` is the separate question of whether the
app can serve anything at all: false when the file is not a config, or
when it declares no `unified_index` applet. The UI blocks on it.

The config is always TOML in the shape above — the server reads and
writes no other. A `config.toml` written before `datalib-step` read its
function from the environment (steps with a `datalib-step download …`
or `datalib-step render …` command, grouped or not) is refused, and the
diagnostic says so; tell the user to rewrite it once with
`datalib-migrate-config <data_root> --force`. There is no API for it,
and you should not try to translate the file yourself.

## Adding your own step commands

A step may name a `command`: an ordinary command line, for a step that
is not one of the built-in functions. If the user's request needs a
new program (a custom fetcher, a converter, …), write it and install
it into **`~/.datalib/bin`** — either the binary itself or a symlink
to wherever it lives:

```sh
mkdir -p ~/.datalib/bin
ln -sf /path/to/my-fetcher ~/.datalib/bin/my-fetcher
```

`~/.datalib/bin` is prepended to `PATH` whenever the UI runs the
pipeline, so a bare `command = "my-fetcher --out ."` resolves. The same
applies to an `[[applets]]` command, so one install location covers both
kinds of entry. Keep step commands non-interactive; they run headless
with their output captured into the job log.

Steps run with the data root as their working directory and write the
one tree their id names, which is what downstream steps' `inputs` name
— a new source should ultimately produce rendered markdown under
`<group>/render_markdown`, and that id added to the index steps' `inputs`,
so the shared index steps pick it up.

## Checking your work

- On every successful PUT the user's config editor (the Manage tab)
  reloads automatically — there is nothing to refresh manually.
- `GET <origin>/api/dag` (with the `Authorization` header) returns the
  step DAG the saved config
  produces (`{ok, error, steps: [{id, command, inputs, outputs,
  deps}]}`), in topological order — use it to confirm the wiring you
  intended.
- `GET <origin>/api/sync/sources` lists the source steps a sync can
  target, as derived from the saved config.
