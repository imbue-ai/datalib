#!/usr/bin/env bash
# Asserts that the version declared in frankweiler/backend/Cargo.toml's
# [workspace.package] section matches the `version = "..."` attribute in
# each BUILD.bazel that stamps one (sync's rust_binary, http's
# rust_library — the latter feeds /api/health and, via the bundled
# binary, the desktop app).
#
# Why this exists: Cargo.toml is the canonical source of truth for the
# project version (all member crates use `version.workspace = true`).
# rules_rust does NOT read Cargo.toml — it needs a `version` attr to
# populate CARGO_PKG_VERSION for the bazel-built crate, and those
# copies will rot unless something enforces parity. This test enforces
# it: bump Cargo.toml without bumping a BUILD.bazel and
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
[[ -f "$cargo_toml" ]] || { echo "ERROR: Cargo.toml not found at $cargo_toml" >&2; exit 1; }

# Pull the version literal from [workspace.package]. The match is
# anchored to lines that look like `version = "X.Y.Z"`; the workspace's
# member crates use `version.workspace = true` so there's only one such
# line in the file.
cargo_version="$(grep -E '^version = "[^"]+"$' "$cargo_toml" | head -n1 | sed -E 's/^version = "([^"]+)"$/\1/')"
if [[ -z "$cargo_version" ]]; then
    echo "ERROR: could not find a workspace [workspace.package] version line in $cargo_toml" >&2
    exit 1
fi

status=0
for pkg in sync http; do
    build_file="$(rlocation "_main/frankweiler/backend/$pkg/BUILD.bazel")"
    [[ -f "$build_file" ]] || { echo "ERROR: $pkg/BUILD.bazel not found at $build_file" >&2; exit 1; }

    # Pull the version literal from the rust_binary/rust_library attr.
    bazel_version="$(grep -E '^[[:space:]]*version = "[^"]+",$' "$build_file" | head -n1 | sed -E 's/^[[:space:]]*version = "([^"]+)",$/\1/')"
    if [[ -z "$bazel_version" ]]; then
        echo "ERROR: could not find a version = \"...\" line in $build_file" >&2
        exit 1
    fi

    if [[ "$cargo_version" != "$bazel_version" ]]; then
        cat >&2 <<EOF
Version mismatch — Cargo.toml is the canonical source of truth.

  frankweiler/backend/Cargo.toml             [workspace.package].version = "$cargo_version"
  frankweiler/backend/$pkg/BUILD.bazel       version = "$bazel_version"

Bump both to the same value (typically: edit Cargo.toml first, then
the BUILD.bazel fields, then re-tag).
EOF
        status=1
    fi
done
exit_msg="OK: Cargo.toml, sync/BUILD.bazel, and http/BUILD.bazel all declare version $cargo_version"
[[ "$status" -eq 0 ]] && echo "$exit_msg"
exit "$status"
