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
# dag.toml, the file-based sources/, and the golden snapshots/ — lives in a
# SEPARATE private repo (it's slightly sensitive, so it's never committed here),
# located at $DATALIB_MANUAL_E2E_DIR. That dir defaults to the canonical
# checkout below; export the var yourself to point at a different copy.
#
# Prereqs: latchkey creds configured for the API-backed sources
# (`latchkey auth set …`). This script builds and exports LATCHKEY_CURL for
# you — see the block below for why that is not optional.
set -euo pipefail

# External private data dir (dag.toml + sources/ + snapshots/). Honor an
# existing export; else use the canonical checkout location.
#
# One spelling only. The pre-rename FRANKWEILER_MANUAL_E2E_DIR is deliberately
# NOT honored — accepting it would let a stale shell profile keep working with
# no indication it names something that no longer exists.
export DATALIB_MANUAL_E2E_DIR="${DATALIB_MANUAL_E2E_DIR:-$HOME/data_liberation_manual_e2e_test_data}"

if [[ -n "${FRANKWEILER_MANUAL_E2E_DIR:-}" ]]; then
  echo "note: FRANKWEILER_MANUAL_E2E_DIR is set and is IGNORED — the variable is" >&2
  echo "      now DATALIB_MANUAL_E2E_DIR. Update your shell profile." >&2
  echo "      using: $DATALIB_MANUAL_E2E_DIR" >&2
fi

if [[ ! -d "$DATALIB_MANUAL_E2E_DIR" ]]; then
  echo "error: DATALIB_MANUAL_E2E_DIR does not exist: $DATALIB_MANUAL_E2E_DIR" >&2
  echo "       clone the private test-data repo there, or export the var to point at it." >&2
  exit 1
fi
if [[ ! -f "$DATALIB_MANUAL_E2E_DIR/dag.toml" ]]; then
  echo "error: no dag.toml in $DATALIB_MANUAL_E2E_DIR" >&2
  echo "       that dir must hold the DAG-format config (dag.toml), sources/, and snapshots/." >&2
  echo "       Pre-TOML dir? Convert once: datalib-migrate-config \"$DATALIB_MANUAL_E2E_DIR/dag.yaml\" -o \"$DATALIB_MANUAL_E2E_DIR/dag.toml\"" >&2
  exit 1
fi

# Run from this script's package; bazel walks up to the workspace root, so the
# script works no matter the caller's cwd — and needs no hardcoded repo path.
cd "$(dirname "${BASH_SOURCE[0]}")"

TARGET="//datalib/backend/dag:manual_e2e_live_sync_golden"

# ── The Chrome-impersonating curl ───────────────────────────────────────
#
# claude.ai and chatgpt.com sit behind Cloudflare bot protection. Without a
# browser-shaped TLS/HTTP fingerprint they return HTTP 403 with
# `cf-mitigated: challenge` and a "Just a moment..." HTML body — no
# Retry-After, no x-ratelimit-* headers, because it is a challenge and not a
# rate limit. There is nothing to wait out: the same request returns 200
# immediately once LATCHKEY_CURL points at the impersonator.
#
# This is worth automating rather than documenting. The 403 reads exactly
# like throttling, which sent us chasing a non-existent rate limit for an
# afternoon (and, two months earlier, got the anthropic source disabled in
# the golden config for the same wrong reason).
if [[ -z "${LATCHKEY_CURL:-}" ]]; then
  IMPERSONATE_TARGET="//datalib/backend/etl:latchkey_curl_impersonate"
  echo "[manual-e2e] building ${IMPERSONATE_TARGET} for LATCHKEY_CURL…" >&2
  bazel build "$IMPERSONATE_TARGET" >&2
  # `bazel info bazel-bin` rather than the convenience symlink: the symlink
  # is absent on a fresh clone until something is built, and points at the
  # wrong config when the last build used different flags.
  LATCHKEY_CURL="$(bazel info bazel-bin)/datalib/backend/etl/latchkey_curl_impersonate"
  if [[ ! -x "$LATCHKEY_CURL" ]]; then
    echo "error: built the impersonator but it is not at $LATCHKEY_CURL" >&2
    exit 1
  fi
  export LATCHKEY_CURL
fi
echo "[manual-e2e] LATCHKEY_CURL=$LATCHKEY_CURL" >&2

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
      --test_env=LATCHKEY_CURL \
      --test_output=streamed \
      --nocache_test_results
    ;;
  *)
    echo "usage: $(basename "$0") [--update | --config]" >&2
    exit 2
    ;;
esac
