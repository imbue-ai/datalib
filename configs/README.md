


## Tiny run

```sh
bazelisk run //frankweiler/backend/sync:frankweiler_sync_bin -- \
  --config "$(pwd)/configs/thad_tiny.yaml" \
  --now "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
```