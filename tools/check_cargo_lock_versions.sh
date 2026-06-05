#!/usr/bin/env bash
# Asserts that every workspace-local crate in
# `frankweiler/backend/Cargo.lock` is pinned to the same version
# declared by `[workspace.package].version` in
# `frankweiler/backend/Cargo.toml`.
#
# Why this exists: rules_rust's `crate.from_cargo` reads BOTH
# Cargo.toml manifests AND Cargo.lock, but only validates the
# external-dep section against the lockfile at build time. The
# workspace-crate version markers in Cargo.lock can silently drift
# from Cargo.toml without rules_rust complaining, and the release
# tarballs ship just fine because rustc reads `version =` from
# Cargo.toml — but a fresh checkout where someone runs `cargo
# build` would see the wrong version on the workspace crates, and a
# `bazel sync --only=frankweiler_crates` could re-resolve in
# surprising ways.
#
# Common cause of the drift: bumping `[workspace.package].version`
# in Cargo.toml without then running a real Cargo.lock refresh.
# `CARGO_BAZEL_REPIN=1 bazel test //...` only repins if some test
# action actually re-runs — when targets cache-hit, the lockfile
# stays where it was. Use `tools/repin_cargo.sh` (referenced in
# the failure message below) to force the refresh.

# --- begin runfiles.bash initialization v3 ---
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
cargo_lock="$(rlocation _main/frankweiler/backend/Cargo.lock)"

[[ -f "$cargo_toml" ]] || { echo "ERROR: Cargo.toml not found at $cargo_toml" >&2; exit 1; }
[[ -f "$cargo_lock" ]] || { echo "ERROR: Cargo.lock not found at $cargo_lock" >&2; exit 1; }

canonical="$(grep -E '^version = "[^"]+"$' "$cargo_toml" | head -n1 | sed -E 's/^version = "([^"]+)"$/\1/')"
if [[ -z "$canonical" ]]; then
    echo "ERROR: could not find a [workspace.package].version line in $cargo_toml" >&2
    exit 1
fi

# Pull every workspace-local crate's recorded version from Cargo.lock.
# Workspace crates are the ones whose `name = "frankweiler-*"`. Cargo
# writes `[[package]]` blocks with `name` and `version` on adjacent
# lines, so an awk pass tracks the most recent `name` and emits the
# `version` when name matches our prefix.
mismatches=()
while IFS=$'\t' read -r crate version; do
    if [[ "$version" != "$canonical" ]]; then
        mismatches+=("${crate}=${version}")
    fi
done < <(awk '
    /^name = "frankweiler-/ {
        match($0, /"[^"]+"/);
        last_name = substr($0, RSTART + 1, RLENGTH - 2);
        next;
    }
    /^version = "/ && last_name != "" {
        match($0, /"[^"]+"/);
        version = substr($0, RSTART + 1, RLENGTH - 2);
        printf "%s\t%s\n", last_name, version;
        last_name = "";
    }
' "$cargo_lock")

if [[ ${#mismatches[@]} -ne 0 ]]; then
    cat >&2 <<EOF
Cargo.lock has workspace-local crates pinned to versions that disagree
with [workspace.package].version in Cargo.toml.

  Canonical (frankweiler/backend/Cargo.toml): ${canonical}
  Mismatched entries in frankweiler/backend/Cargo.lock:
EOF
    for m in "${mismatches[@]}"; do
        printf '    %s\n' "$m" >&2
    done
    cat >&2 <<'EOF'

Fix: run
    tools/repin_cargo.sh
which forces a real Cargo.lock refresh against the current Cargo.toml.
A plain `CARGO_BAZEL_REPIN=1 bazel test //...` is NOT enough on its
own — when test targets cache-hit, no action actually re-runs the
repin extension and the lockfile stays where it was.
EOF
    exit 1
fi

echo "OK: ${canonical} matches every workspace crate in Cargo.lock"
