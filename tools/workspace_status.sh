#!/usr/bin/env bash
# Bazel --workspace_status_command for frankweiler.
#
# Bazel runs this once per build (when --stamp is in effect) and writes
# the `STABLE_*` lines into bazel-out/stable-status.txt, where stamp-aware
# rules can pick them up. The `STABLE_` prefix marks the value as part of
# the cacheable build state — changing it invalidates stamped outputs.
#
# Today the only key we emit is the git hash. We resolve it best-effort
# and fall back to the literal string "unknown" when the working tree
# isn't a git checkout (e.g. a release tarball), so consumers always see
# a non-empty value.
set -euo pipefail

if hash=$(git rev-parse HEAD 2>/dev/null); then
    echo "STABLE_GIT_HASH ${hash}"
else
    echo "STABLE_GIT_HASH unknown"
fi
