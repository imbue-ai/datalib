# Writing a step command

The sync engine (`datalib-dag`) runs a DAG of arbitrary commands. Any
executable can be a step: the runner spawns it, feeds it what it
declared in the config, watches its stdout/stderr, and hashes its
outputs. Everything beyond "run a program and exit 0/non-0" is an
*optional* protocol layer — a plain shell script is a valid step, and
each layer you adopt buys better incrementality, progress reporting,
or failure handling.

This doc is the contract from the command's point of view. The
runner/scheduler side (staleness, retry, what a dropped entry costs)
is in `datalib/backend/dag/README.md`.

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
| `DATALIB_DAG_RUN_ID` | the run this invocation belongs to — a UUID the loop mints for each busy period (the stretch from idle to busy and back, serving every request that arrives meanwhile), or whatever the caller passed to `datalib-dag` as `--run-id`; a UI job names it as its `parent_job_id`. Every row in `system/runs/runs.sqlite` carries it; stamp it into anything you write that should be joinable back to the run. The built-in steps end every doltlite commit message with ` run=<id>` (`doltlite_raw::stamp_run`), which is how the Manage screen's commit history gets from a commit to its log |
| `DATALIB_DAG_ATTEMPT` | which invocation of this step within the run, starting at `1`; a retry or a streaming pass counts up |
| `DATALIB_DAG_GROUP` | the group it is filed under (`weather`); unset for a step outside any group |
| `DATALIB_DAG_GROUP_TYPE` | the group's `type`, when it declares one |
| `DATALIB_DAG_FUNCTION` | what this step does within its group (`ingest`); unset for a step outside any group |
| `DATALIB_DAG_DATA_ROOT` | absolute path of the data root (== cwd) |
| `DATALIB_DAG_INPUTS` | resolved input artifacts, `\n`-separated, relative to the data root |
| `DATALIB_DAG_CHANGED_INPUTS` | the subset of the above whose version moved since this step's last success; empty when there is no last success to compare against (never completed, or the step's own config changed) — do all your work |
| `DATALIB_READS` | a JSON object, input path → the version the runner started this invocation against; an input with no version yet is absent. A version for what you *read*, where the output's own would say less (the qmd index reports a hash of this) |
| `DATALIB_DAG_NOW` | the run's pinned timestamp (RFC 3339). Stamp times with this instead of sampling your own clock, so one run's outputs agree |
| `DATALIB_DAG_RESET` | set only by `datalib-dag --reset`, and then this invocation is a reset, not a run: see § Reset |
| `RUST_LOG` | the run's log filter, in `tracing-subscriber`'s grammar: the config's `log_level` (`trace` when unset) for datalib's own crates, third-party crates no lower than `debug`, the noisy ones at `warn`. A `RUST_LOG` already set where the runner was started is passed through instead. A step in another language may honor it or ignore it; what it prints is kept regardless |

