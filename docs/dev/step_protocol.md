# Writing a step command

The sync engine (`datalib-pipeline`) runs a DAG of arbitrary commands. Any
executable can be a step: the runner spawns it, hands it what its config
entry declares, and watches its stdout and stderr. A plain shell script
that exits 0 or non-0 is a valid step. Everything beyond that is an
optional protocol layer, and each layer you adopt buys better
incrementality, progress reporting or failure handling.

This doc is the contract from the command's side. The runner's side
(when a step is stale, retries, what a dropped config entry costs) is
[the runner's README](../../datalib/backend/dag/README.md).

## The config entry

The config is TOML. A step is one `[[steps]]` table: its `id` is the one
tree it writes, and `command` is the program it runs.

```toml
[[groups]]
id = "weather"
name = "Weather at SFO"

[[steps]]
id = "weather/ingest"
command = "fetch-weather --station KSFO"   # split shell-style
env = { WEATHER_DEBUG = "1" }              # extra child environment
[steps.params]                             # arbitrary TOML, yours
units = "metric"
```

That step writes `<data_root>/weather/ingest/`, and another step reads
it with `inputs = ["weather/ingest"]`. The id's first part is the
step's group: a `[[groups]]` entry with that id is what the Manage
screen shows it under, and the runner uses it for nothing but labels. A
step whose first part names no group is a step outside any group. A
group's `type` is for whoever shows the pipeline; the runner never
reads it.

| key | what the runner does with it |
| --- | --- |
| `id` | the directory `<data_root>/<id>/`; only this step writes there, and no two ids are equal or nested |
| `command` | required: the argv, below |
| `inputs` | the ids of the steps it reads: it waits for each one's first output and runs again when one's version moves |
| `params` | any TOML, handed to the step as JSON in `DATALIB_DAG_PARAMS`; at most 64 KiB |
| `env` | variables added to the step's environment |
| `reads` | `"files"` (the default) or `"pinned"`: § "When you are started" |
| `start` | `"on_each_seal"` (the default) or `"after_inputs_settle"`: § "When you are started" |
| `locks` | named locks it holds while it runs ([the runner's README](../../datalib/backend/dag/README.md) § "What keeps steps apart") |
| `code_version` | any string; changing it runs the step again |

A step's fingerprint, which decides whether its definition changed
since its last success, covers `id`, `command`, `inputs`, `params`,
`env` and `code_version`, and not `reads`, `start` or `locks`.

Two top-level tables concern every step:

- `[env]`: variables handed to every step, below each step's own `env`.
- `[[programs]]`: the verbs a program takes (§ "Verbs"). A step's
  program is the first word of its `command`, compared as written.

```toml
[[programs]]
command = "fetch-weather"
verbs = ["reset"]
```

`datalib-step`, the step program Datalib ships, is one program like
any other: its steps write `command = "datalib-step run <function>
<type>"` (§ "How `datalib-step` fits").

Sub-tables like `[steps.params]` must come after the step's plain keys:
in TOML a table header ends the table it appears in, so everything below
it belongs to `params` until the next header.

`command` is one string, split into an argv shell-style (quotes and
backslash escapes work; there is **no** variable expansion, globbing,
or piping — wrap in `sh -c '…'` if you need real shell). The first
word resolves via `PATH`, with the runner's `--binary-dir` (default:
the directory `datalib-pipeline` itself lives in; also settable as
`binary_dir` in the config, above the first `[[steps]]`) first and the
library's own `<data_root>/system/bin/` second, both ahead of the
inherited `PATH`. The first is how a program shipped beside the runner
is found without an absolute path; the second is where a library's own programs go, so
they travel with it. (They used to go in `~/.datalib/bin`, shared by
every library; a launch copies any program the config still runs from
there into `system/bin/`, `http/src/library_bin.rs`.) A first word
that is a relative path with a `/` in it is relative to the data root,
the step's working directory; that is how a step runs a package's
program, `system/packages/<pin>/bin/…` (`packages.md`).

## What your command receives

**Working directory**: the data root. All artifact paths are relative
to it.

**The command line** — your `command`, and nothing else on an ordinary
run:

```
cd <data_root> && <command…> [--<verb>]
```

A verb is appended only when the invocation is one, and only one your
program declares (§ "Verbs"). It rides on the command line rather than
in the environment so that nothing the step starts inherits it.

**Params** — the entry's `params` subtree, converted TOML → JSON (TOML
dates/times arrive as their string form), in `DATALIB_DAG_PARAMS`.
Unset when the entry has none. The environment, unlike the command
line, is readable only by the same user and root; still, put a secret's
*name* in params (`key_env_var = "WHATSAPP_KEY"`) rather than its value,
since everything a step starts inherits its environment. A command that
takes nothing still works: declare no params and it sees nothing extra.
There is no `--outputs`: the one tree a step writes is its id, which
arrives in the environment.

**Environment**: who you are and what you read.

| variable | meaning |
| --- | --- |
| `DATALIB_DAG_STEP` | this step's id, and the one tree it writes (`weather/ingest`) |
| `DATALIB_DAG_RUN_ID` | the run this invocation belongs to — a UUID the loop mints for each busy period (the stretch from idle to busy and back, serving every request that arrives meanwhile), or whatever the caller passed to `datalib-pipeline` as `--run-id`. Every row in `system/runs/runs.sqlite` carries it; stamp it into anything you write that should be joinable back to the run. The built-in steps end every doltlite commit message with ` run=<id>` (`doltlite_raw::stamp_run`), which is how the Manage screen's commit history gets from a commit to its log |
| `DATALIB_DAG_ATTEMPT` | which attempt this is, starting at `1`; a retry counts up |
| `DATALIB_DAG_DATA_ROOT` | absolute path of the data root (== cwd) |
| `DATALIB_DAG_PARAMS` | the entry's `params`, as JSON; unset when it has none |
| `DATALIB_DAG_INPUTS` | resolved input artifacts, `\n`-separated, relative to the data root |
| `DATALIB_DAG_CHANGED_INPUTS` | the subset of the above whose version moved since this step's last success, `\n`-separated; every input when there is no last success to compare against (a first run, or the step's own config changed). Empty only when nothing moved |
| `DATALIB_DAG_FAILED_INPUTS` | the subset of `DATALIB_DAG_INPUTS` whose producer's most recent run did not succeed — it failed (`incomplete` included), was stopped, or was blocked — `\n`-separated; empty when every one succeeded or is running now. You are started anyway, on what each committed before it ended; whether to go on is yours to decide |
| `DATALIB_READS` | a JSON object, input path → the version the runner started this invocation against, as `<fingerprint>:<version>` (§ "When you are started"); an input with no version yet is absent. A version for what you *read*, where the output's own would say less (an `embed` step reports a hash of this as its version) |
| `DATALIB_DAG_NOW` | the run's pinned timestamp (RFC 3339). Stamp times with this instead of sampling your own clock, so one run's outputs agree |
| `RUST_LOG` | the run's log filter, in `tracing-subscriber`'s grammar, built from the config's `log_level` ([`logging.md`](logging.md) § "Where a line comes from"). A `RUST_LOG` already set where the runner was started is passed through instead. A step in another language may honor it or ignore it; what it prints is kept regardless |

