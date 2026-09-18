# Proto provenance

`backup.proto` and `local_archive.proto` describe the wire format of a
Signal-Android backup: message and field names, field numbers, types,
oneofs, enums and reserved ranges. They are **generated**, not vendored.

Signal-Android is AGPL-3.0, and this repository is MIT, so its
`.proto` sources cannot be copied here. What the format *is* — the
numbers and types a reader needs to decode the bytes — is
interoperability information, not the upstream's expression of it.
The files here carry exactly that and none of Signal's text: they are
printed from the descriptor set `protoc` compiles the upstream schema
into, which discards comments and layout, by
`tools/proto_from_descriptor.py`.

- Upstream: https://github.com/signalapp/Signal-Android,
  `lib/archive/src/main/protowire/{Backup,LocalArchive}.proto`
- Pinned commit: `de27343c245b8bc4b19684dcd46df2249532f5c2` (2026-05-19)
- Roots: `signal.backup.BackupInfo`, `signal.backup.Frame`,
  `signal.backup.local.Metadata`, `signal.backup.local.FilesFrame`.
  Everything upstream is reachable from those, so nothing is trimmed
  today; the roots are there so a future upstream addition we do not
  read stays out.

The ingest stores each frame's full JSON (`serde_json::to_string` of
the prost struct), so the schema has to stay complete under those
roots: a field missing here is a field the raw store silently loses.

## Refreshing

Pick a commit on `main` that is ≥7 days old (see MODULE.bazel header
for the version-pinning policy), update the SHA + date above, then:

```sh
SHA=<new sha>
work=$(mktemp -d)
for f in Backup.proto LocalArchive.proto; do
  curl -sSL "https://raw.githubusercontent.com/signalapp/Signal-Android/$SHA/lib/archive/src/main/protowire/$f" \
    -o "$work/$(echo $f | sed 's/Backup/backup/; s/LocalArchive/local_archive/')"
done
protoc -I "$work" --descriptor_set_out="$work/upstream.pb" "$work"/*.proto
python3 tools/proto_from_descriptor.py "$work/upstream.pb" datalib/backend/signal-backup/proto \
  signal.backup.BackupInfo signal.backup.Frame \
  signal.backup.local.Metadata signal.backup.local.FilesFrame
```

`protoc` is the one Bazel already fetched:
`$(bazelisk info output_base)/external/protobuf++protoc+prebuilt_protoc.<os_arch>/bin/protoc`.
The upstream download is an input to the generator and must not be
committed. Then `bazelisk test //datalib/backend/signal-backup/... //datalib/backend/etl/providers/signal/...`.
