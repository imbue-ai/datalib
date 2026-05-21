#!/usr/bin/env bash
# Materialize a frankweiler data root from the bazel-built TNG fixture.
#
# Single source of truth for the on-disk layout shared between:
#   * `bazelisk run //frankweiler:dev_tng`           (frankweiler/dev_tng.sh)
#   * `bazelisk test //frankweiler/ui:e2e_test`      (run_e2e.sh → playwright)
#
# Produces, under <out-root>:
#   dolt_db/                Dolt repo with the fixture dump loaded.
#   rendered_md/...         Conversation markdown tree (from qmd.tar).
#   qmd/index.sqlite        QMD index (from qmd-index.tar).
#   qmd/models -> ~/.cache/qmd-models  (shared, populated externally)
#   config.yaml             { data_root, dolt.port } — backend reads via
#                           FRANKWEILER_CONFIG.
#
# Usage: materialize_tng_root.sh <out-root>
#
# Requires dolt + python3 on PATH. The qmd model cache at
# ~/.cache/qmd-models must already contain the required GGUF files —
# this script refuses to trigger a download (silent multi-minute stall).

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

DUMP="$(rlocation _main/tests/fixtures/ingested/dump.sql)"
QMD_TAR="$(rlocation _main/tests/fixtures/ingested/qmd.tar)"
QMD_INDEX_TAR="$(rlocation _main/tests/fixtures/ingested/qmd-index.tar)"
[[ -f "$DUMP" ]]          || { echo "ERROR: dump.sql not found at $DUMP" >&2; exit 1; }
[[ -f "$QMD_TAR" ]]       || { echo "ERROR: qmd.tar not found at $QMD_TAR" >&2; exit 1; }
[[ -f "$QMD_INDEX_TAR" ]] || { echo "ERROR: qmd-index.tar not found at $QMD_INDEX_TAR" >&2; exit 1; }

command -v dolt >/dev/null    || { echo "ERROR: dolt not on PATH" >&2; exit 1; }
command -v python3 >/dev/null || { echo "ERROR: python3 not on PATH" >&2; exit 1; }

mkdir -p "$OUT_ROOT"

# Both archives are rooted at `qmd/` (the genrule's staging dir name);
# strip that one component so providers / index land directly under
# <root>/, where the backend's scanners look.
tar -xf "$QMD_TAR"       -C "$OUT_ROOT" --strip-components=1
tar -xf "$QMD_INDEX_TAR" -C "$OUT_ROOT" --strip-components=1

# Init Dolt at <root>/dolt_db and load the byte-stable dump. Repo dir
# name == database name (frankweiler-core's DoltServer derives it from
# file_name()), so this MUST stay `dolt_db` to match the `USE dolt_db;`
# below and the backend's expected default.
mkdir -p "$OUT_ROOT/dolt_db"
(
  cd "$OUT_ROOT/dolt_db"
  dolt init --name "Frankweiler TNG" --email "tng@frankweiler.local" >/dev/null
  { echo "USE dolt_db;"; cat "$DUMP"; } | dolt sql
)

# Ephemeral Dolt port so concurrent runs (e2e shards, a second dev_tng,
# a host-side dev backend on 3306) don't collide.
DOLT_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
cat > "$OUT_ROOT/config.yaml" <<EOF
data_root: $OUT_ROOT
dolt:
  port: $DOLT_PORT
EOF

# qmd models live once in ~/.cache/qmd-models (~1.6 GB) and every data
# root symlinks them in. If the cache is empty we refuse — letting qmd
# download silently is a multi-minute stall that masquerades as a hang.
SHARED_MODELS="${HOME:-.}/.cache/qmd-models"
REQUIRED_MODELS=(
  "hf_ggml-org_embeddinggemma-300M-Q8_0.gguf"
  "hf_tobil_qmd-query-expansion-1.7B-q4_k_m.gguf"
)
missing=()
for m in "${REQUIRED_MODELS[@]}"; do
  p="$SHARED_MODELS/$m"
  if [[ ! -s "$p" ]]; then missing+=("$m"); fi
done
if (( ${#missing[@]} > 0 )); then
  {
    echo "ERROR: missing qmd models in $SHARED_MODELS:"
    for m in "${missing[@]}"; do echo "  - $m"; done
    echo
    echo "Populate the shared cache once by running the qmd indexer"
    echo "against any data root, e.g.:"
    echo "  bazelisk run //frankweiler/backend/qmd_indexer -- --root <some-frankweiler-root>"
  } >&2
  exit 3
fi
mkdir -p "$OUT_ROOT/qmd"
ln -sfn "$SHARED_MODELS" "$OUT_ROOT/qmd/models"

echo "$OUT_ROOT"
