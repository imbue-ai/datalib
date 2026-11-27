#!/usr/bin/env bash
# Lay down the TNG library: a data root built from the bazel-built TNG
# fixture, synced and ready to sync again.
#
# Single source of truth for the on-disk layout shared between:
#   * `bazelisk run //datalib:dev_tng`           (datalib/dev_tng.sh)
#   * `bazelisk test //datalib/ui:e2e_test`      (run_e2e.sh → playwright)
#
# Produces, under <out-root>:
#   <stanza>/render_markdown/...       each source's render store (from rendered.tar).
#   <stanza>/ingest/...                its raw stores, the blob store its pages
#                                      link attachments in among them
#                                      (from library.tar).
#   inputs/<stanza>/...                the files a file-reading source reads
#                                      (from library.tar).
#   unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite  the plain SQLite grid index the
#                                      backend reads, with the search index's
#                                      tables (`:ingested_tng_search_index`).
#   unified_grid_and_search_index/search_index/inference_cache.sqlite  every chunk's
#                                      vector (`:ingested_tng_inference_cache`).
#   unified_grid_and_search_index/models/     the embedding model, linked in from
#                                      bazel outputs.
#   <stanza>/render_time_series/...    each time-series source's store, and
#   unified_time_series_index/time_series_index/...  the index over them
#                                      (from time_series.tar).
#   config.toml                        { data_root }, every source, the
#                                      fan-ins, the embeds, and the two
#                                      applets: `unified_grid_and_search_index`,
#                                      which serves the grid, and
#                                      `unified_time_series_index`, which
#                                      serves the Time series card.
#
# The root has no `system/`, so the loop has no record of having run
# anything: the first sync runs every step, and each finds what it
# reads already done. A source that reads files reads its copy under
# `inputs/`; every other source keeps what its raw store holds
# (`common.download = false`), because what it downloads from is a
# recording only the fixture build can replay.
#
# Usage: materialize_tng_root.sh <out-root>
#
# Requires python3 on PATH. Nothing about the host's home directory: the
# embedding model arrives as a bazel input.

set -eo pipefail

f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null \
  || source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f 2- -d ' ')" 2>/dev/null \
  || source "$0.runfiles/$f" 2>/dev/null \
  || source "$0.runfiles/_main/$f" 2>/dev/null \
  || { echo>&2 "ERROR: cannot find bazel runfiles bootstrap"; exit 1; }
set -u

OUT_ROOT="${1:-}"
[[ -n "$OUT_ROOT" ]] || { echo "usage: $0 <out-root>" >&2; exit 2; }

APPLET_BIN="$(rlocation _main/datalib/backend/applets/datalib_applet)"
DB_FILE="$(rlocation _main/tests/fixtures/ingested/search_index.sqlite)"
VECTORS_FILE="$(rlocation _main/tests/fixtures/ingested/inference_cache.sqlite)"
RENDERED_TAR="$(rlocation _main/tests/fixtures/ingested/rendered.tar)"
TIME_SERIES_TAR="$(rlocation _main/tests/fixtures/ingested/time_series.tar)"
CONFIG_BODY="$(rlocation _main/tests/fixtures/ingested/config_body.toml)"
LIBRARY_TAR="$(rlocation _main/tests/fixtures/ingested/library.tar)"
[[ -x "$APPLET_BIN" ]]    || { echo "ERROR: datalib_applet not found at $APPLET_BIN" >&2; exit 1; }
[[ -f "$DB_FILE" ]]       || { echo "ERROR: search_index.sqlite not found at $DB_FILE" >&2; exit 1; }
[[ -f "$VECTORS_FILE" ]]  || { echo "ERROR: inference_cache.sqlite not found at $VECTORS_FILE" >&2; exit 1; }
[[ -f "$RENDERED_TAR" ]]  || { echo "ERROR: rendered.tar not found at $RENDERED_TAR" >&2; exit 1; }
[[ -f "$TIME_SERIES_TAR" ]] || { echo "ERROR: time_series.tar not found at $TIME_SERIES_TAR" >&2; exit 1; }
[[ -f "$CONFIG_BODY" ]]   || { echo "ERROR: config_body.toml not found at $CONFIG_BODY" >&2; exit 1; }
[[ -f "$LIBRARY_TAR" ]]   || { echo "ERROR: library.tar not found at $LIBRARY_TAR" >&2; exit 1; }

command -v python3 >/dev/null || { echo "ERROR: python3 not on PATH" >&2; exit 1; }

mkdir -p "$OUT_ROOT"

