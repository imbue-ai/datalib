

Configs here are in the new steps format — each step is an explicit
`command:` and edges are derived from artifact paths. See the header
comment of `dag_example.yaml` and `docs/dev/step_protocol.md`.

## Running a config

Build the DAG runner and the step binary, symlink the step binary
under its `datalib-step` name, stage the `datalib-step-*` wrapper
scripts next to it, and point the runner at a config:

```sh
bazelisk build //frankweiler/backend/dag:datalib_dag \
               //frankweiler/backend/datalib_step:datalib_step
bindir=$(mktemp -d) && ln -s "$PWD"/bazel-bin/frankweiler/backend/datalib_step/datalib_step "$bindir"/datalib-step
sh frankweiler/backend/datalib_step/stage_wrappers.sh "$bindir"
bazel-bin/frankweiler/backend/dag/datalib_dag configs/dag_example.yaml \
    --binary-dir "$bindir"
```

## Tiny run

The "tiny" config (a handful of sources, used by the manual e2e live-sync
golden test) now lives OUTSIDE this repo so its slightly sensitive source
data isn't shared when the repo is open-sourced. It's in the private
`data_liberation_manual_e2e_test_data` dir; point the runner above at
its config (which must be in the new steps format).
