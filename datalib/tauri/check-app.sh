#!/usr/bin/env bash
#
# Prove a built Datalib.app can run the tools it bundles: spawn the
# bundled `datalib-step pull-runtime`, which resolves the runtime the
# way the shipped binaries do (the `runtime/` beside `binaries/`) and
# runs `qmd --version` and `latchkey --version` through it. Run after
# `tauri build`, on the .app it produced, because the bundler's copy of
# the staged tree is what stage_runtime.sh's own smoke test cannot
# see: it drops every symlink, and before the tree was staged
# `--no-symlinks` the only symptom was "Test connection" failing in
# the app.
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
