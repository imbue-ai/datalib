# Editing the Datalib data-source config (agent guide)

This doc tells you how to change a Datalib app's data-source config:
which sources it syncs and the steps that turn them into the grid and
search. (Building a card instead? Read `<origin>/agent/cards.md`.)

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

`<data root>` is the directory `config.toml` sits in. **Read the
token fresh** rather than caching it: a 401 from any call below almost
always means the server restarted and minted a new one.

## The model

The sync pipeline is driven by `<root>/config.toml`, which holds three
kinds of entry. `[[groups]]` is what a person sees as one thing: an
`id` (one directory name), a `name`, and for a source a `type`.
`[[steps]]` is the pipeline: each step has an `id` — the one tree it
writes, whose first part is its group — the `command` it runs, and the
`inputs` it reads. Datalib's own steps run `datalib-step run <function>
[<type>]` (the functions `ingest`, `render_markdown`,
`render_time_series`, `render_occurrences`, `embed`,
`unified_grid_and_search_index`, `embedding_map` and
`time_series_index`), with an id of `<group>/<function>` and the
group's `type` as `<type>`; a custom step names its own program.
`[[applets]]` is the app surface — long-lived servers that contribute
card components and the endpoints behind them, filed under a group but
declaring no inputs because they read what steps wrote. This guide is
about groups and steps; for applets see `datalib-step help applets`.
Edges are the declared `inputs`, which name steps by id — file order
does not matter to the runner. It does to a person: the Sources screen
lists groups and steps in file order, and writes them in the order data
flows, each source above the `unified_grid_and_search_index` group and
each step below the steps it reads. Write yours the same way. A step
with no `inputs` is a **source step** (where a sync normally starts);
every source feeds the fan-in step under the
`unified_grid_and_search_index` group.

Each `datalib-step` step also says how it reads its inputs and which
locks it holds, because the runner works out nothing from the program's
name: copy `reads` and `locks` exactly as below. `locks` keeps the
embeddings from loading the model twice at once, and the index steps to
a budget of their own, and every lock a step holds needs a `[[locks]]`
entry. The `[[programs]]` entry says which requests (a reset, the check
at each start, freeing disk) `datalib-step` understands. A config the
app wrote already has all three entries at the top:

```toml
[[programs]]
command = "datalib-step"
verbs = ["reset", "prepare", "compact", "collect"]

[[locks]]
name = "index"
slots = 2

[[locks]]
name = "embedder"
slots = 1

# One source = a group with a `type`, plus an ingest step and a
# render step under it. The ingest step has no inputs (that makes it a
# source step). `params` carries per-provider config; credentials never
# live here (latchkey provides them at runtime).
[[groups]]
id = "slack"
name = "Work Slack"
type = "slack"

[[steps]]
id = "slack/ingest"
command = "datalib-step run ingest slack"
# A sub-table ends the table it sits in, so `params` goes after this
# step's plain keys — and the next entry starts with its own [[…]].
# The one table under it names how the data is reached: `api` is the
# product's own API, and an empty one takes the provider's defaults.
[steps.params.api]

[[steps]]
id = "slack/render_markdown"
command = "datalib-step run render_markdown slack"
inputs = ["slack/ingest"]
reads = "pinned"

# The source's search by meaning: the embeddings of what
# `unified_grid_and_search_index` put in the keyword search (the slow
# one). Leave it out and keyword search still finds the source.
[[steps]]
id = "slack/embed"
command = "datalib-step run embed slack"
inputs = ["unified_grid_and_search_index/unified_grid_and_search_index"]
reads = "files"
locks = ["embedder"]

# The shared fan-in step. It reads every source's render by pattern, so
# a source added above needs no edit here: `*/render_markdown` is every
# source's render. `unified_grid_and_search_index` puts every source it reads in the
# keyword search but those it lists in `params.not_searched`, and keeps
# only the embeddings a chunk still names. Name renders one by one
# instead to leave a source out of the grid on purpose.
[[groups]]
id = "unified_grid_and_search_index"

