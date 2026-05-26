#!/usr/bin/env bash
# Asserts that the version declared in frankweiler/backend/Cargo.toml's
# [workspace.package] section matches the `version = "..."` attribute on
# the `frankweiler_sync_bin` target in frankweiler/backend/sync/BUILD.bazel.
#
# Why this exists: Cargo.toml is the canonical source of truth for the
# project version (all member crates use `version.workspace = true`).
# rules_rust does NOT read Cargo.toml — it needs a `version` attr on
# `rust_binary` to populate CARGO_PKG_VERSION for the bazel-built binary,
# and that copy will rot unless something enforces parity. This test
# enforces it: bump Cargo.toml without bumping BUILD.bazel and
# `bazelisk test //...` fails with a clear diff.

# --- begin runfiles.bash initialization v3 ---
# Copy-pasted from the Bazel Bash runfiles library v3.
set -uo pipefail; set +e
f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null || \
  source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f2- -d' ')" 2>/dev/null || \
  source "$0.runfiles/$f" 2>/dev/null || \
  source "$(grep -sm1 "^$f " "$0.runfiles_manifest" | cut -f2- -d' ')" 2>/dev/null || \
  { echo>&2 "ERROR: cannot find $f"; exit 1; }; f=; set -e
# --- end runfiles.bash initialization v3 ---

cargo_toml="$(rlocation _main/frankweiler/backend/Cargo.toml)"
sync_build="$(rlocation _main/frankweiler/backend/sync/BUILD.bazel)"

[[ -f "$cargo_toml" ]] || { echo "ERROR: Cargo.toml not found at $cargo_toml" >&2; exit 1; }
[[ -f "$sync_build" ]] || { echo "ERROR: sync/BUILD.bazel not found at $sync_build" >&2; exit 1; }

# Pull the version literal from [workspace.package]. The match is
# anchored to lines that look like `version = "X.Y.Z"`; the workspace's
# member crates use `version.workspace = true` so there's only one such
# line in the file.
cargo_version="$(grep -E '^version = "[^"]+"$' "$cargo_toml" | head -n1 | sed -E 's/^version = "([^"]+)"$/\1/')"

# Pull the same version literal from the rust_binary attr.
bazel_version="$(grep -E '^[[:space:]]*version = "[^"]+",$' "$sync_build" | head -n1 | sed -E 's/^[[:space:]]*version = "([^"]+)",$/\1/')"

if [[ -z "$cargo_version" ]]; then
    echo "ERROR: could not find a workspace [workspace.package] version line in $cargo_toml" >&2
    exit 1
fi
if [[ -z "$bazel_version" ]]; then
    echo "ERROR: could not find a version = \"...\" line in $sync_build" >&2
    exit 1
fi

if [[ "$cargo_version" != "$bazel_version" ]]; then
    cat >&2 <<EOF
Version mismatch — Cargo.toml is the canonical source of truth.

  frankweiler/backend/Cargo.toml        [workspace.package].version = "$cargo_version"
  frankweiler/backend/sync/BUILD.bazel  rust_binary(version = "$bazel_version")

Bump both to the same value (typically: edit Cargo.toml first, then
the BUILD.bazel field, then re-tag).
EOF
    exit 1
fi

echo "OK: Cargo.toml and sync/BUILD.bazel both declare version $cargo_version"
