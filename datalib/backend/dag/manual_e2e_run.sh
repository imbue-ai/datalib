#!/usr/bin/env bash
#
# Convenience runner for the manual end-to-end live-sync golden bake
# (//datalib/backend/dag:manual_e2e_live_sync_golden).
#
#   ./manual_e2e_run.sh            # bake: run the pipeline, write snapshots/, read the diff
#   ./manual_e2e_run.sh --config   # validate the config only (offline, no creds)
#
# There is no compare mode, and the test itself refuses to run without
# INSTA_UPDATE — a comparison costs three pipeline passes against real
# APIs to report a diff that was always going to be accepted, because
# upstream moves whether or not our code does. Bake, then read the diff
# with `git diff` in $DATALIB_MANUAL_E2E_DIR.
#
# That is about the snapshots only. The test's assertions — every step
# succeeded, the data_root layout is what it should be, and a
# --reset-and-redownload lands byte-identical content — still fail the
# run, and they are the reason the exit code is worth looking at.
#
# This script lives in the code repo (it's code). The test's *data* — the
# dag.toml, the file-based sources/, and the golden snapshots/ — lives in a
# SEPARATE private repo (it's slightly sensitive, so it's never committed here),
# located at $DATALIB_MANUAL_E2E_DIR. That dir defaults to the canonical
# checkout below; export the var yourself to point at a different copy.
#
# Prereqs: latchkey creds configured for the API-backed sources. Several
# are a browser login rather than a paste: `latchkey auth browser
# google-gmail` for the `gmail` source (and `latchkey auth
# browser-prepare google-gmail` first, once, if it reports no OAuth
# client), and `latchkey auth browser chatgpt`, whose token expires
# often and is the usual reason a bake dies in its first minute. The
# rest are `latchkey auth set …`.
#
# Two failure modes worth telling apart, because the messages are similar
# and the fixes are not:
#   "No service matches URL: …"        → the SERVICE registration is gone.
#                                        Self-hosted registrations live in
#                                        ~/.latchkey/config.json, separate
#                                        from the encrypted credential
#                                        store, so one can vanish while the
#                                        other survives. Re-run the
#                                        `latchkey services register …` from
#                                        the provider's DOWNLOAD.md.
#   "Credentials for X are expired."   → the TOKEN needs reissuing.
#
# This script builds and exports LATCHKEY_CURL for you — see the block below
# for why that is not optional.
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
  echo "       Written before [[groups]]? Rewrite once: datalib-migrate-config \"$DATALIB_MANUAL_E2E_DIR/dag.toml\" -o \"$DATALIB_MANUAL_E2E_DIR/dag.toml\" --force" >&2
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
# immediately once LATCHKEY_CURL points at the dispatch curl.
#
# This is worth automating rather than documenting. The 403 reads exactly
# like throttling, which sent us chasing a non-existent rate limit for an
# afternoon (and, two months earlier, got the claude source disabled in
# the golden config for the same wrong reason).
if [[ -z "${LATCHKEY_CURL:-}" ]]; then
  # The dispatch, not the impersonator: only the dispatch acts on the
  # marker header, and the impersonator is a plain curl without the
  # flags the dispatch adds (docs/dev/curl_impersonate.md). Building
  # both puts them side by side, which is how the dispatch finds it.
  DISPATCH_TARGET="//datalib/backend/etl:latchkey_curl_dispatch"
  IMPERSONATE_TARGET="//datalib/backend/etl:latchkey_curl_impersonate"
  echo "[manual-e2e] building ${DISPATCH_TARGET} + ${IMPERSONATE_TARGET} for LATCHKEY_CURL…" >&2
  bazel build "$DISPATCH_TARGET" "$IMPERSONATE_TARGET" >&2
  # `bazel info bazel-bin` rather than the convenience symlink: the symlink
  # is absent on a fresh clone until something is built, and points at the
  # wrong config when the last build used different flags.
  LATCHKEY_CURL="$(bazel info bazel-bin)/datalib/backend/etl/latchkey_curl_dispatch"
  if [[ ! -x "$LATCHKEY_CURL" ]]; then
    echo "error: built the dispatch curl but it is not at $LATCHKEY_CURL" >&2
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
    exec bazel test //datalib/backend/datalib_step:datalib_step_unittests \
      --test_arg=--ignored \
      --test_arg=manual_e2e_config_loads_and_plans \
      --test_env=DATALIB_MANUAL_E2E_DIR \
      --test_output=all \
      --nocache_test_results
    ;;
  "" | --update)
    # `bazel run` forwards the client environment, so the exported vars reach
    # the test process. The wrapper sets INSTA_UPDATE=always, which is both
    # what makes the snapshots writable and what the test checks before it
    # fetches anything. The .snap files land straight in
    # $DATALIB_MANUAL_E2E_DIR/snapshots.
    #
    # `--update` is kept as a spelling of the default because it is in every
    # doc and every shell history that predates the compare mode going away.
    exec bazel run "${TARGET}.update"
    ;;
  *)
    echo "usage: $(basename "$0") [--config]" >&2
    echo "       no argument bakes the goldens; there is no compare mode." >&2
    exit 2
    ;;
esac
