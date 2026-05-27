#!/usr/bin/env bash
# Launch the frankweiler dev UI pointed at the checked-in TNG fixtures.
#
# Materializes a one-shot data root in a tmpdir via the shared
# `//tests/fixtures:materialize_tng_root` script — the same one used by
# the UI e2e test, so this command and the test stay byte-identical in
# what they materialize. The tmpdir is removed on exit.
#
# Invoke via `bazelisk run //frankweiler:dev_tng`.

set -eo pipefail

f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null \
  || source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f 2- -d ' ')" 2>/dev/null \
  || source "$0.runfiles/$f" 2>/dev/null \
  || source "$0.runfiles/_main/$f" 2>/dev/null \
  || { echo>&2 "ERROR: cannot find bazel runfiles bootstrap"; exit 1; }
set -u

MATERIALIZE="$(rlocation _main/tests/fixtures/materialize_tng_root)"
SERVE_SH="$(rlocation _main/frankweiler/serve_dev.sh)"
[[ -x "$MATERIALIZE" ]] || { echo "ERROR: materialize_tng_root not found at $MATERIALIZE" >&2; exit 1; }
[[ -x "$SERVE_SH" ]]    || { echo "ERROR: serve_dev.sh not found at $SERVE_SH" >&2; exit 1; }

ROOT="$(mktemp -d -t frankweiler-tng.XXXXXX)"
trap 'rm -rf "$ROOT"' EXIT INT TERM
echo "TNG data root: $ROOT" >&2

"$MATERIALIZE" "$ROOT" >/dev/null

# Use serve_dev.sh (backend-only) rather than dev.sh (backend + vite).
# The packaged `frankweiler_http_bin` embeds the Vite-built SPA via
# rust-embed, so a single binary serves both UI and `/api/*` — no
# separate Vite dev server needed. The binary auto-opens the browser;
# serve_dev.sh also opens its own URL after the health probe.
exec "$SERVE_SH" "$ROOT"
