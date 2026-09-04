#!/usr/bin/env bash
# Asserts that the vendored qmd snapshot under `third-party/qmd/`
# resolves its model cache to `$XDG_CACHE_HOME/qmd/models` (with a
# `$HOME/.cache/qmd/models` fallback) — i.e. the same path
# `datalib_qmd_indexer::default_models_dir()` returns.
#
# This is about the SHIPPED app, not the build: a sync run by the app
# downloads models to that path, and the indexer has to look for them
# where qmd put them. The build itself stopped reading the host cache
# when the models became bazel inputs (`@qmd_model_*` in MODULE.bazel).
#
# Why this exists: the vendored `third-party/qmd/` snapshot is kept in
# sync with `DEFAULT_QMD_VERSION` (parity checked below) so this test
# catches the case where upstream qmd changes its cache-path constant —
# we'd otherwise notice only when a user's app quietly re-downloaded
# multiple GB into a second directory. Pairs with the rust-side
# `default_models_dir_matches_qmd_default` unit test in
# //datalib/backend/qmd_indexer:qmd_indexer_unittests.
#
# The matched line in third-party/qmd/src/llm.ts:
#   const MODEL_CACHE_DIR = process.env.XDG_CACHE_HOME
#     ? join(process.env.XDG_CACHE_HOME, "qmd", "models")
#     : join(homedir(), ".cache", "qmd", "models");

# --- begin runfiles.bash initialization v3 ---
set -uo pipefail; set +e
f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null || \
  source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f2- -d' ')" 2>/dev/null || \
  source "$0.runfiles/$f" 2>/dev/null || \
  source "$(grep -sm1 "^$f " "$0.runfiles_manifest" | cut -f2- -d' ')" 2>/dev/null || \
  { echo>&2 "ERROR: cannot find $f"; exit 1; }; f=; set -e
# --- end runfiles.bash initialization v3 ---

llm_ts="$(rlocation _main/third-party/qmd/src/llm.ts)"
pkg_json="$(rlocation _main/third-party/qmd/package.json)"
core_qmd_mod="$(rlocation _main/datalib/backend/unified_index/src/qmd/mod.rs)"

for f in "$llm_ts" "$pkg_json" "$core_qmd_mod"; do
    [[ -f "$f" ]] || { echo "ERROR: required input not found at $f" >&2; exit 1; }
done

# ── 1. Vendored qmd uses the expected MODEL_CACHE_DIR construction ──
#
# Match both arms of the ternary so a refactor that drops only the
# XDG_CACHE_HOME branch (e.g. introduces a config-file override) still
# fails loudly here. The regex tolerates spacing variations but pins
# the literal path components.
xdg_pattern='process\.env\.XDG_CACHE_HOME[^"]*"qmd"[[:space:]]*,[[:space:]]*"models"'
home_pattern='homedir\(\)[^"]*"\.cache"[[:space:]]*,[[:space:]]*"qmd"[[:space:]]*,[[:space:]]*"models"'

if ! grep -qE -- "$xdg_pattern" "$llm_ts"; then
    cat >&2 <<EOF
qmd model cache path drift detected.

  third-party/qmd/src/llm.ts no longer joins
  XDG_CACHE_HOME with ("qmd", "models").

If upstream qmd intentionally moved the cache, update:
  * datalib/backend/qmd_indexer/src/lib.rs (default_models_dir)
  * datalib/dev_perseus.sh                   (SHARED_MODELS)
  * README.md / docs/user/first_time_user.md
…then re-run this test.
EOF
    exit 1
fi
if ! grep -qE -- "$home_pattern" "$llm_ts"; then
    echo "ERROR: third-party/qmd/src/llm.ts no longer joins homedir() with (.cache, qmd, models). See above." >&2
    exit 1
fi

# ── 2. Vendored qmd version matches DEFAULT_QMD_VERSION ─────────────
#
# Without this, the grep above tests a snapshot that isn't what
# `npx -y @tobilu/qmd@<DEFAULT_QMD_VERSION>` actually fetches.
pkg_version="$(grep -E '"version"[[:space:]]*:[[:space:]]*"[^"]+"' "$pkg_json" | head -n1 | sed -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')"
pinned_version="$(grep -E '^pub const DEFAULT_QMD_VERSION:' "$core_qmd_mod" | sed -E 's/.*"([^"]+)".*/\1/')"

if [[ -z "$pkg_version" || -z "$pinned_version" ]]; then
    echo "ERROR: failed to extract qmd version (pkg=$pkg_version, pinned=$pinned_version)" >&2
    exit 1
fi
if [[ "$pkg_version" != "$pinned_version" ]]; then
    cat >&2 <<EOF
Vendored qmd snapshot is out of sync with DEFAULT_QMD_VERSION.

  third-party/qmd/package.json               version = "$pkg_version"
  unified_index/src/qmd/mod.rs    DEFAULT_QMD_VERSION = "$pinned_version"

Either update DEFAULT_QMD_VERSION to "$pkg_version" or re-vendor
third-party/qmd/ at $pinned_version, then re-run this test. The
upstream cache-path check above is only meaningful when these match.
EOF
    exit 1
fi

echo "OK: vendored qmd $pkg_version, model cache at \$XDG_CACHE_HOME/qmd/models (fallback \$HOME/.cache/qmd/models)."