[[steps]]
id = "unified_grid_and_search_index/unified_grid_and_search_index"
command = "datalib-step run unified_grid_and_search_index"
inputs = ["*/render_markdown"]
reads = "pinned"
locks = ["index"]
```

Any top-level keys (`data_root`, `binary_dir`) must be written *above*
the first `[[…]]` header. A `[disk_space]` table moves the free-space
floor: under `pause_below_bytes` (default `"10 GB"`) no step runs and
running ones are stopped, until the disk is back to `resume_at_bytes`
(default `"15 GB"`).

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
the file on disk untouched — fix and re-PUT. A PUT writes only a
config whose every entry loads; a warning comes back in `diagnostics`
but does not stop the write.

**Read `diagnostics`, not just `error`.** It is the full list, one entry
per problem, each with `severity`, `message`, `help`, and a `line` —
so one round-trip tells you everything wrong with the file instead of
one problem per attempt. `error` is only the first of them.

To check text without saving it, `POST /api/config/check` with the same
body; it returns the same shape and writes nothing.

From a terminal, `datalib-dag --check <data_root>/config.toml` prints
the same diagnostics as `file:line:col: severity: message`, with the
offending line and a `help:` line under each. Exit 0 when every entry
loads, 1 if the file is not a config at all, 2 if some entries were
dropped. It does not check a step's params; the PUT and
`POST /api/config/check` do, for every `datalib-step` step.

### A file already on disk is treated more leniently than your PUT

Worth knowing, because the two doors deliberately disagree. The loader
that *reads* `config.toml` keeps whatever loads: an entry it cannot use
is dropped, named in `diagnostics`, and everything else still runs.
That exists so one stray key cannot cost the user their whole app.

The PUT does not do that — it refuses any config that would drop an
entry. So if `GET /api/config` shows `diagnostics` of severity
`rejected` or `blocked` with `parsed_ok: true`, you are looking at a
file someone hand-edited into a partly broken state; the app is running
on the rest of it, and your PUT will not be accepted until you fix the
entries it names. (A `warning` drops nothing and does not block a PUT.)

`app_ready` on `GET /api/config` is the separate question of whether the
app can serve anything at all: false when the file is not a config,
when it declares no `unified_grid_and_search_index` applet, or when the data root was
written by a newer Datalib. The UI blocks on it.

The config is always TOML in the shape above — the server reads and
writes no other. A config in the oldest retired shape (steps with a
`datalib-step download …` or `datalib-step render …` command) is
refused, and the diagnostic says so; tell the user to rewrite it once
with
`datalib-migrate-config <data_root> --force`. There is no API for it,
and you should not try to translate the file yourself. Every other
older shape the server rewrites itself as soon as it reads the file,
keeping the old text as `config.toml.bak`, so re-read the config after
writing one: steps written as `group` + `function`, a bare `command =
"datalib-step"`, the top-level `checkpoint_cadence`, `render_pages` and
`[modal]` (which become variables in `[env]`), a `qmd_index`,
`keyword_index` or `qmd_aggregator` step (each source's `embed` and
`unified_grid_and_search_index` do that work, and a source that was not
keyword-indexed is listed in the index step's `params.not_searched`), a
`grid_index` step (the index step's old function), the index group under
an earlier name (`unified_index`, `unified_document_index`), a lock
named `qmd_embed` (now `embedder`), and an id with capitals in it (every
id is lower case, since each names a folder, and the server moves the
folder too). `datalib-step help config-model` lists every rewrite.

## Adding your own step commands

A step's `command` may be any ordinary command line, for a step that is
not one of `datalib-step`'s functions. If the user's request needs a
new program (a custom fetcher, a converter, …), write it and install
it into the library's **`system/bin/`** — either the binary itself or a
symlink to wherever it lives:

```sh
mkdir -p <data_root>/system/bin
ln -sf /path/to/my-fetcher <data_root>/system/bin/my-fetcher
```

`<data_root>/system/bin` is on `PATH` whenever the pipeline runs, after
Datalib's own binaries, so a bare `command = "my-fetcher --out ."`
resolves. The same applies to an `[[applets]]` command, so one install
location covers both kinds of entry. Keep step commands non-interactive; they run headless
with their output captured into the run's log (the Manage tab shows it).

Steps run with the data root as their working directory and write the
one tree their id names, which is what downstream steps' `inputs` name
— a new source should ultimately keep its rendered documents in
`<group>/render_markdown`, which the index step's `*/render_markdown`
then reads with no edit.

## Checking your work

- On every successful PUT the user's config editor (the Manage tab)
  reloads automatically — there is nothing to refresh manually.
- `GET <origin>/api/dag` (with the `Authorization` header) returns the
  step DAG the saved config produces (`{ok, error, steps: [{id,
  command, inputs, outputs, deps, …}], run}`), in topological order —
  use it to confirm the wiring you intended.
- `GET <origin>/api/sync/sources` lists the source steps, as derived
  from the saved config.
- `POST <origin>/api/requests` with `{"roots": ["<step id>", …]}` syncs
  those steps and everything downstream of them (`{}` syncs every
  source). A source step always runs, any other only if it is out of
  date, and nothing upstream of a root runs.
