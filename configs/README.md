

Configs here are TOML, in the steps format — each step is a
`[[steps]]` table with an explicit `command`, and edges are derived
from artifact paths. See the header comment of `dag_example.toml` and
`docs/dev/step_protocol.md`.

## Running a config

Build the binary directory and point the runner at a config:

```sh
bazelisk build //datalib/backend:bin
bazel-bin/datalib/backend/bin/datalib-dag configs/dag_example.toml
```

`//datalib/backend:bin` stages every shipped binary under its public
dash-separated name (`datalib-dag`, `datalib-step`, `datalib-http`, …)
in one directory — the same layout `scripts/install.sh` produces. That
matters because `datalib-dag` resolves a step's `command` against PATH
with its own directory as the fallback, so `datalib-step` is found with
no flag. Pass `--binary-dir DIR` only when the binaries live somewhere
else.

## Tiny run

The "tiny" config (a handful of sources, used by the manual e2e live-sync
golden test) lives OUTSIDE this repo so its slightly sensitive source data
isn't shared when the repo is open-sourced. It's in the private
`data_liberation_manual_e2e_test_data` dir, in the steps format. You rarely
need to point the runner at it by hand — `datalib/backend/dag/manual_e2e_run.sh`
does that, and `--config` validates it offline. See
[`/docs/dev/testing.md`](/docs/dev/testing.md).

That config predates the TOML switch: convert it once with
`datalib-migrate-config <path>/dag.yaml -o <path>/dag.toml` (and repoint
`manual_e2e_run.sh`), since `datalib-dag` no longer reads YAML.
