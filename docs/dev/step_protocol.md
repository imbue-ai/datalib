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

```yaml
steps:
  - id: weather.download
    command: fetch-weather --station KSFO   # split shell-style
    outputs: [weather/raw]
    params:                                  # arbitrary YAML, yours
      units: metric
    env:                                     # extra child environment
      WEATHER_DEBUG: "1"
```

`command:` is one string, split into an argv shell-style (quotes and
backslash escapes work; there is **no** variable expansion, globbing,
or piping — wrap in `sh -c '…'` if you need real shell). The first
word resolves via `PATH`, with the runner's `--binary-dir` (default:
the directory `datalib-dag` itself lives in; also settable as
`binary_dir:` in the config) prepended — which is how `datalib-step`
is found without an absolute path.

## What your command receives

**Working directory** — the data root. All artifact paths are relative
to it.

**Appended flags** — the runner mechanically appends the entry's
declared fields to your argv, each only when present/non-empty:

| flag | value |
| --- | --- |
| `--params <json>` | the entry's `params:` subtree, converted YAML → JSON |
| `--inputs <json>` | the entry's `inputs:` patterns, as a JSON string array |
| `--outputs <json>` | the entry's `outputs:` paths, as a JSON string array |

So the entry above runs
`fetch-weather --station KSFO --params {"units":"metric"} --outputs ["weather/raw"]`.
A command that takes no flags at all still works — declare no params
and it only ever sees `--inputs`/`--outputs`, which it may ignore (a
`sh -c 'script'` step receives them as `$0`/positional args and can
drop them).

**Environment** — the identity/context channel:

| variable | meaning |
| --- | --- |
| `FRANKWEILER_DAG_STEP` | this step's config `id` |
| `FRANKWEILER_DAG_DATA_ROOT` | absolute path of the data root (== cwd) |
| `FRANKWEILER_DAG_INPUTS` | resolved input artifacts, `\n`-separated, relative to the data root — wildcards in `inputs:` are already expanded against producer outputs |
| `FRANKWEILER_DAG_CHANGED_INPUTS` | the subset of the above whose version moved since this step's last success; empty on a first run |
| `FRANKWEILER_DAG_NOW` | the run's pinned timestamp (RFC 3339). Stamp times with this instead of sampling your own clock, so one run's outputs agree |
| `FRANKWEILER_DAG_RESET_AND_REDOWNLOAD` | `1` when the user asked for a from-scratch re-fetch — honor it if you fetch from an origin, ignore otherwise |
| `FRANKWEILER_DAG_REFETCH_BLOBS` | `1` when the user asked for attachments/blobs to re-fetch |

plus anything in the entry's `env:` map (which wins over the run-wide
values on collision).

## The rules you must follow

These are what the scheduler's correctness rests on:

* **Write only under your declared `outputs:`.** No two steps'
  outputs may overlap; edges are derived purely from one step's
  outputs matching another's inputs.
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
the UI's task board:

```json
{"event":"progress_length","step":"me","total":42}
{"event":"progress_inc","step":"me","delta":1}
{"event":"progress_message","step":"me","msg":"fetching page 3"}
{"event":"log","step":"me","level":"info","msg":"hello"}
```

The `step` field is required by the schema but its value doesn't
matter — the runner re-tags every event with the authoritative step
id (children of `datalib-step` label sub-work `parent/child`, which
also just flows through).

### The outcome line

The last thing you may print is one `outcome` event — your report on
what actually changed:

```json
{"event":"outcome","outputs":[
  {"path":"weather/raw","changed":true,"version":"2026-07-21T06:00Z-a1b2"}
]}
```

Per declared output you can claim, in order of preference:

* `version` — a logical content version you vouch for (a row-set
  hash, a dolt commit hash). Trusted verbatim; the cheapest and most
  precise change signal.
* `changed: true/false` — no version, just the fact. `false` carries
  the previous version forward; `true` makes the scheduler
  content-hash the tree.
* nothing (omit the path, or the whole outcome line) — the scheduler
  blake3-hashes the output tree and decides for itself. Always
  correct, just slower and mtime-blind (content only).

Claiming a path you didn't declare in `outputs:` is a contract
violation and fails the step. Exit `0` means success; the outcome
line is purely informational.

### Failure classification

On a non-zero exit, an outcome line lets you tell the scheduler *what
kind* of failure this is, which drives retry policy:

```json
{"event":"outcome","failure":"rate_limited","outputs":[
  {"path":"weather/raw","changed":true}
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

## stderr: logging

stderr is yours for humans: every line is captured into the event
stream as an `info` log, and the last ~20 lines become the error
message if you exit non-zero. Structured tracing-JSON lines (with a
`level` field) keep their own `warn`/`error` severity.

## Signals: graceful cancellation (optional)

On cancellation (Ctrl-C, or the UI's cancel) the runner sends your
process **SIGINT** and waits. If you can, checkpoint-commit your
partial state, print a `{"event":"outcome","failure":"cancelled"}`
line, and exit 130. If you do nothing, you'll be killed after a grace
period and the next run re-derives from whatever landed on disk —
correct, just wasteful.

## Minimal examples

A shell step, no protocol at all (scheduler hashes the output tree):

```yaml
  - id: notes.import
    command: sh -c 'mkdir -p notes/raw && cp -R "$HOME/notes/." notes/raw/'
    outputs: [notes/raw]
```

A python step using inputs + progress + outcome:

```python
#!/usr/bin/env python3
import json, os, sys

inputs = [p for p in os.environ["FRANKWEILER_DAG_INPUTS"].split("\n") if p]
changed = set(os.environ["FRANKWEILER_DAG_CHANGED_INPUTS"].split("\n"))
args = dict(zip(sys.argv[1::2], sys.argv[2::2]))
params = json.loads(args.get("--params", "{}"))
outputs = json.loads(args["--outputs"])

def emit(obj): print(json.dumps(obj), flush=True)

emit({"event": "progress_length", "step": "", "total": len(inputs)})
for src in inputs:
    if src in changed:
        pass  # ... process only what moved ...
    emit({"event": "progress_inc", "step": "", "delta": 1})

emit({"event": "outcome",
      "outputs": [{"path": outputs[0], "changed": bool(changed)}]})
```

## How `datalib-step` fits

The built-in step types are just one binary implementing this
protocol: `datalib-step download|render <provider>` (plus
`grid_index` / `qmd_index`). It derives its source name from the first
`--outputs` entry (`slack/raw` → `slack`), reads `--params` as the
provider's **phase-specific** config — the download step carries the
provider's download config (`common:` envelope, `sync:` block, …),
the render step only the render knobs (nothing for most providers;
beeper/signal `period`, perseus `alignment_pairs`, email
`outlink_format`/`only_render_labels`) — honors `FRANKWEILER_DAG_NOW`
and the reset env vars, checkpoints on SIGINT, and emits versions
where it has them (the grid index claims its dolt commit hash). Use
it as the reference implementation.