plus `PATH` (with the binary dir first), `PYTHONUNBUFFERED=1`, the
config's `[env]`, and the entry's own `env` (which wins over everything
else on collision).

**Put a temp file where `TMPDIR` says.** In a protected library it names
a folder inside the library, and your temp files hold what the library
holds. Rust's and Node's temp-file calls follow it; macOS's `mktemp`
does not, so in a shell script write `mktemp "$TMPDIR/x.XXXXXX"`.

**Nothing else of the runner's own environment reaches a step but a
short list** (`datalib_env::inherit_only`): what any program needs to
run (`HOME`, `TMPDIR`, the locale and time zone, proxy and certificate
settings), latchkey's `LATCHKEY_*` variables and what latchkey reads,
and how this build logs and finds its runtime. A variable the step's
`params` name under a key ending in `_env_var`, at any depth
(`key_env_var = "WHATSAPP_KEY"`), is handed too, and so is every name
in `DATALIB_CHILD_ENV` (comma-separated) where the runner or the server
was started. Anything else a person exported,
another tool's token or the server's own API token, never reaches a
step: a value a step needs goes in its `env`, or its name in
`DATALIB_CHILD_ENV`.

## The rules you must follow

The scheduler's correctness rests on these:

* **Write only under your own tree**, the one `DATALIB_DAG_STEP` names.
  No two steps write one tree; the loader refuses a config where two ids
  coincide or nest.
* **Be idempotent.** Retries and re-runs simply invoke you again; a
  re-run over unchanged inputs must be safe, and ideally cheap.
* **A commit is a correct state: never leave a torn tree on *any*
  path** (success, failure, interrupt or crash). Readers pin what you
  committed and read it at once, so commit only at a boundary you chose.
  Whatever you wrote after your last commit is discarded by the next
  writer's `open`, never adopted; the next run fetches it again.

Everything else (resume cursors, dedup indexes, bookkeeping) is private
to you. Keep it under your own tree.

## When you are started

