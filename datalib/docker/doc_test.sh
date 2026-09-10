#!/usr/bin/env bash
# Runs docs/user/docker.md against one image.
#
# Every shell block in that page that carries a `<!-- doc-test: run
# <name> -->` marker is executed, in page order, in one shell. The
# page's own setup block (`<!-- doc-test: setup -->`) is replaced with
# values that fit a test: the image named by DATALIB_DOCKER_IMAGE
# (default ghcr.io/imbue-ai/datalib:latest), a free port, a temp data
# root, and the TNG mbox fixture as the user's export. After each named
# block, `doc_test_after` checks what it should have produced.
#
#   bazelisk test //datalib/docker:doc_test --test_env=DATALIB_DOCKER_IMAGE=…
#   DATALIB_DOCKER_IMAGE=… datalib/docker/doc_test.sh      # from a checkout
#
# Needs docker, curl and python3 on the host, and the image pullable.

set -euo pipefail

# --- locate the page and the fixture, under bazel or from a checkout ---
f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null \
  || source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f 2- -d ' ')" 2>/dev/null \
  || source "$0.runfiles/$f" 2>/dev/null \
  || source "$0.runfiles/_main/$f" 2>/dev/null \
  || true

if declare -F rlocation >/dev/null 2>&1 && [[ -n "${RUNFILES_DIR:-}${RUNFILES_MANIFEST_FILE:-}" ]]; then
    DOC="$(rlocation _main/docs/user/docker.md)"
    FIXTURE_MBOX="$(rlocation _main/datalib/backend/etl/providers/email/tests/fixtures/mbox/star_trek.mbox)"
else
    repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
    DOC="${repo_root}/docs/user/docker.md"
    FIXTURE_MBOX="${repo_root}/datalib/backend/etl/providers/email/tests/fixtures/mbox/star_trek.mbox"
fi
[[ -f "$DOC" ]] || { echo "doc_test: page not found at $DOC" >&2; exit 1; }
[[ -f "$FIXTURE_MBOX" ]] || { echo "doc_test: fixture not found at $FIXTURE_MBOX" >&2; exit 1; }
for tool in docker curl python3; do
    command -v "$tool" >/dev/null || { echo "doc_test: $tool not on PATH" >&2; exit 1; }
done

tmp="${TEST_TMPDIR:-$(mktemp -d -t datalib-doc-test)}"
mkdir -p "$tmp/import"
# A real file, not a runfiles symlink: a bind mount does not follow a
# symlink whose target is outside the mounted directory.
cp -L "$FIXTURE_MBOX" "$tmp/import/All mail Including Spam and Trash.mbox"

# --- the setup the page's setup block is replaced with ---
IMG="${DATALIB_DOCKER_IMAGE:-ghcr.io/imbue-ai/datalib:latest}"
PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')"
TOKEN="doc-test-$(date +%s)-$$"
DATA_ROOT="$tmp/root"
MBOX="$tmp/import/All mail Including Spam and Trash.mbox"
export IMG PORT TOKEN DATA_ROOT MBOX

cleanup() {
    docker rm -f datalib-demo datalib >/dev/null 2>&1 || true
}
trap cleanup EXIT

# --- helpers the after-hooks use ---
api() { curl -fsS -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$PORT$1"; }

wait_healthy() {
    local i
    for i in $(seq 1 60); do
        if api /api/health >/dev/null 2>&1; then return 0; fi
        sleep 1
    done
    echo "doc_test: server on port $PORT never answered /api/health" >&2
    docker logs "$1" >&2 || true
    return 1
}

# Prints "<total_estimated> <space-separated distinct sources>" for a
# query (`source` is the row's display label: "Claude", "Mail", "PDF"),
# retrying while the applet behind the grid is still starting.
rows() {
    local q="$1" i out
    for i in $(seq 1 30); do
        if out="$(api "/applet/unified_index/search?q=${q}&limit=200" 2>/dev/null | python3 -c '
import json, sys
d = json.load(sys.stdin)
print(d["total_estimated"], " ".join(sorted({r["source"] for r in d["rows"]})))
')"; then echo "$out"; return 0; fi
        sleep 1
    done
    echo "doc_test: search never answered" >&2
    return 1
}

assert_rows() {  # <query> <min total> [required provider…]
    local q="$1" min="$2"; shift 2
    local out total providers
    out="$(rows "$q")"; total="${out%% *}"; providers="${out#* }"
    echo "doc_test:   q='$q' -> $total rows, providers: $providers"
    (( total >= min )) || { echo "doc_test: expected at least $min rows for q='$q'" >&2; return 1; }
    local p
    for p in "$@"; do
        [[ " $providers " == *" $p "* ]] || { echo "doc_test: provider '$p' missing from results" >&2; return 1; }
    done
}

# Each marked block arrives here as a heredoc on stdin and runs in this
# shell, so the page's variables carry across blocks. The one block
# that is skipped is `pull` when the image is already local — a tag
# built and loaded on this machine has no registry to pull from.
doc_test_run() {
    local name="$1" body
    body="$(cat)"
    if [[ "$name" == pull ]] && docker image inspect "$IMG" >/dev/null 2>&1; then
        echo; echo "doc_test: ### $name (skipped: $IMG is already present locally)"
        return 0
    fi
    echo; echo "doc_test: ### $name"
    eval "$body"
    doc_test_after "$name"
}

doc_test_after() {
    case "$1" in
        demo-serve)
            wait_healthy datalib-demo
            assert_rows "" 50 Claude Mail PDF LinkedIn
            assert_rows "Picard" 1
            ;;
        demo-index)
            docker exec datalib-demo test -s /opt/datalib/demo/unified_index/qmd_index/qmd/index.sqlite
            assert_rows "warp%20core" 1
            ;;
        demo-sql|own-sql)
            ;;  # the block itself fails if the store cannot be read
        own-ingest)
            test -s "$DATA_ROOT/unified_index/grid_index/db.doltlite_db"
            test -s "$DATA_ROOT/unified_index/qmd_index/qmd/index.sqlite"
            ;;
        own-serve)
            wait_healthy datalib
            assert_rows "" 1 Mail
            ;;
        pull|demo-stop|own-config|own-stop)
            ;;
        *)
            echo "doc_test: no check written for block '$1'" >&2
            return 1
            ;;
    esac
}

# --- extract the marked blocks into one script ---
script="$tmp/doc_blocks.sh"
awk '
    /<!-- doc-test: setup -->/            { want = "setup"; next }
    /<!-- doc-test: run [a-z-]+ -->/      { want = $4; next }
    want != "" && /^[[:space:]]*$/         { next }
    want != "" && /^```sh/                 { block = want; want = ""; skip = (block == "setup");
                                             if (!skip) print "doc_test_run " block " <<'"'"'DOC_TEST_BLOCK'"'"'"; next }
    want != ""                             { want = "" }
    block != "" && /^```/                  { if (!skip) print "DOC_TEST_BLOCK"; block = ""; next }
    block != "" && !skip                   { print }
' "$DOC" > "$script"
grep -q '^doc_test_run' "$script" || { echo "doc_test: no marked blocks found in $DOC" >&2; exit 1; }

echo "doc_test: image $IMG, port $PORT, data root $DATA_ROOT"
# shellcheck disable=SC1090
source "$script"
echo
echo "doc_test: every marked block in $(basename "$DOC") ran and checked out"
