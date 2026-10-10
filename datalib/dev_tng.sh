#!/usr/bin/env bash
# Launch the datalib dev UI pointed at the checked-in TNG fixtures.
#
# Materializes a one-shot data root in a tmpdir via the shared
# `//tests/fixtures:materialize_tng_root` script — the same script the
# UI e2e test runs, so this command and the test stay byte-identical in
# what they materialize; only the models differ (all three here, the
# embedding model alone there), and this root adds the contacts app, so
# a chip can be linked to a contact. The tmpdir is removed on exit.
#
# Invoke via `bazelisk run //datalib:dev_tng`.

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
SERVE_SH="$(rlocation _main/datalib/serve_dev.sh)"
APPLET_BIN="$(rlocation _main/datalib/backend/applets/datalib_applet)"
[[ -x "$MATERIALIZE" ]] || { echo "ERROR: materialize_tng_root not found at $MATERIALIZE" >&2; exit 1; }
[[ -x "$SERVE_SH" ]]    || { echo "ERROR: serve_dev.sh not found at $SERVE_SH" >&2; exit 1; }
[[ -x "$APPLET_BIN" ]]  || { echo "ERROR: datalib_applet not found at $APPLET_BIN" >&2; exit 1; }

ROOT="$(mktemp -d -t datalib-tng.XXXXXX)"
trap 'rm -rf "$ROOT"' EXIT INT TERM
echo "TNG data root: $ROOT" >&2

"$MATERIALIZE" "$ROOT" >/dev/null

# The contacts app, as the e2e suite's `contacts` project adds it. Its
# store starts empty and goes with the tmpdir.
cat >> "$ROOT/config.toml" <<EOF

[[applets]]
id = "datalib_contacts"
command = "'$APPLET_BIN' datalib_contacts"
EOF

# Use serve_dev.sh (backend-only) rather than dev.sh (backend + vite).
# The packaged `datalib_http_bin` embeds the Vite-built SPA via
# rust-embed, so a single binary serves both UI and `/api/*` — no
# separate Vite dev server needed. serve_dev.sh opens the browser after
# the health probe when run from a terminal.
exec "$SERVE_SH" "$ROOT"
