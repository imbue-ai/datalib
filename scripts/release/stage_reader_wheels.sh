#!/usr/bin/env bash
# Builds the two imbue-datalib-reader wheels of this machine's architecture,
# manylinux and musllinux, into <out-dir>, and prints each path. Each library is
# compiled inside the image that defines its platform tag, with no network, and
# that image's auditwheel then has to agree with the tag the wheel claims.
# docs/dev/release_steps.md §"The reader wheels".
#
#   READER_ARCH   x86_64 or aarch64, to build another architecture under an
#                 emulator (default: this machine's)
#   BAZEL         the bazel to run (default: bazel)
#   GITHUB_REF_TYPE, GITHUB_REF_NAME   set by Actions; see the version below
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <out-dir>" >&2
    exit 2
fi
mkdir -p "$1"
out_dir="$(cd "$1" && pwd)"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"

# The version datalib is. A tag's run stops if the tag says otherwise, since
# PyPI would keep whatever went up under that number for good.
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' datalib/backend/Cargo.toml | sed -n 1p)"
if [[ "${GITHUB_REF_TYPE:-}" == tag && "${GITHUB_REF_NAME:-}" != "v$version" ]]; then
    echo "ERROR: tag ${GITHUB_REF_NAME} disagrees with Cargo.toml's version $version" >&2
    exit 1
fi

arch="${READER_ARCH:-$(uname -m)}"
case "$arch" in
    x86_64 | amd64) arch=x86_64 docker_platform=linux/amd64 ;;
    aarch64 | arm64) arch=aarch64 docker_platform=linux/arm64 ;;
    *)
        echo "ERROR: no wheel for architecture $arch" >&2
        exit 1
        ;;
esac

# By digest, since whoever controls a tag would otherwise choose the compiler of
# what goes on PyPI. To move one: docker pull the name, then docker inspect
# --format '{{.RepoDigests}}'.
build_image() {
    case "$1-$arch" in
        manylinux-x86_64) echo "quay.io/pypa/manylinux2014_x86_64@sha256:dc58399aca4cfb0de257f92e2de71b4f8421e60cce8542fcd5e28df0c0760942" ;;
        manylinux-aarch64) echo "quay.io/pypa/manylinux2014_aarch64@sha256:db68ee9e5e879da5945dfbc8cab446f02245da6522aca0a9e4cda9256c911d2f" ;;
        musllinux-x86_64) echo "quay.io/pypa/musllinux_1_2_x86_64@sha256:0092b7d1019f3110cc691e4997e9838e7c9f086655643aac442f273ce90efdd0" ;;
        musllinux-aarch64) echo "quay.io/pypa/musllinux_1_2_aarch64@sha256:4b8c72510cd25410a9a12535ea15028b043bfc58a886741066ed06c9c06d4d3e" ;;
    esac
}

expected_tag() {
    case "$1" in
        manylinux) echo "manylinux_2_17_$arch" ;;
        musllinux) echo "musllinux_1_2_$arch" ;;
    esac
}

"${BAZEL:-bazel}" build \
    //third-party/doltlite:rename_amalgamation \
    //third-party/doltlite:library_copts >&2
bazel_bin="$("${BAZEL:-bazel}" info bazel-bin)"

work="$(mktemp -d)"
trap 'chmod -R u+w "$work"; rm -rf "$work"' EXIT
mkdir "$work/in"
cp "$bazel_bin/third-party/doltlite/sqlite3.c" \
    "$bazel_bin/third-party/doltlite/library_copts.txt" \
    "$work/in/"
chmod -R a+rX "$work"

in_image() {
    local image="$1"
    shift
    docker run --rm --network=none --platform "$docker_platform" \
        --user "$(id -u):$(id -g)" \
        -v "$repo/scripts/release:/scripts:ro" \
        -v "$work/in:/in:ro" \
        -v "$work/out:/out" \
        -v "$out_dir:/wheels" \
        "$image" "$@"
}

for libc in manylinux musllinux; do
    image="$(build_image "$libc")"
    rm -rf "$work/out"
    mkdir "$work/out"
    chmod a+rwx "$work/out"

    in_image "$image" bash /scripts/compile_reader_library.sh /in /out >&2

    wheel="$(python3 -I scripts/release/make_reader_wheel.py \
        --version "$version" \
        --libc "$libc" \
        --arch "$arch" \
        --module datalib/reader/datalib_reader.py \
        --library "$work/out/libdoltlite.so" \
        --readme datalib/reader/README.md \
        --license LICENSE=LICENSE \
        --license doltlite/LICENSE.md=third-party/doltlite/LICENSE.md \
        --license doltlite/APACHE_LICENSE=third-party/doltlite/APACHE_LICENSE \
        --out-dir "$out_dir")"

    tag="$(expected_tag "$libc")"
    verdict="$(in_image "$image" auditwheel show "/wheels/$(basename "$wheel")")"
    # auditwheel wraps its lines wherever it likes.
    if ! tr '\n' ' ' <<<"$verdict" | grep -q "platform tag: \"$tag\""; then
        echo "ERROR: $(basename "$wheel") claims $tag, and auditwheel says:" >&2
        echo "$verdict" >&2
        exit 1
    fi
    echo "$wheel"
done
