#!/usr/bin/env bash
# Asserts that the three `http_file` GGUF pins in `MODULE.bazel` still
# name the same HuggingFace repo + file that qmd resolves by default.
#
# Why this exists: `//tests/fixtures:ingested_tng_qmd` no longer lets qmd
# download its models — it hands qmd three Bazel-fetched `@qmd_model_*//file`
# GGUFs via `QMD_{EMBED,RERANK,GENERATE}_MODEL` (see
# tests/fixtures/build_qmd_index.py). Those files are pinned by URL +
# sha256 in MODULE.bazel. If a qmd upgrade changes a default model URI
# (the `DEFAULT_*_MODEL` constants in third-party/qmd/src/llm.ts), our
# pins would silently fetch a *different* model than the one qmd expects
# for that role. This test catches that drift: bump the MODULE.bazel pin
# (url revision + sha256) whenever a qmd default model changes.
#
# It compares repo/file identity only — NOT the pinned commit/sha256
# (those legitimately differ from `resolve/main`). The mapping:
#   qmd:    DEFAULT_EMBED_MODEL = "hf:<repo>/<file>"
#   module: urls = [".../huggingface.co/<repo>/resolve/<commit>/<file>"]
# both normalize to `<repo>/<file>`.
#
# Replaces the old check_qmd_model_cache_path.sh, which asserted the
# (now-removed) `--sandbox_add_mount_pair` host-cache path. Companion to
# //tools:qmd_version_pins_test (vendored qmd version vs DEFAULT_QMD_VERSION).

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

module_bazel="$(rlocation _main/MODULE.bazel)"
llm_ts="$(rlocation _main/third-party/qmd/src/llm.ts)"

for f in "$module_bazel" "$llm_ts"; do
    [[ -f "$f" ]] || { echo "ERROR: required input not found at $f" >&2; exit 1; }
done

# qmd's default model URI for a role, normalized to `<repo>/<file>`.
# Anchored to start-of-line so the commented-out alternative
# `// const DEFAULT_GENERATE_MODEL = ...` is ignored.
qmd_default() {
    local const_name=$1
    grep -E "^const ${const_name}[[:space:]]*=" "$llm_ts" \
        | head -n1 \
        | sed -E 's/.*"hf:([^"]+)".*/\1/'
}

# The http_file url for a repo name, normalized to `<repo>/<file>` by
# dropping the host and the `/resolve/<commit>` segment.
module_pin() {
    local repo_name=$1
    grep -A3 -E "name = \"${repo_name}\"" "$module_bazel" \
        | grep -E 'urls = \[' \
        | head -n1 \
        | sed -E 's#.*"https://huggingface.co/([^"]+)".*#\1#' \
        | sed -E 's#/resolve/[^/]+/#/#'
}

fails=0
check() {
    local label=$1 const_name=$2 repo_name=$3
    local want got
    want="$(qmd_default "$const_name")"
    got="$(module_pin "$repo_name")"
    if [[ -z "$want" ]]; then
        echo "  ${label}: ERROR — could not read ${const_name} from llm.ts" >&2
        fails=$((fails + 1))
    elif [[ -z "$got" ]]; then
        echo "  ${label}: ERROR — could not read http_file '${repo_name}' url from MODULE.bazel" >&2
        fails=$((fails + 1))
    elif [[ "$want" != "$got" ]]; then
        printf '  %-9s MODULE.bazel pins %-60s but qmd default is %s\n' \
            "$label" "$got" "$want" >&2
        fails=$((fails + 1))
    else
        printf '  %-9s = %s  ok\n' "$label" "$got"
    fi
}

echo "qmd model pins (MODULE.bazel http_file vs qmd DEFAULT_*_MODEL):"
check "embed"    "DEFAULT_EMBED_MODEL"    "qmd_model_embed"
check "rerank"   "DEFAULT_RERANK_MODEL"   "qmd_model_rerank"
check "generate" "DEFAULT_GENERATE_MODEL" "qmd_model_generate"

# Also assert qmd's model *cache dir* construction is unchanged. This
# guards the PRODUCT path (not the bazel fixture): `frankweiler-sync`'s
# indexer points qmd at `$XDG_CACHE_HOME` and symlinks
# `<root>/qmd/models` -> `default_models_dir()` (the prod image's baked
# `~/.cache/qmd/models`). That only works while qmd's MODEL_CACHE_DIR is
# `$XDG_CACHE_HOME/qmd/models` (homedir fallback `~/.cache/qmd/models`).
# Mirrors the rust-side `default_models_dir_matches_qmd_default` test.
xdg_pattern='process\.env\.XDG_CACHE_HOME[^"]*"qmd"[[:space:]]*,[[:space:]]*"models"'
home_pattern='homedir\(\)[^"]*"\.cache"[[:space:]]*,[[:space:]]*"qmd"[[:space:]]*,[[:space:]]*"models"'
if grep -qE -- "$xdg_pattern" "$llm_ts" && grep -qE -- "$home_pattern" "$llm_ts"; then
    echo "  cachedir  = \$XDG_CACHE_HOME/qmd/models (fallback ~/.cache/qmd/models)  ok"
else
    echo "  cachedir  ERROR — third-party/qmd/src/llm.ts MODEL_CACHE_DIR no longer" >&2
    echo "            joins XDG_CACHE_HOME / homedir() with (qmd, models). If qmd" >&2
    echo "            moved its model cache, update default_models_dir() in" >&2
    echo "            frankweiler/backend/qmd_indexer/src/lib.rs and the prod" >&2
    echo "            Dockerfile's bake path to match, then this pattern." >&2
    fails=$((fails + 1))
fi

if [[ "$fails" != "0" ]]; then
    cat >&2 <<EOF

${fails} qmd model pin(s) in MODULE.bazel disagree with qmd's default
model URIs in third-party/qmd/src/llm.ts. A qmd upgrade likely changed a
default model. For each role, update the matching http_file in
MODULE.bazel — new repo/file in the url, the immutable commit revision,
and the sha256:

  curl -sIL "https://huggingface.co/<repo>/resolve/main/<file>" \\
    | grep -iE 'x-repo-commit|x-linked-etag'
  # x-repo-commit -> url revision; x-linked-etag -> sha256

then re-run this test.
EOF
    exit 1
fi
