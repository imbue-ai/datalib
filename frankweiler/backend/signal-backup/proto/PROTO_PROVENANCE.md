# Proto provenance

These `.proto` files are vendored verbatim from Signal-Android.

- Upstream: https://github.com/signalapp/Signal-Android
- License: AGPL-3.0 (upstream); see `Backup.proto` header. Vendored
  here for build-time codegen only; not modified.
- Pinned commit: `de27343c245b8bc4b19684dcd46df2249532f5c2`
- Commit date: 2026-05-19
- Fetched on: 2026-06-08
- Files:
  - `Backup.proto`        — `lib/archive/src/main/protowire/Backup.proto`
                            (length-delimited frame stream inside the
                            gzipped `main` payload)
  - `LocalArchive.proto`  — `lib/archive/src/main/protowire/LocalArchive.proto`
                            (the `metadata` envelope + `files` sidecar
                            framing)

## Refreshing

Pick a commit on `main` that is ≥7 days old (see MODULE.bazel header
for the version-pinning policy), update the SHA + date above, and
re-fetch:

```
SHA=<new sha>
for f in Backup.proto LocalArchive.proto; do
  curl -sSL \
    "https://raw.githubusercontent.com/signalapp/Signal-Android/$SHA/lib/archive/src/main/protowire/$f" \
    -o "$f"
done
```
