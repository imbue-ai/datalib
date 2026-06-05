#!/usr/bin/env bash
# Launch the frankweiler dev UI pointed at a one-shot data root that
# only contains the Perseus tiny fixture — Thucydides 1.1, two
# sections, both languages. Useful for eyeballing the bilingual
# `edges` UI without populating a real on-disk data root.
#
# Unlike `:dev_tng`, the data root is built at runtime by invoking
# `frankweiler-sync` directly (translate-only — no GitHub fetch).
# That keeps the Bazel-side wiring trivial: we ship the sync binary
# + the in-crate `:tiny_fixture` filegroup as runfiles and let the
# binary do its normal thing.
#
# Invoke via `bazelisk run //frankweiler:dev_perseus`.

set -eo pipefail

f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null \
  || source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f 2- -d ' ')" 2>/dev/null \
  || source "$0.runfiles/$f" 2>/dev/null \
  || source "$0.runfiles/_main/$f" 2>/dev/null \
  || { echo>&2 "ERROR: cannot find bazel runfiles bootstrap"; exit 1; }
set -u

SYNC_BIN="$(rlocation _main/frankweiler/backend/sync/frankweiler_sync_bin)"
SERVE_SH="$(rlocation _main/frankweiler/serve_dev.sh)"
# `:tiny_fixture` ships the two TEI XMLs at this runfiles path.
# rlocation gives us one file's path; the dir is its parent.
GRC_XML="$(rlocation _main/frankweiler/backend/etl/providers/perseus/tests/fixtures/perseus_tiny/tlg0003.tlg001.perseus-grc2.xml)"
[[ -x "$SYNC_BIN" ]]  || { echo "ERROR: frankweiler_sync_bin not found at $SYNC_BIN" >&2; exit 1; }
[[ -x "$SERVE_SH" ]]  || { echo "ERROR: serve_dev.sh not found at $SERVE_SH" >&2; exit 1; }
[[ -f "$GRC_XML" ]]   || { echo "ERROR: perseus tiny fixture not found at $GRC_XML" >&2; exit 1; }
PERSEUS_FIXTURE_DIR="$(dirname "$GRC_XML")"

ROOT="$(mktemp -d -t frankweiler-perseus.XXXXXX)"
trap 'rm -rf "$ROOT"' EXIT INT TERM
echo "Perseus data root: $ROOT" >&2

# Translate-only config: no `sync:` block means `is_managed()` returns
# false, the extract pass is skipped for this source, and translate
# reads the TEI XMLs directly from `input_path`. We DO build the qmd
# index here even though we won't use the search bar — the backend
# hard-fails at startup if the index is missing (see
# `frankweiler/backend/http/src/main.rs`'s qmd_daemon block), so it's
# easier to pay the cost once than special-case it out.
cat > "$ROOT/config.yaml" <<EOF
data_root: $ROOT
sources:
  - name: perseus
    type: perseus
    input_path: $PERSEUS_FIXTURE_DIR
EOF

# Mirror the model-cache symlink that materialize_tng_root.sh sets up,
# so qmd-indexer can find the GGUF weights without re-downloading them
# on every launch. Path matches qmd's own default; the cache itself is
# populated externally (per README's first-time setup).
SHARED_MODELS="${HOME:-.}/.cache/qmd/models"
if [[ -d "$SHARED_MODELS" ]]; then
  mkdir -p "$ROOT/qmd"
  ln -sfn "$SHARED_MODELS" "$ROOT/qmd/models"
fi

# Deterministic `--now` so re-runs of this script don't bump the
# row's rendered_at and confuse the user when comparing across
# launches. Value matches `:dev_tng`'s style — far-future date so
# it sorts last in any timestamp comparison.
echo "[dev_perseus] running translate against $PERSEUS_FIXTURE_DIR" >&2
"$SYNC_BIN" --config "$ROOT/config.yaml" --now '2369-04-15T00:00:00+00:00'

exec "$SERVE_SH" "$ROOT"
