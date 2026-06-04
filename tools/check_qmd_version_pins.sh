#!/usr/bin/env bash
# Asserts that every qmd version pin in the workspace agrees with the
# canonical pin in `frankweiler/backend/qmd_indexer/src/lib.rs`'s
# `DEFAULT_QMD_VERSION` constant.
#
# Why this exists: qmd is installed/invoked from several places that all
# have to agree, or first-run behavior silently diverges between dev and
# prod (the Dockerfiles bake one version into the image, but
# `frankweiler-sync` runtime invokes a different version via `npx -y
# @tobilu/qmd@<v>` — leaving the baked model layer unused). When you
# bump qmd, update `DEFAULT_QMD_VERSION` first and chase the other
# pins until this test passes.
#
# Pins checked:
#   * frankweiler/backend/qmd_indexer/src/lib.rs   DEFAULT_QMD_VERSION  (canonical)
#   * tests/fixtures/BUILD.bazel                   QMD_VERSION
#   * frankweiler/docker/Dockerfile                ARG QMD_VERSION
#   * .devcontainer/Dockerfile                     ARG QMD_VERSION
#
# Companion to //tools:qmd_model_cache_path_test (which asserts the
# vendored qmd snapshot's cache-path matches what
# `npx -y @tobilu/qmd@<DEFAULT_QMD_VERSION>` writes to at runtime).

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

indexer_lib="$(rlocation _main/frankweiler/backend/qmd_indexer/src/lib.rs)"
fixtures_build="$(rlocation _main/tests/fixtures/BUILD.bazel)"
prod_dockerfile="$(rlocation _main/frankweiler/docker/Dockerfile)"
dev_dockerfile="$(rlocation _main/.devcontainer/Dockerfile)"

for f in "$indexer_lib" "$fixtures_build" "$prod_dockerfile" "$dev_dockerfile"; do
    [[ -f "$f" ]] || { echo "ERROR: required input not found at $f" >&2; exit 1; }
done

# Each extractor pulls the version literal from the matching file's
# pin. Designed to fail loudly (empty result → "" mismatch → reported
# below) rather than silently treat a missing pin as match.
extract_from_indexer() {
    grep -E '^pub const DEFAULT_QMD_VERSION:' "$1" \
        | sed -E 's/.*"([^"]+)".*/\1/'
}
extract_from_fixtures_build() {
    grep -E '^QMD_VERSION = "' "$1" \
        | sed -E 's/.*"([^"]+)".*/\1/'
}
extract_from_dockerfile_arg() {
    # ARG QMD_VERSION=2.5.3  (no surrounding quotes, optional trailing
    # comment). Pin to start-of-line so we don't pick up references
    # inside RUN strings like `@tobilu/qmd@${QMD_VERSION}`.
    grep -E '^ARG QMD_VERSION=' "$1" \
        | sed -E 's/^ARG QMD_VERSION=([^ ]+).*/\1/' \
        | head -n1
}

canonical="$(extract_from_indexer "$indexer_lib")"
fixtures_v="$(extract_from_fixtures_build "$fixtures_build")"
prod_v="$(extract_from_dockerfile_arg "$prod_dockerfile")"
dev_v="$(extract_from_dockerfile_arg "$dev_dockerfile")"

if [[ -z "$canonical" ]]; then
    echo "ERROR: failed to extract DEFAULT_QMD_VERSION from $indexer_lib" >&2
    exit 1
fi

# Collect mismatches first so the user gets ALL the drift in one
# error message, not a one-at-a-time discovery loop.
fails=0
report() {
    local label=$1 found=$2
    if [[ "$found" != "$canonical" ]]; then
        printf '  %-46s = %-10s  (expected %s)\n' "$label" "${found:-<not found>}" "$canonical" >&2
        fails=$((fails + 1))
    else
        printf '  %-46s = %-10s  ok\n' "$label" "$found"
    fi
}

echo "qmd version pins (canonical: ${canonical}):"
report "frankweiler/backend/qmd_indexer/.../lib.rs"  "$canonical"
report "tests/fixtures/BUILD.bazel"                  "$fixtures_v"
report "frankweiler/docker/Dockerfile"               "$prod_v"
report ".devcontainer/Dockerfile"                    "$dev_v"

if [[ "$fails" != "0" ]]; then
    cat >&2 <<EOF

${fails} qmd version pin(s) disagree with the canonical
DEFAULT_QMD_VERSION (${canonical}) declared in
frankweiler/backend/qmd_indexer/src/lib.rs.

Update the diverging files above to match, or — if upstream qmd has
a new release worth tracking — bump DEFAULT_QMD_VERSION (and the
vendored snapshot under third-party/qmd/ if //tools:qmd_model_cache_path_test
complains) and then update the rest.
EOF
    exit 1
fi
