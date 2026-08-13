

Configs here are TOML, in the steps format — each step is a
`[[steps]]` table with an explicit `command`, and edges are derived
from artifact paths. See the header comment of `dag_example.toml` and
`docs/dev/step_protocol.md`.

(`thad_beeper.yaml` is the exception: it's a specimen of the retired
stanza-based `sources:` format, which was only ever YAML. It's kept as
migration test material, not as something to copy.)

## Running a config

Build the DAG runner and the step binary, symlink the step binary
under its `datalib-step` name, and point the runner at a config:

```sh
bazelisk build //datalib/backend/dag:datalib_dag_bin \
               //datalib/backend/datalib_step:datalib_step
bindir=$(mktemp -d) && ln -s "$PWD"/bazel-bin/datalib/backend/datalib_step/datalib_step "$bindir"/datalib-step
bazel-bin/datalib/backend/dag/datalib_dag_bin configs/dag_example.toml \
    --binary-dir "$bindir"
```

## Tiny run

The "tiny" config (a handful of sources, used by the manual e2e live-sync
golden test) now lives OUTSIDE this repo so its slightly sensitive source
data isn't shared when the repo is open-sourced. It's in the private
`data_liberation_manual_e2e_test_data` dir; point the runner above at
its config (which must be in the TOML steps format).
