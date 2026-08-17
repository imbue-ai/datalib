#!/usr/bin/env bash
#
# Convenience runner for the manual end-to-end live-sync golden test
# (//datalib/backend/dag:manual_e2e_live_sync_golden).
#
#   ./manual_e2e_run.sh            # run the pipeline + diff output against snapshots
#   ./manual_e2e_run.sh --update   # accept the new output into snapshots/
#   ./manual_e2e_run.sh --config   # validate the config only (offline, no creds)
#
# This script lives in the code repo (it's code). The test's *data* — the
# dag.yaml, the file-based sources/, and the golden snapshots/ — lives in a
# SEPARATE private repo (it's slightly sensitive, so it's never committed here),
# located at $DATALIB_MANUAL_E2E_DIR. That dir defaults to the canonical
# checkout below; export the var yourself to point at a different copy.
#
# Prereqs: latchkey creds configured for the API-backed sources
# (`latchkey auth set …`). The Cloudflare-impersonating curl shim is
# auto-resolved from the step binary's bazel runfiles; export LATCHKEY_CURL
# yourself only if you want to override it.
set -euo pipefail

# External private data dir (dag.yaml + sources/ + snapshots/). Honor an
# existing export; else fall back to the canonical checkout location.
# FRANKWEILER_MANUAL_E2E_DIR is the pre-rename name, still honored so an old
# shell profile keeps working.
export DATALIB_MANUAL_E2E_DIR="${DATALIB_MANUAL_E2E_DIR:-${FRANKWEILER_MANUAL_E2E_DIR:-$HOME/data_liberation_manual_e2e_test_data}}"

if [[ ! -d "$DATALIB_MANUAL_E2E_DIR" ]]; then
  echo "error: DATALIB_MANUAL_E2E_DIR does not exist: $DATALIB_MANUAL_E2E_DIR" >&2
  echo "       clone the private test-data repo there, or export the var to point at it." >&2
  exit 1
fi
if [[ ! -f "$DATALIB_MANUAL_E2E_DIR/dag.yaml" ]]; then
  echo "error: no dag.yaml in $DATALIB_MANUAL_E2E_DIR" >&2
  echo "       that dir must hold the DAG-format config (dag.yaml), sources/, and snapshots/." >&2
  exit 1
fi

# Run from this script's package; bazel walks up to the workspace root, so the
# script works no matter the caller's cwd — and needs no hardcoded repo path.
cd "$(dirname "${BASH_SOURCE[0]}")"

TARGET="//datalib/backend/dag:manual_e2e_live_sync_golden"

case "${1:-}" in
  --config)
    # Offline pre-flight: parse the config, build the graph, and round-trip
    # every step's params against the provider schemas. No network, no creds,
    # seconds not minutes. Worth running before any live invocation.
    #
    # Caveat: this is not a complete guard. Render params are
    # deny_unknown_fields and so are most download configs, but `email`,
    # `fsindex`, `linkedin` and `sms_backup_restore` are permissive — a
    # misplaced knob on those parses clean here and fails during the live run.
    exec bazel test //datalib/backend/ingest_config:config_examples_test \
      --test_arg=--ignored \
      --test_env=DATALIB_MANUAL_E2E_DIR \
      --test_output=all \
      --nocache_test_results
    ;;
  --update)
    # `bazel run` forwards the client environment, so the exported vars reach
    # the test process. The test writes .snap files straight into
    # $DATALIB_MANUAL_E2E_DIR/snapshots.
    exec bazel run "${TARGET}.update"
    ;;
  "")
    # `bazel test` scrubs the environment, so forward the vars we need by name.
    # (HOME/PATH/USER come through the target's `env_inherit`.)
    # --test_arg=--ignored because the test is #[ignore] in cargo; without it
    # the test binary runs zero tests and "passes" trivially.
    exec bazel test "$TARGET" \
      --test_arg=--ignored \
      --test_env=DATALIB_MANUAL_E2E_DIR \
      ${LATCHKEY_CURL:+--test_env=LATCHKEY_CURL} \
      --test_output=streamed \
      --nocache_test_results
    ;;
  *)
    echo "usage: $(basename "$0") [--update | --config]" >&2
    exit 2
    ;;
esac
