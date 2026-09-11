# Writing a step command

The sync engine (`datalib-dag`) runs a DAG of arbitrary commands. Any
executable can be a step: the runner spawns it, feeds it what it
declared in the config, watches its stdout/stderr, and hashes its
outputs. Everything beyond "run a program and exit 0/non-0" is an
*optional* protocol layer — a plain shell script is a valid step, and
each layer you adopt buys better incrementality, progress reporting,
or failure handling.

This doc is the contract from the command's point of view. The
runner/scheduler side (edge derivation, skipping, retry, subtree
poisoning) is in `pipeline_dag_architecture.md`.

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
| `--params <json>` | the entry's `params` subtree, converted TOML → JSON (TOML dates/times arrive as their string form) |
| `--inputs <json>` | the entry's `inputs`, as a JSON string array |

So the entry above runs
`fetch-weather --station KSFO --params {"units":"metric"}`. A command
that takes no flags at all still works — declare no params and no
inputs and it sees nothing extra (a `sh -c 'script'` step receives
whatever is appended as `$0`/positional args and can drop them). There
is no `--outputs`: the one tree a step writes is its id, which arrives
in the environment.

**Environment** — the identity/context channel:

| variable | meaning |
| --- | --- |
| `DATALIB_DAG_STEP` | this step's id, and the one tree it writes (`weather/ingest`) |
| `DATALIB_DAG_GROUP` | the group it is filed under (`weather`); unset for a step outside any group |
| `DATALIB_DAG_GROUP_TYPE` | the group's `type`, when it declares one |
| `DATALIB_DAG_FUNCTION` | what this step does within its group (`ingest`); unset for a step outside any group |
| `DATALIB_DAG_DATA_ROOT` | absolute path of the data root (== cwd) |
| `DATALIB_DAG_INPUTS` | resolved input artifacts, `\n`-separated, relative to the data root |
| `DATALIB_DAG_CHANGED_INPUTS` | the subset of the above whose version moved since this step's last success; empty when there is no last success to compare against (never completed, or the step's own config changed) — do all your work |
| `DATALIB_DAG_NOW` | the run's pinned timestamp (RFC 3339). Stamp times with this instead of sampling your own clock, so one run's outputs agree |
| `DATALIB_DAG_RESET_AND_REDOWNLOAD` | `1` when the user asked for a from-scratch re-ingest — honor it if you bring data in from outside the pipeline, whether that is re-fetching from an origin or re-reading your files in full; a step whose inputs are other steps' trees ignores it |
| `DATALIB_DAG_REFETCH_BLOBS` | `1` when the user asked for attachments/blobs to re-fetch |

plus anything in the entry's `env:` map (which wins over the run-wide
values on collision).

## The rules you must follow

These are what the scheduler's correctness rests on:

* **Write only under your own tree** — the one `DATALIB_DAG_STEP`
  names. No two steps write one tree; the loader refuses a config where
  two ids coincide or nest.
* **Be idempotent.** Retries and re-runs simply invoke you again; a
  re-run over unchanged inputs must be safe (and ideally cheap).
* **Commit outputs atomically.** Don't leave a torn tree on the
  success path; if you die mid-write, the next run must be able to
  recover (the scheduler re-hashes outputs that made no claim).

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

A step that counts one thing and knows its total may use the shorter
form instead, which the runner translates into the `done` and `queued`
metrics for it:

```json
{"event":"progress_length","step":"me","total":42}
{"event":"progress_inc","step":"me","delta":1}
```

`progress_message` is the step's own words — a phase, not a number.
`log`'s `target` and `fields` are optional; a plain `msg` is fine.

The `step` field is required by the schema but its value doesn't
matter — the runner re-tags every event with the authoritative step
id (children of `datalib-step` label sub-work `parent/child`, which
also just flows through).

Everything above lands in `<data_root>/system/runs.sqlite` — plain
SQLite, one row per log line, the newest value per metric, every
step's state — for this run and the ones before it, which is what the
Manage screen reads. `[run_history]` in the config sets how many runs
are kept.

### The outcome line

The last thing you may print is one `outcome` event — the content
version of each output you produced:

```json
{"event":"outcome","outputs":[
  {"path":"weather/ingest","version":"2026-07-21T06:00Z-a1b2"}
]}
```

There are two cases per declared output, and that is the whole
protocol:

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
commit — the scheduler records those versions so the next run resumes
from them, while dependents stay blocked this run.

### Rendering a source with no data

**A source that has never been downloaded is not a failure.** If your
render step finds no raw store and no legacy tree, emit nothing and
exit 0 — an empty output tree, not `failure: data`.

This is the normal state of every source in a freshly scaffolded
config: the user adds ten sources, authenticates one, and syncs it.
Failing there is wrong twice over. `data` means "a human must look at
this", and it poisons the subtree — the shared `grid_index` / `qmd_index`
fan-in depends on *every* source's `render_markdown`, so one un-downloaded
source blocks the index for the sources that did sync.

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

## stderr: logging

stderr is yours for humans: every line is captured into the event
stream as an `info` log, and the last ~20 lines become the error
message if you exit non-zero. A structured tracing-JSON line (with a
`level` field, as tracing-subscriber's JSON format writes) is
unwrapped rather than quoted: its `fields.message` becomes the log's
`msg`, its `target` and severity are kept, and its other fields ride
along as `fields` — so the Manage screen shows the sentence, not the
envelope.

## Signals: graceful cancellation (optional)

On cancellation (Ctrl-C, or the UI's cancel) the runner sends your
process **SIGINT** and waits. If you can, checkpoint-commit your
partial state, print a `{"event":"outcome","failure":"cancelled"}`
line, and exit 130. If you do nothing, you'll be killed after a grace
period and the next run re-derives from whatever landed on disk —
correct, just wasteful.

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
params = json.loads(args.get("--params", "{}"))

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
store from the first entry of `DATALIB_DAG_INPUTS`. It reads
`--params` as the provider's **function-specific** config — the ingest
step carries the provider's download config (`common` envelope, the method table
block, …), the render step only the render knobs (nothing for most
providers; beeper/signal `period`, perseus `alignment_pairs`, email
`outlink_format`/`only_render_labels`) — honors `DATALIB_DAG_NOW` and
the reset env vars, checkpoints on SIGINT, and emits versions where it
has them (the grid index claims its dolt commit hash). Use it as the
reference implementation.

The two index functions have one reader, the `unified_index` applet,
which finds them from the data root alone; so their ids are fixed at
`unified_index/grid_index` and `unified_index/qmd_index`, and
`datalib-step` refuses to run them under any other.