# The archive is rooted at `root/` (tar_rendered.py); strip that one
# component so the per-stanza markdown trees land at
# `<root>/<stanza>/render_markdown/...`.
tar -xf "$RENDERED_TAR" -C "$OUT_ROOT" --strip-components=1
# The index names each source's store by its path under the root and the
# commit it read, so the stores land where they were written.
tar -xf "$TIME_SERIES_TAR" -C "$OUT_ROOT" --strip-components=1
tar -xf "$LIBRARY_TAR" -C "$OUT_ROOT" --strip-components=1

# Drop the grid index, with the search index's tables, into the
# `unified_grid_and_search_index` step's tree, and the vectors beside it, where the backend
# opens them.
mkdir -p "$OUT_ROOT/unified_grid_and_search_index/unified_grid_and_search_index" "$OUT_ROOT/unified_grid_and_search_index/search_index"
cp "$DB_FILE" "$OUT_ROOT/unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite"
chmod u+w "$OUT_ROOT/unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite"
cp "$VECTORS_FILE" "$OUT_ROOT/unified_grid_and_search_index/search_index/inference_cache.sqlite"
chmod u+w "$OUT_ROOT/unified_grid_and_search_index/search_index/inference_cache.sqlite"

# The grid is served by the `unified_grid_and_search_index` applet, so the config has to
# declare it or the app comes up with no search. An absolute command
# rather than a bare name: this root is materialized into a temp dir with
# no `binary_dir` and nothing installed on PATH.
#
# Single-quoted, because a `command` is split shell-style and the path
# may contain spaces — a checkout under, say, `~/Imbue Dropbox/` yields
# a runfiles path that unquoted splits into `/Users/you/Imbue` and dies
# with "Permission denied". `bazel test` happens to stage runfiles under
# a space-free cache dir, so this only bites the `bazelisk run
# //datalib:dev_tng` path, which resolves through the source tree.
#
# The rest is the library's config, from the pipeline that built the
# stores (`config_body.toml`), with the place its inputs now are written
# in for the placeholder it leaves.
body="$(cat "$CONFIG_BODY")"
{
  echo "data_root = \"$OUT_ROOT\""
  echo
  printf '%s\n' "${body//@INPUTS@/$OUT_ROOT/inputs}"
  cat <<EOF

[[applets]]
group = "unified_grid_and_search_index"
id = "unified_grid_and_search_index"
command = "'$APPLET_BIN' unified_grid_and_search_index"

[[applets]]
group = "unified_time_series_index"
id = "unified_time_series_index"
command = "'$APPLET_BIN' unified_time_series_index"
EOF
} > "$OUT_ROOT/config.toml"

# The embedding model, where a search of this root looks for it
# (`datalib_embedding_models::models_dir_for`): every search by meaning embeds
# the query. A link INTO the runfiles tree rather than a copy in every
# root, the same assumption the applet `command` above already makes.
MODELS_DIR="$OUT_ROOT/unified_grid_and_search_index/models"
mkdir -p "$MODELS_DIR"
EMBED_MODEL="_main/third-party/embedding_models/hf_ggml-org_embeddinggemma-300M-Q8_0.gguf"
src="$(rlocation "$EMBED_MODEL")" || src=""
if [[ -z "$src" || ! -s "$src" ]]; then
  echo "ERROR: embedding model not found in runfiles: $EMBED_MODEL" >&2
  echo "  (is //third-party/embedding_models:embeddinggemma still in this target's \`data\`?)" >&2
  exit 3
fi
ln -sfn "$src" "$MODELS_DIR/$(basename "$EMBED_MODEL")"

# Drop the TNG-themed scan tree into the root as `file_inventory_scan/`. It's a plain
# directory the `file_inventory` (Unison-style) scanner can index; nothing renders it
# (file_inventory is extract-only), so it just sits in the root alongside the
# per-stanza markdown trees. Anchor off the checked-in `.file_inventory.toml`
# breadcrumb and copy its containing dir, dereferencing the runfiles symlinks
# (`cp -RL`) so the materialized tree is real files, like a user's directory.
FILE_INVENTORY_BREADCRUMB="$(rlocation _main/datalib/backend/etl/providers/file_inventory/tests/fixtures/file_inventory_tng/.file_inventory.toml)"
if [[ -f "$FILE_INVENTORY_BREADCRUMB" ]]; then
  cp -RL "$(dirname "$FILE_INVENTORY_BREADCRUMB")" "$OUT_ROOT/file_inventory_scan"
else
  echo "ERROR: file_inventory_tng fixture not found at $FILE_INVENTORY_BREADCRUMB" >&2
  exit 1
fi

echo "$OUT_ROOT"
