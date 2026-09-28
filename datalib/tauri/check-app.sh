#!/usr/bin/env bash
#
# Prove a built Datalib.app can run the tools it bundles: spawn the
# bundled `datalib-step pull-runtime`, which resolves the runtime the
# way the shipped binaries do (the `runtime/` beside `binaries/`) and
# runs `qmd --version` and `latchkey --version` through it; then run
# the bundled `latchkey ensure-browser` the way the wizard's "Latchkey
# auth" button does, which is what loads playwright. Run after
# `tauri build`, on the .app it produced, because the bundler's copy of
# the staged tree is what stage_runtime.sh's own smoke test cannot
# see: it drops every symlink, and before the tree was staged
# `--no-symlinks` the only symptom was "Test connection" failing in
# the app. A tree with playwright pruned ran `--version` fine and
# failed every browser login.
#
# The login check needs a Chrome on the machine (GitHub's macOS runners
# have one); a mac without it cannot do a browser login either.
#
#   datalib/tauri/check-app.sh <path/to/Datalib.app>

set -euo pipefail

app="${1:?usage: $0 <Datalib.app>}"
resources="$app/Contents/Resources"
step="$resources/binaries/datalib-step"
[[ -x "$step" ]] || { echo "check-app: no datalib-step at $step" >&2; exit 1; }

# The env that would let the resolver find a runtime anywhere else.
unset DATALIB_RUNTIME_DIR DATALIB_ALLOW_NPX
report="$(cd / && "$step" pull-runtime 2>&1)" || {
    printf '%s\n' "$report" >&2
    echo "check-app: the bundled runtime does not run" >&2
    exit 1
}
printf '%s\n' "$report" >&2
case "$report" in
    *"runtime: $resources/runtime"*) ;;
    *)
        echo "check-app: the tools ran, but not from the .app's own runtime ($resources/runtime)" >&2
        exit 1 ;;
esac
echo ">>> check-app: $app runs its bundled qmd and latchkey" >&2

# The sources are `ensure_browser_args()` in datalib/backend/http/src/connect.rs.
# A scratch latchkey directory, so the person's own browser config is
# neither read nor written; no gateway, which refuses the command.
latchkey_dir="$(mktemp -d "${TMPDIR:-/tmp}/check-app-latchkey.XXXXXX")"
trap 'rm -rf "$latchkey_dir"' EXIT
report="$(cd / && env -u LATCHKEY_GATEWAY LATCHKEY_DIRECTORY="$latchkey_dir" \
    "$resources/binaries/latchkey" ensure-browser \
    --source existing-config,system-browser,existing-playwright-browser 2>&1)" || {
    printf '%s\n' "$report" >&2
    echo "check-app: the bundled latchkey cannot configure a browser, so every browser login would fail" >&2
    exit 1
}
printf '%s\n' "$report" >&2
echo ">>> check-app: $app's latchkey can run a browser login" >&2
