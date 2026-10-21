# Help an agent can set datalib up from

**Status: agreed 2026-10-21. §2.1 is #1275, §2.2 is #1278, §2.3 is
built on the branch after it except the two items marked open, and
§2.4 is deferred.**

## 1. The problem

A person sets datalib up through the app: the source wizard says which
fields matter, checks the connection, and writes a config that loads.
An agent has a release tarball and a shell. What it gets there today:

- `datalib-dag --help` is one hand-written usage string, read by a
  hand-rolled argument parser, so no test can say an example in it
  still parses.
- `datalib-step --help` is clap's default. The binary knows every
  source type and every function, and can explain none of them.
- The tarball holds binaries and license notices and no docs.
  [`agent_user.md`](../../agent_user.md) reaches an agent only if the
  agent finds GitHub.
- What a source's params mean is written down twice already: the
  `///` comments on each `<p>_config` crate's serde structs, and the
  wizard's `datalib/ui/src/config/catalog.ts`. Neither reaches a shell.
- `datalib-dag --check` checks a config's shape, not its params: the
  runner does not link the providers. A misspelled Slack param is
  found when the step runs.

## 2. What we build

The binaries document themselves, every example they show is checked
by a test, and `datalib-step` — which knows the sources — is where an
agent goes to learn about one. `datalib-dag` stays free of provider
knowledge: it points at `datalib-step`, and calls it, as it already
calls it for every step with no `command`. The step protocol
([`step_protocol.md`](../step_protocol.md)) does not change; a custom
step documents itself however its author likes.

### 2.1 Every `--help` is a door (PR 1)

- `datalib-dag` and `datalib-migrate-config` move onto clap, so each
  of `datalib-dag`'s verbs (`status`, `stop`, `turn-off`, `turn-on`)
  has its own `--help`, and every binary's parse is one a test can call.
- Each binary's help ends with examples, kept as data
  (`datalib/backend/cli_help`). A test runs every one through that
  binary's own parser.

### 2.2 `datalib-step help` (PR 2)

Each of our binaries' help says where to start: `datalib-step help`.

```
datalib-step help                  the setup journey, each step's command; the sources and functions
datalib-step help agent            agent_user.md, embedded
datalib-step help <type>           what it mirrors, how it signs in, its ingest methods, what to run next
datalib-step help <type> <method>  every field of that method: its doc, type and default
datalib-step help <type> --example minimal|complete [--id <id>]
                                   bare TOML on stdout, ready to append to a config
datalib-step help <function>       what it does, where it goes, and how to run one
datalib-step help <command>        one of datalib-step's commands (probe, …)
```

The journey is config → credentials → `--check` → `probe` → sync →
query. An example can only prove a stanza's shape; `datalib-step
probe` is what says the credentials reach anything, so each source's
page names it as the next step.

A source's page is its sections of `docs/user/getting_your_data.md`,
compiled in; `//:lint_repo` already holds that doc to a section per
type. A function's page says how to run one step: through
`datalib-dag --sync`, never `datalib-step` by hand, since only the
runner records what a step published; and, for the steps that can be
rebuilt, `--reset` and `--sync` together. The `datalib-dag` commands
these pages quote are tested against its parser, which lives in
`datalib_dag::cli` for that reason.

The tarball (and so the Docker image) carries the agent guide as
`docs/agent_user.md`, written at staging by the staged `datalib-step
help agent`. Every relative link in what `help` prints points at the
build's commit on GitHub, so nothing a doc links to has to ship.

### 2.3 Examples and fields from the config crates (PR 3)

- Each `<p>_config` crate keeps `examples/minimal_<method>.toml` and
  `examples/complete_<method>.toml` and exports them as `EXAMPLES`, a
  `const` rather than a trait: `claude_code` and `codex` share one
  config struct and still need examples of their own.
  `datalib_step/src/source_docs.rs` maps every `SourceType` to its
  crate's examples and schema with an exhaustive `match`. Its test
  loads and plans every stanza, checks each holds the method it is
  named for and still loads under another group id, and fails for a
  source with no minimal or no complete example per method.
- The config structs derive `schemars::JsonSchema`, so the field
  reference in `help <type> <method>` comes from their doc comments,
  reworded for a person writing a config. A method's complete stanza
  must write every option of its own table. The shared `common` and
  `latchkey_settings` options are set only where the source reads
  them, since a stanza teaching a sign-in to a source that reads local
  files would mislead. A retired field a planner refuses leaves the
  schema with `#[schemars(skip)]`. A time-series source's stanzas
  declare `render_time_series`.
- **Open:** `docs/user/config_examples/all_sources.toml` is not yet
  checked against the examples, and the stanzas found it stale: no
  `render_time_series` for garmin, yolink or airvisual; an `embed` on
  airvisual, which the config upgrade removes; no render step for
  apple_photos, lightroom or google_drive, whose storage report puts
  them in the grid; a commented `databases = true` notion does not
  accept; a GitHub ref form (`owner/repo/pull/NUM`) the parser refuses.
  Generate it from the complete stanzas, or check it against them.
- `datalib-step check-params <config>` plans every built-in step's
  params and prints one JSON line per step a sync would refuse.
  `datalib-dag --check` runs it (never a custom command) and reports
  each at the misspelled key's line, or the step's, and exits 2.
  `PUT /api/config` and `POST /api/config/check` ask it too, and refuse
  to write such a config; the Manage editor shows the refusal with its
  line.

### 2.4 Later: adding a source from the shell (deferred)

An `add-source` command does the wizard's job from the CLI, and the
wizard reads the same schema and examples instead of keeping its own
copy in `catalog.ts`.

## 3. How we will know

Give a fresh agent a release tarball and one task ("mirror my Slack
and Gmail"), once before PR 2 and once after, and note where each run
gets stuck.
