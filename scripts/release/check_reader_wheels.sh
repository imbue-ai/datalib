#!/usr/bin/env bash
# Installs the imbue-datalib-reader wheels in <wheel-dir> the way a person would,
# on the Pythons they are for, and reads a store the pinned shell wrote.
#
# `linux`: the oldest glibc a manylinux wheel promises, a current Debian, Alpine,
# and `uv add` with the Python uv fetches on both libcs. <wheel-dir> holds both
# libcs' wheels, so each install also shows the installer choosing the right one.
#
# `macos`: the Python Xcode's command line tools install, the oldest a Mac is
# likely to have, and `uv add` with the Python uv fetches.
#
# docs/dev/release_steps.md §"The reader wheels".
#
#   READER_ARCH   `linux` only: x86_64 or aarch64, to check another architecture
#                 under an emulator (default: this machine's)
#   BAZEL         the bazel to run (default: bazel)
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <linux|macos> <wheel-dir>" >&2
    exit 2
fi
target="$1"
wheels="$(cd "$2" && pwd)"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"
bazel="${BAZEL:-bazel}"
check="$repo/scripts/release/check_reader_wheel_inside.py"

"$bazel" build //third-party/doltlite:doltlite >&2
shell="$("$bazel" info bazel-bin)/third-party/doltlite/doltlite"

scratch="$(mktemp -d)"
trap 'chmod -R u+w "$scratch"; rm -rf "$scratch"' EXIT
fixture="$scratch/fixture"
mkdir "$fixture"
store="$fixture/entities.doltlite_db"
"$shell" "$store" "CREATE TABLE t(id TEXT PRIMARY KEY);
    INSERT INTO t VALUES ('a');
    SELECT dolt_commit('-A', '-m', 'added a');" >/dev/null
first="$("$shell" "$store" "SELECT dolt_hashof('HEAD');")"
"$shell" "$store" "INSERT INTO t VALUES ('b');
    SELECT dolt_commit('-A', '-m', 'added b');" >/dev/null
# Read-only, so a reader that tried to write would fail.
chmod -R a+rX,a-w "$fixture"

check_macos() {
    local dir
    export STORE="$store" FIRST_COMMIT="$first" WANT_PLATFORM=macosx

    echo "=== /usr/bin/python3 with pip" >&2
    dir="$scratch/pip"
    /usr/bin/python3 -m venv "$dir"
    "$dir/bin/pip" install --quiet --disable-pip-version-check --no-index --find-links "$wheels" imbue-datalib-reader
    STOCK_SQLITE=/usr/lib/libsqlite3.dylib "$dir/bin/python" -I "$check"

    echo "=== uv add, with the Python uv fetches" >&2
    dir="$scratch/uv"
    mkdir "$dir"
    (
        cd "$dir"
        export UV_PYTHON_PREFERENCE=only-managed
        uv init --quiet --bare --python 3.13
        uv add --quiet --find-links "$wheels" imbue-datalib-reader
        uv run --quiet python -I "$check"
    )
}

check_linux() {
    local arch docker_platform oldest
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
    local debian=mirror.gcr.io/library/python@sha256:09a17cad02668ae61ece136b4fd36ac3849e14f670ebedb637ea3e15580525bc  # 3.13-slim
    local alpine=mirror.gcr.io/library/python@sha256:1e1f6334529d1fb26a2ef619d7cffb30a7c7d7cc1cc7338ebee283a58d1aeaca  # 3.13-alpine
    local uv_debian=ghcr.io/astral-sh/uv@sha256:f6da342fe598dc462a7e6f61e7b3bf7fc84e381467efbdf29ccc1d32f2fc68a4       # 0.13.7-debian-slim
    local uv_alpine=ghcr.io/astral-sh/uv@sha256:a788290ce829958f18bcea1dfd9fb5dcf4bfb22f6380b52c9322f9d91e86b117       # 0.13.7-alpine

    # `$1` an image, `$2` the wheel it should choose, `$3` what installs and runs
    # it, `$4` the stock SQLite it holds to load alongside, where it has one.
    run() {
        echo "=== $1 ($2)" >&2
        docker run --rm --platform "$docker_platform" \
            -v "$wheels:/wheels:ro" \
            -v "$fixture:/fixture:ro" \
            -v "$check:/check.py:ro" \
            -e "STORE=/fixture/entities.doltlite_db" \
            -e "WANT_PLATFORM=$2" \
            -e "FIRST_COMMIT=$first" \
            -e "STOCK_SQLITE=${4:-}" \
            "$1" sh -euc "$3"
    }

    with_pip() {
        echo "$1 -m venv /venv && /venv/bin/pip install --quiet --no-index --find-links /wheels imbue-datalib-reader && /venv/bin/python -I /check.py"
    }
    local with_uv='mkdir /app && cd /app && uv init --quiet --bare --python 3.13 && uv add --quiet --find-links /wheels imbue-datalib-reader && uv run --quiet python -I /check.py'

    run "$oldest" manylinux "$(with_pip /opt/python/cp39-cp39/bin/python)"
    run "$debian" manylinux "$(with_pip python)" libsqlite3.so.0
    run "$alpine" musllinux "$(with_pip python)" libsqlite3.so.0
    run "$uv_debian" manylinux "$with_uv"
    run "$uv_alpine" musllinux "$with_uv"
}

case "$target" in
    linux) check_linux ;;
    macos) check_macos ;;
    *)
        echo "ERROR: unknown target $target" >&2
        exit 1
        ;;
esac
