#!/usr/bin/env bash
# Bazel --workspace_status_command for frankweiler.
#
# Bazel runs this once per build (when --stamp is in effect) and writes
# the `STABLE_*` lines into bazel-out/stable-status.txt, where stamp-aware
# rules can pick them up. The `STABLE_` prefix marks the value as part of
# the cacheable build state — changing it invalidates stamped outputs.
#
# Emits STABLE_GIT_HASH (full SHA) and STABLE_GIT_DESCRIBE (the result
# of `git describe --tags --always --dirty`). Both fall back to the
# literal string "unknown" outside a git checkout (e.g. a release
# tarball), so consumers always see a non-empty value.
set -euo pipefail

if hash=$(git rev-parse HEAD 2>/dev/null); then
    echo "STABLE_GIT_HASH ${hash}"
else
    echo "STABLE_GIT_HASH unknown"
fi

# `git describe --tags --always --dirty` resolves to:
#   - "v0.1.2"               for the commit a tag points at
#   - "v0.1.2-3-gabc123d"    for a commit 3 commits past v0.1.2
#   - "abc123d"              when no reachable tag exists (still
#                            unambiguous via --always)
#   - any of the above + "-dirty" when the working tree has uncommitted
#                                 changes
# Consumed by frankweiler-sync's --version stamp. Falls back to
# "unknown" outside a git checkout for parity with STABLE_GIT_HASH.
if describe=$(git describe --tags --always --dirty 2>/dev/null); then
    echo "STABLE_GIT_DESCRIBE ${describe}"
else
    echo "STABLE_GIT_DESCRIBE unknown"
fi
