#!/usr/bin/env bash
#
# Convenience runner for the manual end-to-end live-sync golden test
# (//frankweiler/backend/sync:manual_e2e_live_sync_golden).
#
#   ./manual_e2e_run.sh            # run the sync + diff output against snapshots
#   ./manual_e2e_run.sh --update   # accept the new output into snapshots/
#
# This script lives in the code repo (it's code). The test's *data* — the
# config.yaml, the file-based sources/, and the golden snapshots/ — lives in a
# SEPARATE private repo (it's slightly sensitive, so it's never committed here),
# located at $FRANKWEILER_MANUAL_E2E_DIR. That dir defaults to the canonical
# checkout below; export the var yourself to point at a different copy.
#
# Prereqs: latchkey creds configured for the API-backed sources
# (`latchkey auth set …`). The Cloudflare-impersonating curl shim is
# auto-resolved from the sync binary's bazel runfiles; export LATCHKEY_CURL
# yourself only if you want to override it.
set -euo pipefail

# External private data dir (config.yaml + sources/ + snapshots/). Honor an
# existing export; else fall back to the canonical checkout location.
export FRANKWEILER_MANUAL_E2E_DIR="${FRANKWEILER_MANUAL_E2E_DIR:-$HOME/data_liberation_manual_e2e_test_data}"

if [[ ! -d "$FRANKWEILER_MANUAL_E2E_DIR" ]]; then
  echo "error: FRANKWEILER_MANUAL_E2E_DIR does not exist: $FRANKWEILER_MANUAL_E2E_DIR" >&2
  echo "       clone the private test-data repo there, or export the var to point at it." >&2
  exit 1
fi

# Run from this script's package; bazel walks up to the workspace root, so the
# script works no matter the caller's cwd — and needs no hardcoded repo path.
cd "$(dirname "${BASH_SOURCE[0]}")"

TARGET="//frankweiler/backend/sync:manual_e2e_live_sync_golden"

if [[ "${1:-}" == "--update" ]]; then
  # `bazel run` forwards the client environment, so the exported vars reach the
  # test process. The test writes .snap files straight into
  # $FRANKWEILER_MANUAL_E2E_DIR/snapshots.
  exec bazel run "${TARGET}.update"
else
  # `bazel test` scrubs the environment, so forward the vars we need by name.
  # --test_arg=--ignored because the test is #[ignore] in cargo; without it the
  # test binary runs zero tests and "passes" trivially.
  exec bazel test "$TARGET" \
    --test_arg=--ignored \
    --test_env=FRANKWEILER_MANUAL_E2E_DIR \
    ${LATCHKEY_CURL:+--test_env=LATCHKEY_CURL} \
    --test_output=streamed \
    --nocache_test_results
fi
