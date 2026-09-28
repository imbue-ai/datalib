# Example configs

[`dag_example.toml`](dag_example.toml) is a complete, commented config:
a `[[groups]]` entry per source, `[[steps]]` entries declared as
`group` + `function` (a built-in one names no `command`), and edges
from each step's declared `inputs`. Its header comment explains the
shape; [`docs/dev/step_protocol.md`](../docs/dev/step_protocol.md) says
what a step with a `command` must do.

## Running a config

Build the binary directory and point the runner at a config:

```sh
bazelisk build //datalib/backend:bin
bazel-bin/datalib/backend/bin/datalib-dag configs/dag_example.toml
```

`//datalib/backend:bin` stages every shipped binary under its public
dash-separated name (`datalib-dag`, `datalib-step`, `datalib-http`, …)
in one directory — the same layout `scripts/install.sh` produces.
`datalib-dag` puts its own directory at the front of every step's
`PATH`, so `datalib-step` is found with no flag. Pass `--binary-dir DIR`
only when the binaries live somewhere else.

## Tiny run

The "tiny" config (a handful of sources, used by the manual e2e live-sync
golden test) lives outside this repo, because its source data is
slightly sensitive and the repo is public. It is `dag.toml` in the
private `data_liberation_manual_e2e_test_data` directory. You rarely
need to point the runner at it by hand —
`datalib/backend/dag/manual_e2e_run.sh` does that, and its `--config`
validates it offline. See [`docs/dev/testing.md`](../docs/dev/testing.md).
