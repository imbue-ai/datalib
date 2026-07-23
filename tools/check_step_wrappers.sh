#!/usr/bin/env bash
# Asserts that the source-type list in
# frankweiler/backend/datalib_step/stage_wrappers.sh matches
# SOURCE_TYPES in frankweiler/backend/datalib_step/src/dispatch.rs.
#
# Why this exists: dispatch.rs is the canonical source of truth for
# what source types `datalib-step` handles, but the wrapper staging
# script is plain sh and can't read it — it carries its own copy of
# the list. Add a provider to dispatch.rs without updating
# stage_wrappers.sh and the new provider's `datalib-step-download-<ty>`
# wrapper would silently not exist; this test fails with a diff
# instead.

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

dispatch_rs="$(rlocation _main/frankweiler/backend/datalib_step/src/dispatch.rs)"
stage_sh="$(rlocation _main/frankweiler/backend/datalib_step/stage_wrappers.sh)"
[[ -f "$dispatch_rs" ]] || { echo "ERROR: dispatch.rs not found at $dispatch_rs" >&2; exit 1; }
[[ -f "$stage_sh" ]] || { echo "ERROR: stage_wrappers.sh not found at $stage_sh" >&2; exit 1; }

# The `"…",` string literals between `SOURCE_TYPES: &[&str] = &[` and
# the closing `];`.
rust_types="$(awk '/SOURCE_TYPES: &\[&str\] = &\[/{on=1;next} on&&/\];/{exit} on' "$dispatch_rs" \
    | sed -nE 's/^[[:space:]]*"([a-z0-9_]+)",$/\1/p' | sort)"
[[ -n "$rust_types" ]] || { echo "ERROR: no SOURCE_TYPES entries parsed from $dispatch_rs" >&2; exit 1; }

# The bare-word lines of the source_types="…" heredoc-style variable.
sh_types="$(awk '/^source_types="$/{on=1;next} on&&/^"$/{exit} on' "$stage_sh" \
    | sed -nE 's/^([a-z0-9_]+)$/\1/p' | sort)"
[[ -n "$sh_types" ]] || { echo "ERROR: no source_types entries parsed from $stage_sh" >&2; exit 1; }

if [[ "$rust_types" != "$sh_types" ]]; then
    cat >&2 <<EOF
Source-type list mismatch — dispatch.rs is the canonical source of truth.

  frankweiler/backend/datalib_step/src/dispatch.rs (SOURCE_TYPES)
  frankweiler/backend/datalib_step/stage_wrappers.sh (source_types)

Diff (dispatch.rs vs stage_wrappers.sh):
$(diff <(echo "$rust_types") <(echo "$sh_types") || true)

Update stage_wrappers.sh's source_types list to match.
EOF
    exit 1
fi
echo "OK: stage_wrappers.sh covers all $(echo "$rust_types" | wc -l | tr -d ' ') source types in dispatch.rs"