The loop decides when your step runs ([the runner's
README](../../datalib/backend/dag/README.md#what-the-loop-runs-and-what-makes-a-step-stale)).
What that means from your side:

* **By default you never run beside a writer of what you read.** With
  `reads = "files"`, the default, no producer of your inputs runs while
  you do, in either order: read the input directories as they are on
  disk. That is the safe choice for a program that does not know about
  versions.
* **With `reads = "pinned"` you may be started while an input is still
  being written, and run again.** You then promise to read only the
  version `DATALIB_READS` names (below), and writers carry on beside
  you. A producer that declares `streams_output` (below, under
  "checkpoint") lets the steps that read it this way start on each of
  its seals while it carries on. The downloads Datalib ships for Slack, mail,
  Claude, ChatGPT, Notion, Google Drive, Linear, Todoist, Garmin, YoLink,
  GitHub, GitLab and Baby Connect all declare it, and so does every
  `render_markdown`. You are run again after each later seal, and
  once more when the producer ends if it moved since your last run;
  never twice at once.
* **Or ask to start once it has all settled.** A step whose config entry
  says `start = "after_inputs_settle"` is not started on seals: it
  waits until nothing upstream of it is running or about to run, then
  runs once, on what its producers ended with. Unlike `reads = "files"`
  it holds no writer back while it runs; if a sync that starts
  meanwhile moves what it read, it runs again once that sync has
  settled. The default,
  `start = "on_each_seal"`, is the behaviour above.
* **Reading pinned, read the commit you were started against, never
  `main`.**
  `DATALIB_READS` holds, for each input, the version you were started
  against, as `<fingerprint>:<version>`: a hash of the producing step's
  definition, a colon, and the version that step reported. For a raw
  store that version is a commit, so the commit is everything after
  the first `:` (`${v#*:}` in a shell): the last one at which the
  store's content moved. A sync that found nothing new still commits
  its own record (`sync_runs`, its stamps), which moves `main` but not
  the version, so you are not run for it. `main` may have moved on
  since; when its content moved, the loop runs you again for it.
* **`DATALIB_DAG_CHANGED_INPUTS` names what moved.** On a first run,
  or when your definition changed since your last success, there is
  nothing to compare with, and it names every input: do all your work.
  So does a reset of your step. Empty only ever means nothing you read
  moved.
* **A failed input is read at its last commit, and named.** What a
  producer committed before it failed is a version like any other;
  `DATALIB_DAG_FAILED_INPUTS` lets you tell a partial input from a whole
  one. Only a step none of whose inputs has ever published anything is
  held back (`blocked`).

## stdout: the event protocol (optional)

stdout is parsed line by line as NDJSON. A line that does not parse is
kept as a plain `info` log, so `echo` output is captured, not lost.
Parseable lines drive live progress in the runner and the Manage screen:

```json
{"event":"metric","step":"me","name":"rows_upserted_total","labels":{"table":"messages"},"value":1234}
{"event":"metric","step":"me","name":"queued","value":17}
{"event":"progress_message","step":"me","msg":"fetching page 3"}
{"event":"log","step":"me","level":"info","msg":"hello","target":"me::fetch","fields":{"page":3}}
```

The `step` field is required but its value does not matter: the runner
re-tags every event with the real step id.

**`metric` is a current value, never a delta.** Name the thing counted
and send the total so far each time it moves; `labels` is optional and
splits one name into series (`table=messages`). Absolute values make the
runner's coalescing lossless: it keeps the newest value per series, and
a dropped position costs nothing where a dropped increment would be lost
work.

**Name a series the way Prometheus would**, because `GET /metrics`
serves it under that name and reads its type off it
([`logging.md`](logging.md#from-outside-the-app)):

- A running total that only grows ends in **`_total`** and is served as
  a counter (`rows_upserted_total`). It may start again from zero next
  run. One that goes down within a run gets a warning in your log.
- A count of what the last pass did, which the next pass replaces, is a
  gauge named for it: `last_pass_rows_inserted`.
- Anything that can go down is a gauge: `queued`, `items`.
- A unit goes before the suffix, in base units: `fetched_bytes_total`,
  `wait_seconds_total`.
- Lower case, `_` between words, and a label for what varies
  (`{table=…}`) rather than a name per value.

**`queued`** is the gauge the UI reads for progress: how much work is
ahead of you now, which you usually know even when you cannot know the
total. The Manage screen's Status column shows it while you run, with an
ETA from the pace work comes off it (from `done_total` if you use the
`progress_*` form below, otherwise from the falls in `queued`). You need
not send a last `queued` of zero: when you end, however you end, the
runner sets it to zero. When you end short of done (stopped, or failed,
`incomplete` included) it keeps what `queued` held as your **`owed`**,
which the Manage row and `datalib-pipeline status` show; when you succeed you
owe nothing. So a download that gives up with work left must end
`incomplete`, not succeed. Every series you report is charted on the
group's sync dashboard.

Three more are read by name. **`items`** is how many things your output
store holds (messages, readings, events; the whole store, not this run),
and fills the Manage screen's Items column and its sparkline;
**`documents`** is how many documents those items sit in, for that
cell's hover; **`problems`**, with a `severity=error` or
`severity=warning` label, is a grey count after a row's name. Count the
problems your step found, not ones it copied from an input, since the
group's count is the sum of its steps'. (The red and yellow counts are
problem rows: § "Problems: what you could not do".) Report each one
every run, zero included: the screen shows the newest value a step
reported, so a count left out keeps last run's. A step that reports
none of them draws a blank Items cell and no counts.

Two series you never send: the runner samples the resident memory of
your process and every process below it every two seconds, as
**`rss_bytes`** (zero once you exit) and **`rss_peak_bytes`**, the
highest this attempt; the peak also goes on your attempt's row in the
run store. A child whose parent exited is no longer below you and drops
out, so the sum is a floor. Nothing caps it. On macOS the runner also
raises the peak to your process's own lifetime peak footprint once you
exit, which catches a step shorter than one sample. On macOS the runner
starts you with `MallocLargeCache=0`, so freed large blocks are not
counted as yours (a Claude Code first sync peaked at 2.9 GB with it on
and under 0.4 GB with it off); a step's `env` can set it back to `1`.

A step that counts one thing and knows its total may use the shorter
form, which the runner turns into the `done_total` and `queued` metrics:

```json
{"event":"progress_length","step":"me","total":42}
{"event":"progress_inc","step":"me","delta":1}
```

`progress_message` is the step's own words: a phase, not a number.
`log`'s `target` and `fields` are optional; a plain `msg` is fine.

A step that seals part of its output while still running says so with a
`checkpoint`, and should say how many rows that seal added:

```json
{"event":"checkpoint","step":"me","version":"a1b2c3","rows":340}
```

A checkpoint records your version, so a kill keeps what you sealed. It
does not by itself let a consumer start early. A consumer may read your
output while you are still writing only if you say it can be read that
way: once, before your first checkpoint, print

```json
{"event":"capabilities","step":"me","streams_output":true}
```

Say it only if a reader of your output sees each seal whole and never
half a write, as a reader pinned to a doltlite commit does ([the
runner's README](../../datalib/backend/dag/README.md#what-a-sink-owes-its-consumers)).
Without it every consumer waits for you to finish, and a consumer that
reads your files off disk rather than at a pinned commit waits either
way.

The same event may carry `version`, the commit your program was built
from, which the run store records for this attempt's process (§ "Your
attempt is a process" below). It may come on its own, at any point:

```json
{"event":"capabilities","step":"me","version":"ae2d52f0"}
```

`rows` is what the runner keeps each **consumer's** queue depth from —
the `queued{from=<you>}` metric on every step that reads your output,
counting up as you seal and down as they read — with no store opened
to measure it. A step that cannot count leaves `rows` out; its
consumers' queue then reads as unknown rather than wrong. A
checkpoint you re-announce (which you should, until a consumer has
run — one arriving while the consumer is mid-pass is dropped) counts
once, and one that arrives after your outcome is ignored.

Everything above lands in `<data_root>/system/runs/runs.sqlite`, plain
SQLite: one row per log line, the newest value per metric, every step's
state, for this run and the ones before it. That is what the Manage
screen reads; how to read it yourself is [`logging.md`](logging.md).

### Problems: what you could not do

A problem is one thing your step could not fully do: a record it could
not fetch, a file it could not read. It is a row of the `problems`
table, the same table in every store. A reader finds a step's problems
in the stores directly in its folder (`<data root>/<group>/<function>/`):
each `.doltlite_db` as its `main` commit has it, and each plain
`.sqlite` file. A step with no store of its own keeps them in
`problems.sqlite` there, and either writes that file itself or sends two
kinds of line and lets the runner write it:

```json
{"event":"problem","stage":"fetch","scope_kind":"entity","scope_key":"messages:C01/1712345678.000100","outcome":"dropped","reason":"fetch_failed","severity":"warning","sample":"HTTP 404","upstream_entity_kind":"message","upstream_id":"C01/1712345678.000100"}
{"event":"covered","kind":"records","table":"messages","all_but":["C01/1712345999.000200"]}
```

- **A `problem` line** is a row without the columns the writer fills:
  its id, its source (the step's group) and its stamps. `stage`,
  `scope_kind` (`entity` or `markdown`), `outcome`, `reason` and
  `severity` are Datalib's own closed sets; `sample` says what happened;
  `severity`, `item_uuid`, `field`, `path` and `rule` are optional.
- **`scope_key`** is what the problem is about within your step:
  `<table>:<id>` for a record, `file:<path>` for a file, or
  `listing:<name>` or `phase:<name>` for a part of your run. It is what
  `covered` matches.
- **What it is about upstream** goes in `upstream_entity_kind`,
  `upstream_id` and `upstream_account`, the names a grid row carries;
  never an id some other step mints.
- **`covered`** says what this run checked again, so that a problem it
  covered and did not find again goes: `all`, `listings` (every
  `listing:` and `phase:` scope), `records` of one `table`, or `files`
  `under` a folder. A `records` line with `only` vouches for exactly
  those records; one with `all_but`, and a `files` line with it, for
  every one but those. Both count even when your run fails. Without
  either, and for `all` and `listings`, only a run that succeeds clears
  anything.

The runner writes problems at most a second after their line, so they
show while you run, and clears when you end. A step that sends no
`problem` or `covered` line has its file left alone. Run by hand
(§"Running `datalib-step` by hand"), these lines are printed and kept
nowhere.

### The outcome line

The last thing you may print is one `outcome` event: the content version
of your output.

```json
{"event":"outcome","outputs":[
  {"path":"weather/ingest","version":"2026-07-21T06:00Z-a1b2","rows":12}
]}
```

`rows` is optional and means what it means on a `checkpoint`: what the
output gained since your last seal, or in all if you never sealed.

There are two cases, and that is the whole protocol:

* **`version`**: a content version you vouch for, such as a doltlite
  commit hash, a row-set hash or a cursor hash. Trusted verbatim and
  compared only for equality; the runner never opens your output.
* **nothing** (leave the path or the whole line out): each success
  counts as new, so every step that reads your output runs again after
  it. Always safe, and wasteful when nothing changed; the runner logs an
  `info` line saying so.

**The version must be a function of the output's content.** Two runs
that leave the same data behind must report the same string, because
that string is the whole signal for "did this change?": a step that did
nothing this pass reports what it reported last time, and its consumers
skip. A timestamp, a run id or a counter is not a version: it moves
every run, which is what reporting nothing already gets you.

**Spell one version the same way everywhere.** A checkpoint's `version`
and the outcome's are compared as strings, so if you finish on the
commit you last sealed, report it exactly as the checkpoint did, and
your consumers do not run again for it. A doltlite store's head is a
good version: it moves only when a commit changed something
([`doltlite.md`](doltlite.md#diffs)). A built-in ingest reports a
better one, the last commit at which a table its readers read moved,
since its own bookkeeping commits on every run.

Reporting on any path but your own tree is a contract violation and
fails the step. Exit `0` means success; the outcome line only informs.

### Failure classification

On a non-zero exit, an outcome line can tell the scheduler what kind of
failure this is, which decides whether it retries:

```json
{"event":"outcome","failure":"rate_limited","outputs":[
  {"path":"weather/ingest","version":"2026-07-21T05:00Z-9f3c"}
]}
```

| `failure` | meaning | scheduler reaction |
| --- | --- | --- |
| `transient` | network blip, lock contention | retry, up to 3 attempts in all, 1s then 2s apart |
| `rate_limited` | HTTP 429 and friends | retry, as for `transient` |
| `auth` | credentials need a human | fail fast |
| `permission` | the OS would not let you read a path your settings name (macOS privacy) | fail fast; the app's Manage row offers "Choose folder again" |
| `data` | bad input; retrying won't help | fail fast (the default when absent) |
| `cancelled` | you were interrupted | fail fast; exit code 130 by convention |
| `incomplete` | you gave up with work still owed, or part of the work failed, after committing what you got | fail fast: your own retries are what ran out, and the next run resumes |
| `incompatible` | your answer to `--prepare`: what you wrote is in a shape you cannot use | the step is turned off until a later start's `--prepare` does not refuse (§ "Prepare") |

`outputs` on a failure reports what you *did* commit. The scheduler
records it, your dependents read it now, and the next run resumes from
it: a commit is a correct state. That includes `cancelled`: a step that
stops at a consistent point and commits should report where it got to.
A failure that reports nothing moves nothing, since the runner cannot
know your tree is whole.

### Rendering a source with no data

**An input that has never been written is not a failure.** If your
render step finds no raw store, emit nothing and exit 0: an empty output,
not `failure: data`. That is the normal state of every source in a
freshly scaffolded config, where a person adds ten sources and syncs
one; a `data` failure would turn the Manage row red for a source that
has simply not been synced. An empty render is safe for the index,
which removes only the documents a render store dropped.

Keep failing, loudly, when the data is *there but wrong*: a store that
exists and cannot be opened, a schema missing tables it should have, a
row that will not parse. The line is **absent vs. malformed**: return
empty on the "nothing exists" branch and let every error below it
propagate.

## stdin: nothing to read

**Your step is given `/dev/null` on stdin.** Read it and you get an
immediate end-of-file; there is no input channel.
Everything a step is told arrives as environment variables, its
command line, and its declared inputs.

## fd 3: the runner's pipe, if you want it

Every step is also handed a pipe on file descriptor 3, and may ignore
it. The runner holds the write end for exactly as long as it lives, so
**end-of-file on fd 3 means the runner is gone**, even when it was
SIGKILLed or taken by the OOM killer and could run no code to stop you.
A step that does not watch it carries on with its store open and nobody
recording how the run ended.

`DATALIB_PARENT_PIPE` holds the descriptor's number; read it rather
than hard-coding 3. In Rust it is one call at startup:

```rust
datalib_parent_watch::exit_with_parent(|| { /* stop; see below */ })?;
```

In any other language, watch that descriptor for end-of-file yourself.
It is not on stdin because a program that reads stdin expecting the
immediate end-of-file of `/dev/null` would block forever on a pipe
nobody writes to.

When it closes, take down what *you* spawned, not just yourself. The
runner starts each step as its own process-group leader, so
`kill(0, SIGINT)` is exactly "me and my children". That is what
`datalib-step` does, and its SIGINT handler then seals and exits.

## stderr: logging

stderr is yours for humans: every line is kept as an `info` log, and so
is every stdout line that is not an event. If you exit non-zero, your
last few stderr lines (plain lines, and the message of any structured
`warn` or `error` line) become the step's error message, which the
runner also writes as the step's last log line at `error` level; that
line is where the Manage row's double-click opens the log. A step that
exits after a cancel (`failure: cancelled`) is recorded as stopped, and
its line is a `warn`.

A step that reads a streaming producer runs once per seal, and a pass
that fails runs again on the producer's next seal. The run reports the
last pass, but a failed pass that was run again still shows: as an
`error` line of its own (`pass 1 failed: …`), as a warning on the Manage
row, and in `datalib-pipeline`'s report.

Each line records which pipe it came from and is timestamped as the
runner reads it; the two pipes are read concurrently, so the log is in
arrival order across both. **Flush per line**: a program that
block-buffers a piped stdout (C's stdio, Python's by default) hands the
runner its progress in 4 KB lumps, long after the stderr it belonged
beside. The runner sets `PYTHONUNBUFFERED=1`; Rust's stdout is
line-buffered and `sh` writes through.

A tracing-JSON line (with a `level` field, as `tracing-subscriber`'s
JSON format writes) is unwrapped: its `fields.message` becomes the
line's text, its `timestamp`, `level`, `target` and thread become
columns, and the rest rides along as `fields`. Its own timestamp wins
over the arrival time, so write it with the local offset
(`…T10:00:00+01:00`); a `Z` stamp is filed as `+00:00`. Every level is
kept, `debug` and `trace` included.

A structured tracing-JSON line (with a `level` field, as
tracing-subscriber's JSON format writes) is unwrapped rather than
quoted: its `fields.message` becomes the log's `msg`; its `timestamp`,
`level`, `target` and thread (`threadName`, else `threadId`) become
columns — the line's own timestamp wins over the runner's arrival time,
offset included, so write it in the local offset
(`…T10:00:00+01:00`): a `Z` stamp is filed as `+00:00` beside the
runner's lines in the real one — and everything else it carried
(`filename`, `line_number`, the remaining `fields`) rides along as
`fields`. The Manage screen shows
the sentence, not the envelope, and can still sort by thread.

Every level is kept, `debug` and `trace` included: the runner stores a
`DEBUG` envelope as a `debug` row rather than rounding it up to `info`.

**Your attempt is a process.** Every line names the process that
wrote it, and each attempt of a step is one: the runner records it at
spawn and again at `wait(2)` with the `exit_code` or `signal` that
ended it, and its commit — the `version` your `capabilities` event
reports, none if it reports none — which is what a structured line's
`filename` and `line_number` are relative to. What
the runner itself says *about* your step is the runner's line, with
your step as its subject. The model, the other kinds of process and
the log card that reads them: [`logging.md`](logging.md).

## Verbs

Besides an ordinary run, the runner may invoke a step with one verb
appended: `--reset`, `--prepare`, `--compact`, `--collect` or
`--check`. It sends
one only to a step whose program declares it in a `[[programs]]` entry,
so a program that ignores its arguments is never sent one:

```toml
[[programs]]
command = "datalib-step"
verbs = ["reset", "prepare", "compact", "collect", "check"]
```

An invocation with a verb gets the same environment and params as a
run, but no inputs are resolved, and it does the verb's work and
nothing else. Each verb is below; for a program that does not declare
it:

| verb | sent when | if the program does not declare it |
| --- | --- | --- |
| `--reset` | someone resets the step | the runner deletes `<data_root>/<id>/` itself |
| `--prepare` | each time the runner starts, before the step's first run | nothing |
| `--compact`, `--collect` | someone asks to free disk space | nothing |
| `--check` | someone saves a config, or runs `datalib-pipeline check` | the step's params are not checked before it runs |

## Reset

`datalib-pipeline reset <data_root> <step-id>…` (or the app's
Reset, `POST /api/reset`) empties what a step wrote so the next run
does its work from the start. The runner invokes a step whose program
takes the verb once with `--reset` appended, and deletes the tree of
any other; then it forgets the step ever succeeded and records the
version the reset reports, or a new one if it reports none, so
everything reading the tree runs again. Given the verb, empty your
tree, keep whatever history you keep, commit if you commit, exit 0,
and do nothing else; no sync follows.
`datalib-step` deletes every row of the tree's store in one
commit, keeping the tables, plus a render tree's documents; the run log
and the rest are still in the history. Keeping the tables is what lets
a reader take the emptiness in as ordinary deletions: the app's Reset
then syncs what reads the step, so its documents leave the grid. The app
does not wait for a sync to end: the loop stops the step if it runs and
resets it there and then ([the runner's
README](../../datalib/backend/dag/README.md#resets-and-purges)). A reset
also reports the metrics a run would (`problems`, and `documents` for a
render step), counted off the emptied store: the Manage row shows a
step's newest sample, and the reset step itself does not run again
until the next sync.

`datalib-step` resets `ingest`, `render_markdown` and `embedding_map`
and refuses the rest. For a download or a render it deletes every row of
the step's store in one commit and keeps the tables, so the history
still has the rows and a reader takes the emptiness
in as ordinary deletions: the app's Reset then syncs what reads the
step, and its documents leave the grid. A reset also reports the
`problems` (and for a render, `documents`) counts of the emptied store,
since the Manage row shows a step's newest sample. The app does not wait
for a sync to end: the loop stops the step if it runs and resets it
there and then ([the runner's
README](../../datalib/backend/dag/README.md#resets-and-purges)).

Nothing resets an ingest step's blob store, `blobs.sqlite`: a reset
keeps the bytes, and the refetch lands on them. To get the space back,
delete the file **and** reset the ingest step, together. Deleting the
file alone leaves rows naming bytes that are gone, and the download
does not fetch what it believes it already has.

## Prepare

Each time the runner starts, it invokes every step whose program takes
the verb once, producers before their readers, with `--prepare`
appended, before your step or anything that reads it runs ([the runner's README](../../datalib/backend/dag/README.md#upgrading-a-root)).
The runner does not know whether your program changed since the last
start, so you are asked every time: answer quickly when there is
nothing to do. The runner knows nothing of what a step wrote; the
step decides, and answers in its outcome line:

* **nothing to do** — what it wrote is in this program's shape, or it
  has written nothing yet: exit 0, report nothing.
* **brought up to date** — it brought what it wrote to this program's
  shape in place, fetching nothing: report the output's new version as
  a run would. The runner records it and keeps the step's last success,
  so what reads the output is stale.
* **needs a rerun** — what it wrote is in a shape it cannot reach in
  place: add `"needs_rerun": true` to the outcome. The runner holds the
  step due until it next succeeds, and opens one request for every such
  step once the launch's prepare pass is over.
* **refused** — what it wrote is in a shape this program cannot use,
  such as a newer build's: exit non-zero with `"failure":"incompatible"`
  and say why on stderr. The runner turns the step off, as turned off
  by `the runner, because the step refused --prepare`, and turns it
  back on at a later start whose `--prepare` does not refuse.

```json
{"event":"outcome","needs_rerun":true,"outputs":[]}
```

`datalib-step` answers for each store it keeps by comparing the store's
`_datalib_meta` with this build's, so a store already in shape costs one
read. An ingest whose raw store is older first says it is updating the
store and downloading nothing, as a line and as its status, because its
name reads as a download; it then opens its raw store with the
provider's migration ladder and closes it: every rung, the additive DDL
and an old blob store's conversion run, and each is sealed to `main`. A
render step and the grid index answer `needs_rerun` when their store's
tables or ladders differ from this build's (`_datalib_meta`); a derived
store is rebuilt, never migrated. When only its indexes differ
(`index_hash`), the step opens the store as its owner, which makes
them, and answers nothing. The grid index also answers `needs_rerun` when the search
index's keyword tables are in another shape or a source's documents
were drawn by another version, which its keyword pass reads again.
`embedding_map` deletes a map this build does not read and answers
`needs_rerun`, which lays it out afresh; `embed` has nothing to answer.
Any of them refuses a store a newer build wrote.

## Check

`datalib-pipeline check`, and the app each time a config is saved, ask
every step whose program takes the verb whether it would run its params
as written, with `--check` appended and nothing yet run. The step
reads its params as a run would, opens and writes nothing, and answers:

* **runs as written**: exit 0. A key that does nothing, or anything
  else that runs but is probably a mistake, goes in the outcome's
  `warnings`, one sentence each, which the app and the check show at
  the step's line and do not refuse.
* **would refuse**: exit non-zero and say why on stderr, as a run that
  fails does. Name the key serde's way (``unknown field `chanels` ``)
  and the diagnostic points at that key's line. The app does not write
  the config, and `datalib-pipeline check` exits 2.

```json
{"event":"outcome","warnings":["`period` no longer does anything; delete it"],"outputs":[]}
```

A step has thirty seconds to answer. Up to eight are asked at once.
`datalib-step` plans the params as a run would: its provider's config
schema, its method tables, and the keys that no longer do anything,
which it runs without and names as warnings.

## Compact and collect

`datalib-pipeline free-disk-space`, or the app's `POST /api/free_disk_space`
(the status bar's "Free up disk space…"), asks every step that takes the
verbs to give back the disk its store holds, once each: with
`--compact` appended when old versions of derived data are to be
dropped (`--drop-old-versions-on-derived-data`,
`"drop_old_versions_on_derived_data": true`), else `--collect`. Nothing
that reads a store may be reading it while it is rewritten: the loop
holds the steps it asks and every step that reads them until it is done (`dag/README.md`
§"Resets and purges"). The step decides what the verb means for what it
wrote, runs nothing else, and reports its version as a run would; the
runner records it, so a step that kept its content and reports a
content version moves nothing downstream. Report nothing when there is
nothing to give back.

A step that shrank a store also says how big it was, in bytes, before and
after, which is what the app's free-space pill adds up:

```json
{"event":"outcome","store_bytes":{"before":6600000000,"after":3100000000},"outputs":[...]}
```

`--collect` is lossless: it runs `dolt_gc` on the step's doltlite
store, deleting only chunks no commit, branch or tag reaches.
`--compact` is lossy for a render store only, whose history nothing
reads: it folds the history into one commit and collects. The store's
version is its tree, which the fold keeps, so nothing runs again. A raw
store's history is the record of what upstream once held, and a
time-series store's commits are what its index points at, so those are
only collected. The grid index is plain SQLite, so neither verb touches
it. Either verb refuses a store bigger than the disk's free space, since
gc writes the compacted store before it drops the old one.

## Signals: graceful cancellation (optional)

On cancellation (Ctrl-C, the UI's Stop, or the step turned off) the
runner sends your process group **SIGINT** and gives you fifteen
seconds, then SIGKILL. **Stop at your next consistent point, commit
there, and exit 130** with a `{"event":"outcome","failure":"cancelled"}`
line: what you committed stands, and the next run resumes. **Never
commit from the signal handler itself**: a commit made wherever the
signal landed publishes a half-written batch to every reader. If you do
nothing, you are killed at the grace and the next writer's `open`
discards what you wrote after your last commit. The kill reaches
anything still in your process group, even once you have exited, so
stop your children before you exit.

`datalib-step` does this on SIGINT or SIGTERM: its fetch loops stop
before the next unit of work (a channel, a conversation, a batch of
messages), it seals at once whatever the cadence says, it ends a
backoff sleep and sends no new request, and it reports `cancelled`. A
stopped run does not record its scope as satisfied, so a widened filter
interrupted part-way is backfilled by the next run.
`unified_grid_and_search_index` checks between documents: it rolls back
the source it is loading and keeps the ones already committed.

## Minimal examples

A shell step, no protocol at all (each success counts as new, so what
reads its tree runs after every sync):

```toml
[[steps]]
id = "notes/ingest"
command = "sh -c 'mkdir -p notes/ingest && cp -R \"$HOME/notes/.\" notes/ingest/'"
```

A Python step using inputs, progress and an outcome, which opens each
download that moved at the commit it was started against. It works under
any Python, uv's included. Install
[`imbue-datalib-reader`](doltlite.md#calling-the-library-directly) in
that Python, then point `command` at it: `/path/to/python3 /path/to/step.py`.

```python
#!/usr/bin/env python3
import hashlib, json, os, pathlib, sys

from datalib_reader import Store

def lines(name): return [p for p in os.environ.get(name, "").split("\n") if p]

inputs = lines("DATALIB_DAG_INPUTS")
changed = set(lines("DATALIB_DAG_CHANGED_INPUTS"))  # every input on a first run
failed = set(lines("DATALIB_DAG_FAILED_INPUTS"))    # read, but partial
reads = json.loads(os.environ.get("DATALIB_READS", "{}"))  # no entry: no version yet
root = pathlib.Path(os.environ["DATALIB_DAG_DATA_ROOT"])
out = os.environ["DATALIB_DAG_STEP"]   # the one tree this step writes
params = json.loads(os.environ.get("DATALIB_DAG_PARAMS", "{}"))

def emit(obj): print(json.dumps(obj), flush=True)

emit({"event": "progress_length", "step": "", "total": len(inputs)})
for src in inputs:
    if src in failed:
        emit({"event": "log", "step": "", "level": "warn",
              "msg": f"{src} did not finish its last run; reading what it committed"})
    if src in changed and src in reads:
        # A download's version is a commit of its raw store: <fingerprint>:<commit>.
        commit = reads[src].split(":", 1)[1]
        with Store(root / src / "entities.doltlite_db", commit) as store:
            # ... process only what moved; dolt_diff_<table> says what that is ...
            [(message,)] = store.query(
                "SELECT message FROM dolt_log WHERE commit_hash = ?", commit)
        emit({"event": "log", "step": "", "level": "info", "msg": f"{src} moved: {message}"})
    emit({"event": "progress_inc", "step": "", "delta": 1})

# A content version: same output bytes → same string, every run. A
# store with its own commit hash should report that instead; this
# digest is the generic fallback for a plain file tree.
h = hashlib.blake2b(digest_size=16)
for f in sorted((root / out).rglob("*")):
    if f.is_file():
        h.update(str(f.relative_to(root / out)).encode())
        h.update(f.read_bytes())

emit({"event": "outcome",
      "outputs": [{"path": out, "version": h.hexdigest()}]})
```

## How `datalib-step` fits

The built-in step types are one program implementing this protocol, and
the runner treats it as it treats any other. A step names what it does
on the command line:

```text
datalib-step run <function> [<type>] [--source-type <type>]
```

`<function>` is `ingest`, `render_markdown`, `render_time_series`,
`render_occurrences`, `embed`, `unified_grid_and_search_index`,
`embedding_map` or `time_series_index`; anything else is refused with
the list. `<type>` is the source type, which `ingest`
and the renders require; under a diff group it is `diff`, and
`--source-type` names the type of the source it compares. Its step's id
must be `<group>/<function>`, and it writes that tree; a render reads
its raw store from the first entry of `DATALIB_DAG_INPUTS`. It reads
`DATALIB_DAG_PARAMS` as the provider's **function-specific** config —
the ingest step carries the provider's download config (`common`
envelope, the method table block, …), the render step only the render
knobs (nothing for most providers; perseus `alignment_pairs`, email
`outlink_format`/`only_render_labels`). It
honors `DATALIB_DAG_NOW` and the five verbs, which Datalib's configs
declare for it in `[[programs]]`; it stops at its next consistent point
on SIGINT and commits there, and emits versions where it has them (the
index reports its generation; the versions are in the [runner's
README](../../datalib/backend/dag/README.md#versions-reported-by-the-step)). It reads three knobs a
config sets in `[env]`:

| variable | meaning |
| --- | --- |
| `DATALIB_STEP_CHECKPOINT_CADENCE` | how many seconds to let pass before a run's first checkpoint; it doubles the wait after each one (the next comes once the run is twice as old as at the last), since every commit keeps the pages it touched; `0` means every consistent point and never grows |
| `DATALIB_STEP_RENDER_PAGES` | `<render_units>,<blob_mb>`, either side empty for its default: how many render units a render reads together and how many MB of their blobs it loads at once |
| `DATALIB_STEP_MODAL_MAX_SANDBOXES` | the most Modal Sandboxes an `embed` step that sends its chunks to Modal runs at once (default 100) |

Use it as the reference implementation.

### Running `datalib-step` by hand

An app that embeds one download can run `datalib-step` itself, with no
config and no loop, into a folder of its own. Set the working directory
to that folder, and in the environment:

- `DATALIB_DAG_DATA_ROOT`, the same folder, absolute;
- `DATALIB_DAG_STEP`, `<group>/<function>`, such as `slack/ingest`;
- `DATALIB_DAG_INPUTS`, for a step that reads another (a render reads
  `<group>/ingest`);
- `DATALIB_DAG_PARAMS`, the params as JSON, shaped as the step's
  `params` table would be.

It runs as `datalib-step run <function> <type>`, such as
`datalib-step run ingest slack`. The step checkpoints as it goes and
writes the event protocol on stdout, as under the runner. What the
runner adds is lost: nothing records its version, so no step that reads
it knows to run. A run killed part way leaves its `sync_runs` row
`running` until the next run of that step marks it `interrupted`.

Four things a hand run has to get right that the runner does for it:

- **Stop it with SIGINT or SIGTERM, and wait for it.** Either one makes
  it stop at its next consistent point and seal what it has; one still
  running ten seconds later exits without that last seal. Give it more
  than those ten seconds before anything harsher: supervisord's
  `stopwaitsecs` and Docker's stop timeout both default to exactly ten,
  so raise them to fifteen. SIGKILL ends it at once: what it fetched
  since its last seal is discarded, and the next run fetches it again.
- **Leave `DATALIB_PARENT_PIPE` unset**, unless you hand it a pipe on
  that descriptor and keep the other end open. A process the applet
  gateway started has it set to `0`, and a step that inherits that
  watches its own stdin: `/dev/null` there makes it refuse to start, and
  a pipe that gets closed makes it exit without sealing.
- **Don't run a step the app's loop also runs on that data root.** The
  store's lock refuses a second writer at open, naming the first
  (`already has a writer: datalib-step (pid N)`), so whichever comes
  second fails. When that is the loop's run, it is a `data` failure, not
  retried, and its request ends failed. Use a folder no loop serves, or
  turn the step off in the loop (`datalib-pipeline turn-off`,
  `POST /api/steps/<id>/turn_off`) while you run it.
- **Give an absolute `DATALIB_DAG_DATA_ROOT`.** The step runs with the
  data root as its working directory, so a relative one is resolved a
  second time, beneath itself.
