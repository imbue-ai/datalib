#!/usr/bin/env bash
# Runs a fixture genrule's command with its output in a log file, and
# prints the tail of that log only if the command fails. Bazel echoes
# everything an action prints, so a green build would otherwise put the
# whole sync event stream in every CI log.
#
# Usage: quiet.sh <log file> <command> [args...]
set -uo pipefail

log=$1
shift
"$@" >"$log" 2>&1
status=$?
if [ "$status" -ne 0 ]; then
  script=${2-}
  echo "${1##*/} ${script##*/} failed (exit $status); the last 200 lines of its output:" >&2
  tail -n 200 "$log" >&2
fi
exit "$status"
