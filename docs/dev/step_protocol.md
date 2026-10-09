# Writing a step command

The sync engine (`datalib-dag`) runs a DAG of arbitrary commands. Any
executable can be a step: the runner spawns it, feeds it what it
declared in the config, and watches its stdout/stderr. Everything
beyond "run a program and exit 0/non-0" is an *optional* protocol layer — a plain shell script is a valid step, and
each layer you adopt buys better incrementality, progress reporting,
or failure handling.

This doc is the contract from the command's point of view. The
runner/scheduler side (staleness, retry, what a dropped entry costs)
is in [`datalib/backend/dag/README.md`](../../datalib/backend/dag/README.md).

## The config entry

The config is TOML. A step is one `[[steps]]` table, filed under a
`[[groups]]` entry: the step names its group and the function it
performs there, and its id — the one tree it writes — is composed as
`<group>/<function>` rather than written.

```toml
[[groups]]
id = "weather"
name = "Weather at SFO"

[[steps]]
group = "weather"
function = "ingest"
command = "fetch-weather --station KSFO"   # split shell-style
env = { WEATHER_DEBUG = "1" }              # extra child environment
[steps.params]                             # arbitrary TOML, yours
units = "metric"
```

That step's id is `weather/ingest`, so it writes `<data_root>/weather/ingest/`
and another step reads it with `inputs = ["weather/ingest"]`. A step
outside any group is also legal — `id = "weather/ingest"` written
verbatim, no `group` or `function` — and is the shape for a one-off
executable that belongs to no source.

A step with no `command` at all is a built-in one: it runs
`datalib-step`, which takes the function and the group's `type` from
the environment below. That is the shape every source's own steps
have, and it is only legal under a group.

Sub-tables like `[steps.params]` must come after the step's plain keys:
in TOML a table header ends the table it appears in, so everything
below it belongs to `params` until the next header.

`command` is one string, split into an argv shell-style (quotes and
backslash escapes work; there is **no** variable expansion, globbing,
or piping — wrap in `sh -c '…'` if you need real shell). The first
word resolves via `PATH`, with the runner's `--binary-dir` (default:
the directory `datalib-dag` itself lives in; also settable as
`binary_dir` in the config, above the first `[[steps]]`) prepended —
which is how `datalib-step` is found without an absolute path.

## What your command receives

**Working directory** — the data root. All artifact paths are relative
to it.

**Appended flags** — the runner mechanically appends the entry's
declared fields to your argv, each only when present/non-empty:

| flag | value |
| --- | --- |
| `--params-file <path>` | a JSON file holding the entry's `params` subtree, converted TOML → JSON (TOML dates/times arrive as their string form) |
| `--inputs <json>` | the entry's `inputs`, as a JSON string array |
| `--reset <part>` | only on a reset, and then this invocation is a reset, not a run: see § Reset |
| `--migrate` | only on the first launch of a build, to a step that takes it, and then this invocation fetches nothing: see § Migrate |

A verb (`--reset`, `--migrate`) rides on the command line rather than in
the environment so that a command which has never heard of it refuses it
instead of running a sync, and so that nothing the step starts inherits
it.

So the entry above runs
`fetch-weather --station KSFO --params-file <root>/system/params/weather_ingest.XXXX.json`,
and that file holds `{"units":"metric"}`. The params travel in a file
and not on the command line because they are where tokens and device
ids live, and every user on the machine can read every process's
command line with `ps`. The runner creates the file readable by its
owner only and deletes it when the step exits, so read it early and
don't keep the path. A command that takes no flags at all still works —
declare no params and no inputs and it sees nothing extra (a `sh -c
'script'` step receives whatever is appended as `$0`/positional args and
can drop them). There is no `--outputs`: the one tree a step writes is
its id, which arrives in the environment.

**Environment** — the identity/context channel:

