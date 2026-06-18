


## Tiny run

The "tiny" config (a handful of sources, used by the manual e2e live-sync
golden test) now lives OUTSIDE this repo so its slightly sensitive source
data isn't shared when the repo is open-sourced. It's in the private
`data_liberation_manual_e2e_test_data` dir; run it via the `run.sh` there,
or point `--config` at its `config.yaml`:

```sh
bazelisk build //frankweiler/backend/sync:frankweiler_sync_bin &&
./bazel-bin/frankweiler/backend/sync/frankweiler_sync_bin \
    --config ~/data_liberation_manual_e2e_test_data/config.yaml
```

## Bigger dev run

```sh
bazelisk build //frankweiler/backend/sync:frankweiler_sync_bin &&
./bazel-bin/frankweiler/backend/sync/frankweiler_sync_bin \
    --config "$(pwd)/configs/thad_dev.yaml"
```