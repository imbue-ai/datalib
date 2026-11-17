#!/usr/bin/env bash
# The SessionStart hook `.claude/settings.json` runs. In a Claude Code
# cloud session whose environment carries BUILDBUDDY_API_KEY, it writes
# the `.bazelrc.user` CI's prepare-bazel action writes, so bazel reads
# and writes the contributor cache. Anywhere else, or when a
# `.bazelrc.user` is already there, it does nothing.
set -euo pipefail

[[ "${CLAUDE_CODE_REMOTE:-}" == "true" ]] || exit 0
[[ -n "${BUILDBUDDY_API_KEY:-}" ]] || exit 0

rc="${CLAUDE_PROJECT_DIR:-$PWD}/.bazelrc.user"
[[ -e "$rc" ]] && exit 0

umask 077
cat >"$rc" <<EOF
common --remote_header=x-buildbuddy-api-key=${BUILDBUDDY_API_KEY}
build --config=buildbuddy
# The VM is thrown away and BuildBuddy holds the shared cache, so a local
# disk cache would only spend the VM's small disk a second time. Both
# lines, because .bazelrc sets it for each.
build --disk_cache=
test --disk_cache=
EOF
echo "bazel: BuildBuddy cache on (wrote .bazelrc.user from BUILDBUDDY_API_KEY)"
