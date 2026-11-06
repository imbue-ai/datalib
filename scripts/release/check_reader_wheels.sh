#!/usr/bin/env bash
# Installs the imbue-datalib-reader wheels in <wheel-dir> the way a person would,
# on the Pythons they are for, and reads a store the pinned shell wrote: the
# oldest glibc a manylinux wheel promises, a current Debian, Alpine, and `uv add`
# with the Python uv fetches on both libcs. <wheel-dir> holds both libcs' wheels,
# so each install also shows the installer choosing the right one.
# docs/dev/release_steps.md §"The reader wheels".
#
#   READER_ARCH   x86_64 or aarch64, to check another architecture under an
#                 emulator (default: this machine's)
#   BAZEL         the bazel to run (default: bazel)
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <wheel-dir>" >&2
    exit 2
fi
wheels="$(cd "$1" && pwd)"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"

arch="${READER_ARCH:-$(uname -m)}"
case "$arch" in
    x86_64 | amd64) docker_platform=linux/amd64 oldest=quay.io/pypa/manylinux2014_x86_64@sha256:dc58399aca4cfb0de257f92e2de71b4f8421e60cce8542fcd5e28df0c0760942 ;;
    aarch64 | arm64) docker_platform=linux/arm64 oldest=quay.io/pypa/manylinux2014_aarch64@sha256:db68ee9e5e879da5945dfbc8cab446f02245da6522aca0a9e4cda9256c911d2f ;;
    *)
        echo "ERROR: no wheel for architecture $arch" >&2
        exit 1
        ;;
esac

# By digest, each a multi-architecture index.
debian=mirror.gcr.io/library/python@sha256:09a17cad02668ae61ece136b4fd36ac3849e14f670ebedb637ea3e15580525bc  # 3.13-slim
alpine=mirror.gcr.io/library/python@sha256:1e1f6334529d1fb26a2ef619d7cffb30a7c7d7cc1cc7338ebee283a58d1aeaca  # 3.13-alpine
uv_debian=ghcr.io/astral-sh/uv@sha256:f6da342fe598dc462a7e6f61e7b3bf7fc84e381467efbdf29ccc1d32f2fc68a4       # 0.13.7-debian-slim
uv_alpine=ghcr.io/astral-sh/uv@sha256:a788290ce829958f18bcea1dfd9fb5dcf4bfb22f6380b52c9322f9d91e86b117       # 0.13.7-alpine

"${BAZEL:-bazel}" build //third-party/doltlite:doltlite >&2
shell="$("${BAZEL:-bazel}" info bazel-bin)/third-party/doltlite/doltlite"

fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT
store="$fixture/entities.doltlite_db"
"$shell" "$store" "CREATE TABLE t(id TEXT PRIMARY KEY);
    INSERT INTO t VALUES ('a');
    SELECT dolt_commit('-A', '-m', 'added a');" >/dev/null
first="$("$shell" "$store" "SELECT dolt_hashof('HEAD');")"
"$shell" "$store" "INSERT INTO t VALUES ('b');
    SELECT dolt_commit('-A', '-m', 'added b');" >/dev/null
chmod -R a+rX "$fixture"

# `$1` an image, `$2` the libc its wheel is for, `$3` what installs and runs it,
# `$4` the stock SQLite it holds to load alongside, where it has one.
# The fixture is mounted read-only: a reader that tried to write would fail.
run() {
    echo "=== $1 ($2)" >&2
    docker run --rm --platform "$docker_platform" \
        -v "$wheels:/wheels:ro" \
        -v "$fixture:/fixture:ro" \
        -v "$repo/scripts/release/check_reader_wheel_inside.py:/check.py:ro" \
        -e "WANT_LIBC=$2" \
        -e "FIRST_COMMIT=$first" \
        -e "STOCK_SQLITE=${4:-}" \
        "$1" sh -euc "$3"
}

with_pip() {
    echo "$1 -m venv /venv && /venv/bin/pip install --quiet --no-index --find-links /wheels imbue-datalib-reader && /venv/bin/python -I /check.py"
}
with_uv='mkdir /app && cd /app && uv init --quiet --bare --python 3.13 && uv add --quiet --find-links /wheels imbue-datalib-reader && uv run --quiet python -I /check.py'

run "$oldest" manylinux "$(with_pip /opt/python/cp39-cp39/bin/python)"
run "$debian" manylinux "$(with_pip python)" libsqlite3.so.0
run "$alpine" musllinux "$(with_pip python)" libsqlite3.so.0
run "$uv_debian" manylinux "$with_uv"
run "$uv_alpine" musllinux "$with_uv"