| variable | meaning |
| --- | --- |
| `DATALIB_DAG_STEP` | this step's id, and the one tree it writes (`weather/ingest`) |
| `DATALIB_DAG_RUN_ID` | the run this invocation belongs to — a UUID the loop mints for each busy period (the stretch from idle to busy and back, serving every request that arrives meanwhile), or whatever the caller passed to `datalib-dag` as `--run-id`. Every row in `system/runs/runs.sqlite` carries it; stamp it into anything you write that should be joinable back to the run. The built-in steps end every doltlite commit message with ` run=<id>` (`doltlite_raw::stamp_run`), which is how the Manage screen's commit history gets from a commit to its log |
| `DATALIB_DAG_ATTEMPT` | which attempt this is, starting at `1`; a retry counts up |
| `DATALIB_DAG_GROUP` | the group it is filed under (`weather`); unset for a step outside any group |
| `DATALIB_DAG_GROUP_TYPE` | the group's `type`, when it declares one |
| `DATALIB_DAG_FUNCTION` | what this step does within its group (`ingest`); unset for a step outside any group |
| `DATALIB_DAG_DATA_ROOT` | absolute path of the data root (== cwd) |
| `DATALIB_DAG_INPUTS` | resolved input artifacts, `\n`-separated, relative to the data root |
| `DATALIB_DAG_CHANGED_INPUTS` | the subset of the above whose version moved since this step's last success; empty when there is no last success to compare against (never completed, or the step's own config changed) — do all your work |
| `DATALIB_READS` | a JSON object, input path → the version the runner started this invocation against; an input with no version yet is absent. A version for what you *read*, where the output's own would say less (the qmd index reports a hash of this) |
| `DATALIB_DAG_NOW` | the run's pinned timestamp (RFC 3339). Stamp times with this instead of sampling your own clock, so one run's outputs agree |
| `DATALIB_DAG_CHECKPOINT_CADENCE` | set when the config has `checkpoint_cadence`: the most seconds you should let pass between checkpoints |
| `RUST_LOG` | the run's log filter, in `tracing-subscriber`'s grammar, built from the config's `log_level` ([`logging.md`](logging.md) § "Where a line comes from"). A `RUST_LOG` already set where the runner was started is passed through instead. A step in another language may honor it or ignore it; what it prints is kept regardless |

plus `PATH` (with the binary dir first), `PYTHONUNBUFFERED=1`, and
anything in the entry's `env` table (which wins over the run-wide values
on collision).

## The rules you must follow

These are what the scheduler's correctness rests on:

* **Write only under your own tree** — the one `DATALIB_DAG_STEP`
  names. No two steps write one tree; the loader refuses a config where
  two ids coincide or nest.
* **Be idempotent.** Retries and re-runs simply invoke you again; a
  re-run over unchanged inputs must be safe (and ideally cheap).
* **A commit is a correct state — never leave a torn tree on *any*
  path**: success, failure, interrupt, or crash. Readers pin what you
  committed and read it at once, so commit only at a boundary you
  chose. Whatever you wrote after your last commit is discarded by the
  next writer's `open`, never adopted; the next run refetches it from
  your cursor, which is what idempotency is for.

Everything else — resume cursors, dedup indexes, bookkeeping — is
private to you. Keep it under your own output trees.

## stdout: the event protocol (optional)

stdout is parsed line by line as NDJSON. Lines that don't parse are
forwarded as plain `info` logs, so `echo` output is captured, not
lost. Parseable lines let you drive live progress in the runner and
the Manage screen:

```json
{"event":"metric","step":"me","name":"rows_upserted_total","labels":{"table":"messages"},"value":1234}
{"event":"metric","step":"me","name":"queued","value":17}
{"event":"progress_message","step":"me","msg":"fetching page 3"}
{"event":"log","step":"me","level":"info","msg":"hello","target":"me::fetch","fields":{"page":3}}
```

**`metric` is a current value, never a delta.** Name the thing counted
and send the total so far each time it moves; `labels` is optional and
splits one name into series (`table=messages`).