plus anything in the entry's `env:` map (which wins over the run-wide
values on collision).

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
{"event":"metric","step":"me","name":"rows_upserted","labels":{"table":"messages"},"value":1234}
{"event":"metric","step":"me","name":"queued","value":17}
{"event":"progress_message","step":"me","msg":"fetching page 3"}
{"event":"log","step":"me","level":"info","msg":"hello","target":"me::fetch","fields":{"page":3}}
```

**`metric` is a current value, never a delta.** Name the thing counted
(`rows_upserted`, `api_requests`, `bytes_fetched`) and send the total so
far each time it moves; `labels` is optional and splits one name into
series (`table=messages`). A value that goes down is simply a gauge, and
the one gauge the UI looks for is **`queued`** — how much work is ahead
of you right now, which you usually know even when you cannot know the
total. Absolute values are what make the runner's coalescing lossless:
it keeps the newest value per series, and a dropped position costs
nothing where a dropped increment would be lost work.

Two names are read by name rather than just drawn. **`documents`** is
how many documents your output store holds — whole store, not this run
— and fills the Manage screen's Documents column; **`problems`**, with
a `severity=error` or `severity=warning` label, fills its Problems
column. Report each one every run, zero included: a missing series
means "never counted" and draws as a blank cell, which is what you want
a step that does not count either of them to leave behind. They are
`datalib_metrics::DOCUMENTS` and `datalib_problems::METRIC` in the
tree; nothing else makes the reporter and the column agree on the
spelling.

A step that counts one thing and knows its total may use the shorter
form instead, which the runner translates into the `done` and `queued`
metrics for it:

```json
{"event":"progress_length","step":"me","total":42}
{"event":"progress_inc","step":"me","delta":1}
```

`progress_message` is the step's own words — a phase, not a number.
`log`'s `target` and `fields` are optional; a plain `msg` is fine.

A step that seals part of its output while still running says so with
a `checkpoint` (the streaming protocol in
`docs/dev/plans/streaming_steps_plan.md`), and should say how many rows
that seal added:

```json
{"event":"checkpoint","step":"me","version":"a1b2c3","rows":340}
```

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

**If your tree holds doltlite stores, you need not report a version
at all.** The runner reads the commit each `*.doltlite_db` at the top of
your tree has on `main`, after every invocation and at every checkpoint,
and uses that; anything you report for such a tree is not consulted.
Publish before you checkpoint (`commit_run` does), and the checkpoint
means what it says.

For any other tree there are two cases per declared output, and that is
the whole protocol:

* **`version`** — a content version you vouch for: a dolt commit hash,
  a row-set hash, a cursor hash. Trusted verbatim, and compared only
  for equality.
* **nothing** (omit the path, or the whole outcome line) — the
  scheduler blake3-hashes the output tree and decides for itself.
  Always correct, and always slower: it reads every byte under the
  output.

**The version must be a function of the output's content.** Two runs
that leave the same data behind must report the same string, because
that string is the entire signal for "did this change?" — a step that
did nothing this pass reports the version it reported last time, and
its consumers skip. There is no separate "unchanged" flag to assert;
unchanged is something the scheduler *derives* from two equal
versions. A timestamp, a run id, or a counter is not a version: it
moves every run and re-runs everything downstream forever.

If you cannot cheaply derive one, omit the output and let the
scheduler hash. That is correct, just slower — and for a big output
(a raw store with a blob CAS, a large rendered tree) the difference is
substantial, so prefer a logical version wherever the underlying store
already has one. When the scheduler does hash, it says so on the event
stream (`info`: "reported no version for `<path>`; reading the whole
tree to hash it"), so the cost is visible rather than showing up as an
unexplained pause.

The hash only ever happens for a step that **ran**. The runner does
not hash a tree on behalf of a step it skipped: a skipped step's
output keeps the version recorded for it last time, or
`datalib_dag::version::UNKNOWN` if there is none. So omitting a
version costs a tree read per invocation, not per run of the pipeline.

Claiming a path you didn't declare in `outputs` is a contract
violation and fails the step. Exit `0` means success; the outcome
line is purely informational.

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
| `transient` | network blip, lock contention | retry soon |
| `rate_limited` | HTTP 429 and friends | retry with backoff |
| `auth` | credentials need a human | fail fast |
| `data` | bad input; retrying won't help | fail fast (default when absent) |
| `cancelled` | you were interrupted | fail fast, exit code 130 convention |

`outputs` on a failure outcome reports partial progress you *did*
commit. The scheduler records those versions, the next run resumes from
them, and your dependents read them now: a commit is a correct state.

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

The scheduler models the same distinction: an artifact that doesn't
exist hashes to the distinguished version `absent`
(`datalib_dag::version::ABSENT`) rather than being an error state.

## stdin: nothing to read

**Your step is given `/dev/null` on stdin**, as it always has been. Read
it and you get an immediate end-of-file; there is no input channel.
Everything a step is told arrives as environment variables, the
`--params-file`, and its declared inputs.

## fd 3: the runner's pipe, if you want it

Every step is also handed a pipe on file descriptor 3. You can ignore it
completely — it costs one open descriptor and nothing else. What it is
for:

The runner normally stops its steps by signalling them: SIGINT on a
cancel, SIGKILL for anything still running as it exits. Both need the
runner to be alive to run that code. A runner that is itself SIGKILLed —
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

Other datalib spawners do put this pipe on stdin — the gateway's
applets, the desktop shell's `datalib-http` — because they know exactly
what program they are starting. The runner does not: a step is whatever
`command` says. A program that reads stdin expecting the immediate
end-of-file `/dev/null` gives would block forever on a pipe nobody ever
writes to, and a hung step holding its store open is a worse failure
than the orphan the pipe prevents. So stdin is left alone and the pipe
goes somewhere a program will only find if it looks.

This is also why there is no configuration for it. There is nothing to
opt into and nothing to get wrong: the pipe is always there, and
watching it is your program's business.

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
event. If you exit non-zero, the last few lines a person could not
find by reading "it failed" — plain lines, and the message of any
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

Every level is kept, `debug` and `trace` included: a built-in step
logs its own lines down to the level the run's `RUST_LOG` names
(`datalib_log_filter`; the config's `log_level`, `trace` when unset)
— a doltlite commit, a batch of rows upserted, a request being
retried, each with its numbers in the sentence — and the runner stores
a `DEBUG` envelope as a `debug` row rather than rounding it up to
`info`.

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

`datalib-dag --reset <step-id>[+<more>]` empties
what a step wrote so the next run does its work from the start: the
runner invokes the step once with `DATALIB_DAG_RESET` set to `store`, or
to whatever followed the `+` (`blobs`: the built-in ingest step's store
*and* its blob CAS), then forgets the step ever succeeded and records
its tree's new version. Empty that part of your tree, keep whatever
history you keep, commit if you commit, exit 0, and do nothing else: no
inputs are resolved and no sync follows unless `--sync` was also given.
A command that does not know the verb exits non-zero and nothing is
changed. `datalib-step` deletes every row of the tree's store in one
commit, keeping the tables, plus a render tree's documents; the run log
and the rest are still in the history. Keeping the tables is what lets
a reader take the emptiness in as ordinary deletions: the app's Reset
then syncs what reads the step, so its documents leave the grid.

## Signals: graceful cancellation (optional)

On cancellation (Ctrl-C, or the UI's cancel) the runner sends your
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

## Minimal examples

A shell step, no protocol at all (scheduler hashes the output tree):

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
learn what to do — `ingest`, `render_markdown`, `grid_index` or
`qmd_index`; anything else is refused with the list — and
`DATALIB_DAG_GROUP_TYPE` to learn which provider to run, which the two
per-source functions require and the two index functions ignore. It
writes the tree `DATALIB_DAG_STEP` names, after checking that it is
`<DATALIB_DAG_GROUP>/<DATALIB_DAG_FUNCTION>`; a render reads its raw
store from the first entry of `DATALIB_DAG_INPUTS`. It reads the
params file as the provider's **function-specific** config — the ingest
step carries the provider's download config (`common` envelope, the method table
block, …), the render step only the render knobs (nothing for most
providers; beeper/signal `period`, perseus `alignment_pairs`, email
`outlink_format`/`only_render_labels`) — honors `DATALIB_DAG_NOW` and
`DATALIB_DAG_RESET`, stops at its next consistent point on SIGINT and
commits there, and emits versions where it has them (the grid index claims its dolt commit hash). Use it as the
reference implementation.

The two index functions have one reader, the `unified_index` applet,
which finds them from the data root alone; so their ids are fixed at
`unified_index/grid_index` and `unified_index/qmd_index`, and
`datalib-step` refuses to run them under any other.
