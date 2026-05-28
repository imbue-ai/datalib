


## Tiny run

```sh
bazelisk build //frankweiler/backend/sync:frankweiler_sync_bin &&
./bazel-bin/frankweiler/backend/sync/frankweiler_sync_bin \
    --config "$(pwd)/configs/thad_tiny.yaml"
```

## Bigger dev run

```sh
bazelisk build //frankweiler/backend/sync:frankweiler_sync_bin &&
./bazel-bin/frankweiler/backend/sync/frankweiler_sync_bin \
    --config "$(pwd)/configs/thad_dev.yaml"
```