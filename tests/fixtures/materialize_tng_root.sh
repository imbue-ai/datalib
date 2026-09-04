#!/usr/bin/env bash
# Materialize a datalib data root from the bazel-built TNG fixture.
#
# Single source of truth for the on-disk layout shared between:
#   * `bazelisk run //datalib:dev_tng`           (datalib/dev_tng.sh)
#   * `bazelisk test //datalib/ui:e2e_test`      (run_e2e.sh → playwright)
#
# Produces, under <out-root>:
#   <stanza>/rendered_md/...           Conversation markdown trees (from qmd.tar).
#   unified_index/grid/db.doltlite_db  doltlite (SQLite-compatible) file the backend reads.
#   unified_index/qmd/index.sqlite            QMD index (from qmd-index.tar).
#   unified_index/qmd/models -> ~/.cache/qmd/models  (shared, populated externally)
#   config.toml                        { data_root } plus the
#                                      `unified_index` applet the grid
#                                      is served by.
#
# Usage: materialize_tng_root.sh <out-root>
#
# Requires python3 on PATH. The qmd model cache at ~/.cache/qmd/models
# must already contain the required GGUF files — this script refuses to
# trigger a download (silent multi-minute stall).

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
DB_FILE="$(rlocation _main/tests/fixtures/ingested/backend_index.doltlite_db)"
QMD_TAR="$(rlocation _main/tests/fixtures/ingested/qmd.tar)"
QMD_INDEX_TAR="$(rlocation _main/tests/fixtures/ingested/qmd-index.tar)"
[[ -x "$APPLET_BIN" ]]    || { echo "ERROR: datalib_applet not found at $APPLET_BIN" >&2; exit 1; }
[[ -f "$DB_FILE" ]]       || { echo "ERROR: backend_index.doltlite_db not found at $DB_FILE" >&2; exit 1; }
[[ -f "$QMD_TAR" ]]       || { echo "ERROR: qmd.tar not found at $QMD_TAR" >&2; exit 1; }
[[ -f "$QMD_INDEX_TAR" ]] || { echo "ERROR: qmd-index.tar not found at $QMD_INDEX_TAR" >&2; exit 1; }

command -v python3 >/dev/null || { echo "ERROR: python3 not on PATH" >&2; exit 1; }

mkdir -p "$OUT_ROOT"

# Both archives are rooted at `qmd/` (the genrule's staging dir name);
# strip that one component so the per-stanza markdown trees land at
# `<root>/<stanza>/rendered_md/...` and the index at `<root>/unified_index/qmd/`,
# where the backend's scanners look.
tar -xf "$QMD_TAR"       -C "$OUT_ROOT" --strip-components=1
tar -xf "$QMD_INDEX_TAR" -C "$OUT_ROOT" --strip-components=1

# Drop the doltlite file into its canonical home under `system/`; the
# backend opens it directly via `<data_root>/unified_index/grid/db.doltlite_db`.
mkdir -p "$OUT_ROOT/unified_index/grid"
cp "$DB_FILE" "$OUT_ROOT/unified_index/grid/db.doltlite_db"
chmod u+w "$OUT_ROOT/unified_index/grid/db.doltlite_db"

# The grid is served by the `unified_index` applet, so the config has to
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
cat > "$OUT_ROOT/config.toml" <<EOF
data_root = "$OUT_ROOT"

[[applets]]
id = "unified_index"
command = "'$APPLET_BIN' unified_index"
EOF

# qmd's GGUF models. They arrive as bazel inputs (`:qmd_models`, pinned
# in MODULE.bazel), so this no longer depends on the developer having run
# qmd at least once — which it used to, refusing with a "populate the
# shared cache first" message. That check existed because letting qmd
# download 2.2 GB silently is a multi-minute stall that masquerades as a
# hang; taking the models from bazel removes the stall instead of
# reporting it.
#
# Two details, both deliberate:
#
#   * The links point INTO the runfiles tree rather than copying 2.2 GB
#     into every root. Same assumption the applet `command` above already
#     makes — a runfiles path outlives the run that wrote it, up to a
#     `bazel clean`.
#   * `unified_index/qmd/models` has to be a SYMLINK, not the directory
#     itself. A later sync against this root calls
#     `qmd_indexer::ensure_models_symlink`, which errors out when it
#     finds a real directory at that path. So the real directory is a
#     sibling and the expected path points at it.
MODELS_DIR="$OUT_ROOT/unified_index/qmd_models"
mkdir -p "$MODELS_DIR" "$OUT_ROOT/unified_index/qmd"
for entry in \
  "qmd_model_embeddinggemma/file/hf_ggml-org_embeddinggemma-300M-Q8_0.gguf" \
  "qmd_model_query_expansion/file/hf_tobil_qmd-query-expansion-1.7B-q4_k_m.gguf" \
  "qmd_model_reranker/file/hf_ggml-org_qwen3-reranker-0.6b-q8_0.gguf"; do
  src="$(rlocation "$entry")" || src=""
  if [[ -z "$src" || ! -s "$src" ]]; then
    echo "ERROR: qmd model not found in runfiles: $entry" >&2
    echo "  (is \`:qmd_models\` still in materialize_tng_root's \`data\`?)" >&2
    exit 3
  fi
  ln -sfn "$src" "$MODELS_DIR/$(basename "$entry")"
done
ln -sfn "$MODELS_DIR" "$OUT_ROOT/unified_index/qmd/models"

# Drop the TNG-themed scan tree into the root as `fsindex_scan/`. It's a plain
# directory the `fsindex` (Unison-style) scanner can index; nothing renders it
# (fsindex is extract-only), so it just sits in the root alongside the
# per-stanza markdown trees. Anchor off the checked-in `.fsindex.yaml`
# breadcrumb and copy its containing dir, dereferencing the runfiles symlinks
# (`cp -RL`) so the materialized tree is real files, like a user's directory.
FSINDEX_BREADCRUMB="$(rlocation _main/datalib/backend/etl/providers/fsindex/tests/fixtures/fsindex_tng/.fsindex.yaml)"
if [[ -f "$FSINDEX_BREADCRUMB" ]]; then
  cp -RL "$(dirname "$FSINDEX_BREADCRUMB")" "$OUT_ROOT/fsindex_scan"
else
  echo "ERROR: fsindex_tng fixture not found at $FSINDEX_BREADCRUMB" >&2
  exit 1
fi

echo "$OUT_ROOT"