**Name a series the way Prometheus would**, because `GET /metrics`
serves it under that name (`docs/dev/logging.md` §"From outside the
app") and reads its type off it:

- A running total that only grows ends in **`_total`** and is served as
  a counter: `rows_upserted_total`, `api_requests_total`. It may start
  again from zero next run; a reader of a counter expects that. One
  that goes down within a run is a gauge under a counter's name, and
  the runner says so with a warning in your log.
- A count of what the last pass did, which the next pass replaces, is
  a gauge named for it: `last_pass_rows_inserted`.
- Anything that can go down does not, and is served as a gauge:
  `queued`, `items`.
- A unit goes before that suffix, in base units: `fetched_bytes_total`,
  `wait_seconds_total`.
- Lower case, `_` between words, and a label for what varies
  (`{table=…}`) rather than a name per value.

The one gauge the UI looks for is **`queued`** — how much work is ahead
of you right now, which you usually know even when you cannot know the
total. The Manage screen's Status column shows it while the step has
work queued. Its ETA is the queue
over the pace work has come off it lately: read off `done_total` when you
use the `progress_*` form below, otherwise off the falls in your
`queued`. Every series you report is charted over the run on the
group's sync dashboard. You need not send a last `queued` of zero: when you end, however
you end, the runner sets it to zero for you. Absolute values are what
make the runner's coalescing lossless: it keeps the newest value per
series, and a dropped position costs nothing where a dropped increment
would be lost work.

Three names are read by name rather than just drawn. **`items`** is
how many things your output store holds — messages, readings, events;
whole store, not this run — and fills the Manage screen's Items column
and its sparkline; **`documents`** is how many documents those items
sit in, for that cell's hover; **`problems`**, with
a `severity=error` or `severity=warning` label, fills the red and
yellow counts after a row's name — count the problems your step
found, not ones it copied from an input, since the group's count is
the sum of its steps'. Report each one every run, zero
included: the screen shows the newest value a step reported, so a
count left out keeps last run's. A step that counts neither leaves no
series, and draws a blank Items cell and no counts. They are
`datalib_metrics::ITEMS`, `datalib_metrics::DOCUMENTS` and
`datalib_problems::METRIC` in the
tree; nothing else makes the reporter and the screen agree on the
spelling.

A step that counts one thing and knows its total may use the shorter
form instead, which the runner translates into the `done_total` and `queued`
metrics for it:

```json
{"event":"progress_length","step":"me","total":42}
{"event":"progress_inc","step":"me","delta":1}
```

`progress_message` is the step's own words — a phase, not a number.
`log`'s `target` and `fields` are optional; a plain `msg` is fine.

A step that seals part of its output while still running says so with
a `checkpoint` (P2 in `datalib/backend/dag/README.md`
§ "What a sink owes its consumers"), and should say how many rows
that seal added:

```json
{"event":"checkpoint","step":"me","version":"a1b2c3","rows":340}
```

A checkpoint alone does not let a consumer start early. A consumer may
read your output while you are still writing it only if you say your
output can be read that way: once, before your first checkpoint, print

```json
{"event":"capabilities","step":"me","streams_output":true}
```

Say it only if a reader of your output sees each seal whole and never
half a write, as a reader pinned to a doltlite commit does. Without it, each checkpoint still records
your version, so a kill keeps what you sealed, but every consumer waits
for you to finish. A consumer that reads your files off disk rather than
at a pinned commit waits for you to finish either way.

`rows` is what the runner keeps each **consumer's** queue depth from —
the `queued{from=<you>}` metric on every step that reads your output,
counting up as you seal and down as they read — with no store opened
to measure it. A step that cannot count leaves `rows` out; its
consumers' queue then reads as unknown rather than wrong. A
checkpoint you re-announce (which you should, until a consumer has
run — one arriving while the consumer is mid-pass is dropped) counts
once, and one that arrives after your outcome is ignored.

The `step` field is required by the schema but its value doesn't
matter — the runner re-tags every event with the authoritative step
id (children of `datalib-step` label sub-work `parent/child`, which
also just flows through).

Everything above lands in `<data_root>/system/runs/runs.sqlite` — plain
SQLite, one row per log line, the newest value per metric, every
step's state — for this run and the ones before it, which is what the
Manage screen reads. The store, its retention and how to read it:
[`logging.md`](logging.md).

### The outcome line

The last thing you may print is one `outcome` event — the content
version of each output you produced:

```json
{"event":"outcome","outputs":[
  {"path":"weather/ingest","version":"2026-07-21T06:00Z-a1b2","rows":12}
]}
```

`rows` is optional and means the same as on a `checkpoint`: what this
output gained since your last seal (or in all, if you never sealed) —
finishing is the last seal, as far as a consumer's queue is concerned.

For your output there are two cases, and that is the whole protocol:

* **`version`**: a content version you vouch for, such as a dolt commit
  hash, a row-set hash or a cursor hash. Trusted verbatim, and compared
  only for equality. The runner never opens your output to check it.
* **nothing** (omit the path, or the whole outcome line): each success
  counts as new, so every step that reads your output runs again after
  it. Always safe, and wasteful when nothing changed; the runner says so
  on the event stream (`info`: "reported no version for `<path>`, so
  every step reading it runs again").

**The version must be a function of the output's content.** Two runs
that leave the same data behind must report the same string, because
that string is the entire signal for "did this change?": a step that
did nothing this pass reports the version it reported last time, and
its consumers skip. There is no separate "unchanged" flag to assert;
unchanged is something the scheduler *derives* from two equal
versions. A timestamp, a run id, or a counter is not a version: it
moves every run and re-runs everything downstream, which is exactly
what reporting nothing already gets you.

**Spell one version the same way everywhere.** A checkpoint's `version`
and the outcome's are compared as strings, so if you finish on the
commit you last sealed, report it exactly as the checkpoint did, and
your consumers do not run again for it. A doltlite store's head is a
good version for both: it moves only when a commit changed something
([`doltlite.md`](doltlite.md#diffs)).

A step that did not run keeps the version recorded for its output last
time, or `datalib_dag::version::UNKNOWN` if there is none.

Reporting on any path but your own tree (`DATALIB_DAG_STEP`) is a
contract violation and fails the step. Exit `0` means success; the
outcome line is purely informational.

### Failure classification

On a non-zero exit, an outcome line lets you tell the scheduler *what
kind* of failure this is, which drives retry policy:

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
| `data` | bad input; retrying won't help | fail fast (default when absent) |
| `cancelled` | you were interrupted | fail fast, exit code 130 convention |

(`RetryPolicy` in `dag/src/scheduler.rs`.)

`outputs` on a failure outcome reports partial progress you *did*
commit. The scheduler records those versions, the next run resumes from
them, and your dependents read them now: a commit is a correct state.
That includes `cancelled`: a step that stops at a consistent point and
commits should report where it got to. A failure that reports nothing
moves nothing, since the runner cannot know your tree is whole.

### Rendering a source with no data

**A source that has never been downloaded is not a failure.** If your
render step finds no raw store and no legacy tree, emit nothing and
exit 0 — an empty output tree, not `failure: data`.

This is the normal state of every source in a freshly scaffolded
config: the user adds ten sources, authenticates one, and syncs it.
Failing there is wrong: `data` means "a human must look at this", and
the Manage row turns red for a source that has simply not been synced
yet.

An empty render is safe for the index: `grid_index` deletes per
document (`DELETE FROM grid_rows WHERE markdown_uuid = ?`), driven by
the markdown files actually present, so rendering nothing contributes
nothing rather than dropping another source's rows.

Keep failing, loudly, when the data is *there but wrong*: a store that
exists and can't be opened, a schema missing tables it should have, a
row that won't parse. The distinction is **absent vs. malformed**, and
only the second one is a `data` failure. In practice that falls out of
the ordinary shape — return empty on the "nothing exists" branch and
let every error below it propagate:

```rust
pub fn parse(path: &Path) -> Result<Parsed> {
    let db_path = db_path_for(path);
    if db_path.exists() {
        return parse_doltlite(&db_path);   // a broken store still errors
    }
    if path.is_dir() {
        return parse_json_dir(path);       // legacy tree
    }
    Ok(Parsed::default())                  // never downloaded — not an error
}
```

## stdin: nothing to read

**Your step is given `/dev/null` on stdin.** Read it and you get an
immediate end-of-file; there is no input channel.
Everything a step is told arrives as environment variables, the
`--params-file`, and its declared inputs.

## fd 3: the runner's pipe, if you want it

Every step is also handed a pipe on file descriptor 3. You can ignore it
completely — it costs one open descriptor and nothing else. What it is
for:

The runner normally stops a step by signalling it: SIGINT, then
SIGKILL fifteen seconds later if it is still there. Both need the runner
to be alive to run that code. A runner that is itself SIGKILLed —
or that aborts, or is taken by the OOM killer — runs nothing, and since
nothing else ever signals a step, a step that waits to be told would
carry on with its store open and nobody recording how the run ended.

What survives a SIGKILL is the kernel closing the dead process' file
descriptors. So the runner holds the write end of that pipe for exactly
as long as it lives. **End-of-file on fd 3 means the runner is gone.**

`DATALIB_PARENT_PIPE` holds the descriptor's number — read it rather
than hard-coding 3. In Rust it is one call at startup:

```rust
datalib_parent_watch::exit_with_parent(|| { /* stop; see below */ })?;
```

In any other language, watch that descriptor for end-of-file yourself.

### Why fd 3 and not stdin

A step is whatever `command` says, and a program that reads stdin
expecting the immediate end-of-file `/dev/null` gives would block
forever on a pipe nobody writes to. So stdin is left alone and the pipe
goes somewhere a program will only find if it looks. There is nothing to
configure: the pipe is always there, and watching it is your program's
business. (`datalib_parent_watch`'s crate doc has the general rule.)

### What to do when it closes

Take down what *you* spawned, not just yourself. The runner starts each
step as its own process-group leader — so that a signal aimed at a step
reaches a `node` the step wrapped — which makes `kill(0, SIGINT)`
exactly "me and my children". That is what `datalib-step` does, and its
ordinary SIGINT handler then seals and exits.

A step that ignores fd 3 is simply one the runner cannot clean up after
being SIGKILLed: it keeps running until something else stops it.

## stderr: logging

stderr is yours for humans: every line is captured into the event
stream as an `info` log. So is every stdout line that is not an
event. If you exit non-zero, the last few stderr lines a person could
not find by reading "it failed" — plain lines, and the message of any
structured `warn` or `error` line — become the step's error message;
structured `info` lines stay in the log. The runner writes that
message into the log too, as the step's last line at `error` level,
and that line is where the Manage row's double-click opens the log.
A step that exits after a cancel (`failure: cancelled`) is recorded
as stopped, its message says so, and its line is a `warn`.

Each line records which pipe it came from (`stream`) and is
timestamped as the runner reads it, and the two pipes are read
concurrently, so the log is in arrival order across both.

**Flush per line.** Arrival order is only as good as the child's
buffering: a program that block-buffers its stdout when it is a pipe
(C's stdio, Python's by default) hands the runner its progress in 4KB
lumps, minutes after the stderr they belonged beside. The runner sets
`PYTHONUNBUFFERED=1` for every child; Rust's stdout is line-buffered
regardless and `sh` writes through. Anything else should flush after
each line it wants seen on time.

A structured tracing-JSON line (with a `level` field, as
tracing-subscriber's JSON format writes) is unwrapped rather than
quoted: its `fields.message` becomes the log's `msg`; its `timestamp`,
`level`, `target` and thread (`threadName`, else `threadId`) become
columns — the line's own timestamp wins over the runner's arrival time
— and everything else it carried (`filename`, `line_number`, the
remaining `fields`) rides along as `fields`. The Manage screen shows
the sentence, not the envelope, and can still sort by thread.

Every level is kept, `debug` and `trace` included: the runner stores a
`DEBUG` envelope as a `debug` row rather than rounding it up to `info`.

**Your attempt is a process.** Every line names the process that
wrote it, and each attempt of a step is one: the runner records it at
spawn and again at `wait(2)` with the `exit_code` or `signal` that
ended it, and its commit — the runner's own when the step is the
built-in program, none for a custom command — which is what a
structured line's `filename` and `line_number` are relative to. What
the runner itself says *about* your step is the runner's line, with
your step as its subject. The model, the other kinds of process and
the log card that reads them: [`logging.md`](logging.md).

## Reset (optional)

`datalib-dag --reset <step-id>[,<step-id>…]` (or the app's
Reset, `POST /api/reset`) empties what a step wrote so the next run
does its work from the start: the
runner invokes the step once with `--reset store` appended,
then forgets the step ever succeeded and records
the version the reset reports, or a new one if it reports none, so
everything reading the tree runs again. Empty that part of your tree, keep whatever
history you keep, commit if you commit, exit 0, and do nothing else: no
inputs are resolved and no sync follows unless `--sync` was also given.
A command that does not know the verb exits non-zero and nothing is
changed. `datalib-step` deletes every row of the tree's store in one
commit, keeping the tables, plus a render tree's documents; the run log
and the rest are still in the history. Keeping the tables is what lets
a reader take the emptiness in as ordinary deletions: the app's Reset
then syncs what reads the step, so its documents leave the grid. The app
does not wait for a sync to end: the loop stops the step if it runs and
resets it there and then (`datalib/backend/dag/README.md` §"Resets and
purges"). A reset
also reports the metrics a run would (`problems`, and `documents` for a
render step), counted off the emptied store: the Manage row shows a
step's newest sample, and the reset step itself does not run again
until the next sync.

Nothing resets an ingest step's blob CAS, `blobs.sqlite`: a reset keeps
the bytes, and the refetch lands on them. To get the space back, delete
the file **and** reset the ingest step, together. Deleting the file
alone leaves edge rows naming bytes that are gone, and the download
does not fetch what its edge rows say it already has.

## Migrate

The first time a build runs on a root, before the loop takes any
request, the process that holds the runner lock invokes every step that
takes the verb once, producers before their readers, with `--migrate`
appended ([dag README](../../datalib/backend/dag/README.md) §
"Upgrading a root"). The runner knows nothing of what a step wrote; the
step decides, and answers in its outcome line:

* **nothing to do** — what it wrote is in this build's shape, or it has
  written nothing yet: exit 0, report nothing.
* **migrated** — it brought what it wrote to this build's shape in
  place, fetching nothing: report the output's new version as a run
  would. The runner records it and keeps the step's last success, so
  what reads the output is stale.
* **needs a rerun** — what it wrote is in a shape it cannot reach in
  place: add `"needs_rerun": true` to the outcome. The runner holds the
  step due until it next succeeds, and the app offers to run it; a sync
  that does not reach it leaves it alone.

```json
{"event":"outcome","needs_rerun":true,"outputs":[]}
```

Only `datalib-step`'s own steps take the verb today (the loader sets
`StepSpec::migrates` for a step with no `command`). An ingest step opens
its raw store with the provider's migration ladder and closes it: every
rung, the additive DDL and an old blob store's conversion run, and each
is sealed to `main`. A render step and the grid index compare their
store's `_datalib_meta.schema_hash` with this build's and answer
`needs_rerun` when it differs; a derived store is rebuilt, never
migrated. The qmd steps have nothing to answer.

## Signals: graceful cancellation (optional)

On cancellation (Ctrl-C, the UI's Stop, or the step turned off) the runner sends your
process **SIGINT** and gives you fifteen seconds. The right response is
to **stop at your next consistent point, commit there, and exit 130**
with a `{"event":"outcome","failure":"cancelled"}` line: what you
committed stands, and the next run resumes from your cursor. **Never
commit from the signal handler itself** — a commit made wherever the
signal happened to land publishes a half-written batch to every
reader. If you do nothing, you are killed at the grace, and the next
writer's `open` discards whatever you wrote after your last commit.

`datalib-step` does this for the built-in ingests: SIGINT raises a
stop flag (`datalib_etl::stop::StopFlag`, on `DownloadControl`) that
the fetch loops read before starting a unit of work — a channel, a
conversation, a batch of messages — that makes the seal path seal at
the next consistent point whatever the cadence says, and that ends a
backoff sleep and refuses to send a new request at the HTTP chokepoint.
The run then returns as a shorter run, `finish` commits blobs then
entities, and the step reports `cancelled`. A stopped run does not
record its scope config as satisfied, so a widened filter interrupted
part-way is backfilled by the next run rather than believed done.
`grid_index` reads the same flag between documents: it rolls back the
source it is loading and keeps the ones it has already committed.

## Minimal examples

A shell step, no protocol at all (each success counts as new, so what
reads its tree runs after every sync):

```toml
[[steps]]
id = "notes/ingest"
command = "sh -c 'mkdir -p notes/ingest && cp -R \"$HOME/notes/.\" notes/ingest/'"
```

A python step using inputs + progress + outcome:

```python
#!/usr/bin/env python3
import hashlib, json, os, pathlib, sys

inputs = [p for p in os.environ["DATALIB_DAG_INPUTS"].split("\n") if p]
changed = set(os.environ["DATALIB_DAG_CHANGED_INPUTS"].split("\n"))
root = pathlib.Path(os.environ["DATALIB_DAG_DATA_ROOT"])
out = os.environ["DATALIB_DAG_STEP"]   # the one tree this step writes
args = dict(zip(sys.argv[1::2], sys.argv[2::2]))
params = json.load(open(args["--params-file"])) if "--params-file" in args else {}

def emit(obj): print(json.dumps(obj), flush=True)

emit({"event": "progress_length", "step": "", "total": len(inputs)})
for src in inputs:
    if src in changed:
        pass  # ... process only what moved ...
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

The built-in step types are one binary implementing this protocol,
run with no arguments of its own. It reads `DATALIB_DAG_FUNCTION` to
learn what to do — `ingest`, `render_markdown`, `keyword_index`,
`embed`, `grid_index`, `qmd_aggregator` or `embedding_map`; anything else is
refused with the list — and `DATALIB_DAG_GROUP_TYPE` to learn which
provider to run, which `ingest` and `render_markdown` require and the
rest ignore. It
writes the tree `DATALIB_DAG_STEP` names, after checking that it is
`<DATALIB_DAG_GROUP>/<DATALIB_DAG_FUNCTION>`; a render reads its raw
store from the first entry of `DATALIB_DAG_INPUTS`. It reads the
params file as the provider's **function-specific** config — the ingest
step carries the provider's download config (`common` envelope, the method table
block, …), the render step only the render knobs (nothing for most
providers; beeper/signal `period`, perseus `alignment_pairs`, email
`outlink_format`/`only_render_labels`) — honors `DATALIB_DAG_NOW`,
`--reset` and `--migrate`, stops at its next consistent point on SIGINT and
commits there, and emits versions where it has them (the grid index claims its dolt commit hash). Use it as the
reference implementation.

The index functions' ids are fixed at `unified_index/<function>`, and
the qmd steps share one index file; the rules are in
[`config_model.md`](config_model.md) § "Naming rules".
