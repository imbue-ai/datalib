#!/usr/bin/env bash
# Run the sign-in suite (tests/e2e_auth/, playwright.auth.config.ts):
# the wizard against a real datalib-http and the real, Bazel-pinned
# latchkey, with the third-party sites faked. tests/e2e_auth/README.md
# says what is real and what is not.
#
#   bazel run //datalib/ui:e2e_auth [-- <playwright args>]   source tree, edit and re-run
#   bazel test //datalib/ui:e2e_auth_test                     runfiles, as CI runs it
set -eo pipefail

# --- bazel runfiles bootstrap ---
f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null \
  || source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f 2- -d ' ')" 2>/dev/null \
  || source "$0.runfiles/$f" 2>/dev/null \
  || source "$0.runfiles/_main/$f" 2>/dev/null \
  || { echo>&2 "ERROR: cannot find bazel runfiles bootstrap"; exit 1; }
set -u

need_runfile() { # rlocationpath, test flag
  local out=""
  out="$(rlocation "$1")" || out=""
  if [[ -z "$out" ]] || ! test "${2:--e}" "$out"; then
    echo "ERROR: cannot locate '$1' in runfiles; did it drop out of :e2e_auth's data?" >&2
    exit 1
  fi
  printf '%s\n' "$out"
}

RUN_DIR="$(mktemp -d "${TEST_TMPDIR:-${TMPDIR:-/tmp}}/datalib-e2e-auth.XXXXXX")"
if [[ -z "${DATALIB_TEST_AUTH_KEEP:-}" ]]; then
  trap 'chmod -R u+w "$RUN_DIR" 2>/dev/null; rm -rf "$RUN_DIR"' EXIT
else
  echo "keeping $RUN_DIR"
fi
export DATALIB_TEST_AUTH_SCRATCH="$RUN_DIR/worlds"

if [[ -n "${BUILD_WORKSPACE_DIRECTORY:-}" ]]; then
  UI_DIR="$BUILD_WORKSPACE_DIRECTORY/datalib/ui"
  source "$UI_DIR/../../scripts/ensure_pnpm.sh"
  (cd "$UI_DIR" && pnpm install --frozen-lockfile)
  (cd "$UI_DIR" && pnpm exec playwright install chromium >/dev/null)
  PLAYWRIGHT_CMD=(pnpm exec playwright test)
else
  # Playwright skips symlinked spec files, so the runfiles copy is
  # materialized first; see `e2e_inputs` in BUILD.bazel.
  UI_RUNFILES="$(dirname "$(need_runfile _main/datalib/ui/package.json -f)")"
  UI_DIR="$RUN_DIR/stage"
  mkdir -p "$UI_DIR"
  rsync -aL --exclude node_modules --exclude 'e2e*_test' --exclude 'run_e2e*.sh' \
    "$UI_RUNFILES/" "$UI_DIR/"
  ln -s "$UI_RUNFILES/node_modules" "$UI_DIR/node_modules"
  node "$UI_DIR/node_modules/@playwright/test/cli.js" install chromium >/dev/null
  PLAYWRIGHT_CMD=(node "$UI_DIR/node_modules/@playwright/test/cli.js" test)
fi

DATALIB_HTTP_BIN="$(need_runfile "$DATALIB_TEST_AUTH_HTTP_BIN_RLOC" -x)"
DATALIB_STEP_BIN="$(need_runfile "$DATALIB_TEST_AUTH_STEP_BIN_RLOC" -x)"
export DATALIB_HTTP_BIN DATALIB_STEP_BIN
DATALIB_TEST_AUTH_CURL_ROUTER="$(need_runfile "$DATALIB_TEST_AUTH_ROUTER_RLOC" -x)"
export DATALIB_TEST_AUTH_CURL_ROUTER
DATALIB_TEST_AUTH_GARMIN_PLUGIN="$(dirname "$(need_runfile "$DATALIB_TEST_AUTH_GARMIN_PLUGIN_RLOC" -f)")"
export DATALIB_TEST_AUTH_GARMIN_PLUGIN

# The runtime the release ships — Node, qmd, latchkey — staged the way
# the dev launchers stage it, then with its `node` swapped for the
# stand-in in tests/e2e_auth/fake_node.mjs.
unset DATALIB_RUNTIME_DIR
source "$(need_runfile _main/datalib/dev_runtime.sh -f)"
REAL_NODE="$(readlink "$DATALIB_RUNTIME_DIR/node/bin/node")"
LATCHKEY_CLI="$(ls "$DATALIB_RUNTIME_DIR"/latchkey/*/node_modules/latchkey/dist/src/cli.js)"
FAKE_NODE_JS="$UI_DIR/tests/e2e_auth/fake_node.mjs"
rm "$DATALIB_RUNTIME_DIR/node/bin/node"
cat > "$DATALIB_RUNTIME_DIR/node/bin/node" <<EOF
#!/bin/sh
case "\$1" in
  */latchkey/dist/src/cli.js) ;;
  *) exec '$REAL_NODE' "\$@" ;;
esac
'$REAL_NODE' '$FAKE_NODE_JS' "\$@"
rc=\$?
[ "\$rc" -eq 0 ] && exec '$REAL_NODE' "\$@"
[ "\$rc" -eq 100 ] && exit 0
exit "\$rc"
EOF
chmod +x "$DATALIB_RUNTIME_DIR/node/bin/node"
export DATALIB_TEST_AUTH_RUNTIME_DIR="$DATALIB_RUNTIME_DIR"
export DATALIB_TEST_AUTH_REAL_NODE="$REAL_NODE"
export DATALIB_TEST_AUTH_LATCHKEY_CLI="$LATCHKEY_CLI"
unset DATALIB_RUNTIME_DIR

# The report and traces outlive the run: bazel keeps a test's undeclared
# outputs, and `bazel run` leaves them in the source tree's test-results.
export DATALIB_TEST_AUTH_ARTIFACTS="${TEST_UNDECLARED_OUTPUTS_DIR:-$UI_DIR/test-results/e2e_auth}"

cd "$UI_DIR"
"${PLAYWRIGHT_CMD[@]}" --config playwright.auth.config.ts "$@"
