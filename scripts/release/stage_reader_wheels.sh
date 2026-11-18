#!/usr/bin/env bash
# Builds imbue-datalib-reader wheels into <out-dir> and prints each path.
#
# `linux`: this machine's architecture, manylinux and musllinux. Each library is
# compiled inside the image that defines its platform tag, with no network, and
# that image's auditwheel then has to agree with the tag the wheel claims.
#
# `macos`: an Apple-silicon Mac's wheel, around the library a release's tarball
# ships. There is no auditwheel to ask, so this checks what the tag promises.
#
# docs/dev/release_steps.md §"The reader wheels".
#
#   READER_ARCH   `linux` only: x86_64 or aarch64, to build another architecture
#                 under an emulator (default: this machine's)
#   BAZEL         the bazel to run (default: bazel)
#   GITHUB_REF_TYPE, GITHUB_REF_NAME   set by Actions; see the version below
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <linux|macos> <out-dir>" >&2
    exit 2
fi
target="$1"
mkdir -p "$2"
out_dir="$(cd "$2" && pwd)"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"
bazel="${BAZEL:-bazel}"

# The version datalib is. A tag's run stops if the tag says otherwise, since
# PyPI would keep whatever went up under that number for good.
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' datalib/backend/Cargo.toml | sed -n 1p)"
if [[ "${GITHUB_REF_TYPE:-}" == tag && "${GITHUB_REF_NAME:-}" != "v$version" ]]; then
    echo "ERROR: tag ${GITHUB_REF_NAME} disagrees with Cargo.toml's version $version" >&2
    exit 1
fi

fail() {
    echo "ERROR: $*" >&2
    exit 1
}

# `$1` the make_reader_wheel.py target, `$2` the architecture, `$3` the library.
# Prints the wheel's path.
write_wheel() {
    python3 -I scripts/release/make_reader_wheel.py \
        --version "$version" \
        --target "$1" \
        --arch "$2" \
        --module datalib/reader/datalib_reader.py \
        --library "$3" \
        --readme datalib/reader/README.md \
        --license LICENSE=LICENSE \
        --license doltlite/LICENSE.md=third-party/doltlite/LICENSE.md \
        --license doltlite/APACHE_LICENSE=third-party/doltlite/APACHE_LICENSE \
        --out-dir "$out_dir"
}

stage_macos() {
    [[ "$(uname -s)-$(uname -m)" == Darwin-arm64 ]] || fail "the macos wheel builds on an Apple-silicon Mac"

    "$bazel" build -c opt //third-party/doltlite:libdoltlite.dylib >&2
    local library
    library="$("$bazel" info -c opt bazel-bin)/third-party/doltlite/libdoltlite.dylib"

    [[ "$(lipo -archs "$library")" == arm64 ]] || fail "$library is not arm64 alone"
    # What the wheel's macosx_11_0 tag promises; .bazelrc's --macos_minimum_os sets it.
    local build
    build="$(vtool -show-build "$library")"
    [[ "$build" == *"minos 11.0"* ]] || fail "$library does not say macOS 11.0: $build"
    # Apple silicon refuses to load an unsigned library, and a Mac has nothing from
    # outside the system to link against once this leaves the build machine.
    codesign --verify "$library"
    local outside
    outside="$(otool -L "$library" | sed 1d | awk '{print $1}' \
        | grep -v -E '^(@rpath/libdoltlite\.dylib$|/usr/lib/|/System/Library/)' || true)"
    [[ -z "$outside" ]] || fail "$library links outside the system: $outside"

    write_wheel macos arm64 "$library"
}

stage_linux() {
    local arch docker_platform
    arch="${READER_ARCH:-$(uname -m)}"
    case "$arch" in
        x86_64 | amd64) arch=x86_64 docker_platform=linux/amd64 ;;
        aarch64 | arm64) arch=aarch64 docker_platform=linux/arm64 ;;
        *) fail "no wheel for architecture $arch" ;;
    esac

    "$bazel" build \
        //third-party/doltlite:rename_amalgamation \
        //third-party/doltlite:library_copts >&2
    local bazel_bin
    bazel_bin="$("$bazel" info bazel-bin)"

    work="$(mktemp -d)"
    trap 'chmod -R u+w "$work"; rm -rf "$work"' EXIT
    mkdir "$work/in"
    cp "$bazel_bin/third-party/doltlite/sqlite3.c" \
        "$bazel_bin/third-party/doltlite/library_copts.txt" \
        "$work/in/"
    chmod -R a+rX "$work"

    local libc image wheel tag verdict
    for libc in manylinux musllinux; do
        # By digest, since whoever controls a tag would otherwise choose the
        # compiler of what goes on PyPI. To move one: docker pull the name, then
        # docker inspect --format '{{.RepoDigests}}'.
        case "$libc-$arch" in
            manylinux-x86_64)
                image="quay.io/pypa/manylinux2014_x86_64@sha256:dc58399aca4cfb0de257f92e2de71b4f8421e60cce8542fcd5e28df0c0760942"
                tag="manylinux_2_17_$arch"
                ;;
            manylinux-aarch64)
                image="quay.io/pypa/manylinux2014_aarch64@sha256:db68ee9e5e879da5945dfbc8cab446f02245da6522aca0a9e4cda9256c911d2f"
                tag="manylinux_2_17_$arch"
                ;;
            musllinux-x86_64)
                image="quay.io/pypa/musllinux_1_2_x86_64@sha256:0092b7d1019f3110cc691e4997e9838e7c9f086655643aac442f273ce90efdd0"
                tag="musllinux_1_2_$arch"
                ;;
            musllinux-aarch64)
                image="quay.io/pypa/musllinux_1_2_aarch64@sha256:4b8c72510cd25410a9a12535ea15028b043bfc58a886741066ed06c9c06d4d3e"
                tag="musllinux_1_2_$arch"
                ;;
        esac

        rm -rf "$work/out"
        mkdir "$work/out"
        chmod a+rwx "$work/out"
        docker run --rm --network=none --platform "$docker_platform" \
            --user "$(id -u):$(id -g)" \
            -v "$repo/scripts/release:/scripts:ro" \
            -v "$work/in:/in:ro" \
            -v "$work/out:/out" \
            "$image" bash /scripts/compile_reader_library.sh /in /out >&2

        wheel="$(write_wheel "$libc" "$arch" "$work/out/libdoltlite.so")"

        verdict="$(docker run --rm --network=none --platform "$docker_platform" \
            -v "$out_dir:/wheels:ro" \
            "$image" auditwheel show "/wheels/$(basename "$wheel")")"
        # auditwheel wraps its lines wherever it likes.
        if [[ "${verdict//$'\n'/ }" != *"platform tag: \"$tag\""* ]]; then
            fail "$(basename "$wheel") claims $tag, and auditwheel says: $verdict"
        fi
        echo "$wheel"
    done
}

case "$target" in
    linux) stage_linux ;;
    macos) stage_macos ;;
    *) fail "unknown target $target" ;;
esac
