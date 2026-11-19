#!/usr/bin/env bash
# Run inside the oldest Linux the release supports (release.yml's `build`
# job starts it in `ubuntu:24.04`; docs/dev/release_steps.md §"The Linux
# floor"): unpack this leg's tarball and its Node runtime, ask the dynamic
# loader whether every ELF file in them would load here, and start the
# binaries a person runs first. check_glibc_floor.sh reads the version names;
# this is the same promise kept by the real loader, which also notices a
# shared library the distro does not have.
#
#   scripts/release/smoke_linux_assets.sh <datalib-tarball> <runtime-tarball>
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <datalib-tarball> <runtime-tarball>" >&2
    exit 2
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/datalib" "$work/runtime"
tar -xzf "$1" -C "$work/datalib"
tar -xzf "$2" -C "$work/runtime"

echo "glibc here: $(getconf GNU_LIBC_VERSION)"

failed=0
loaded=0
while IFS= read -r file; do
    magic=""
    LC_ALL=C IFS= read -r -d '' -n 4 magic < "$file" || true
    [[ "$magic" == $'\x7fELF' ]] || continue
    out="$(ldd "$file" 2>&1 || true)"
    # The runtime carries npm's musl prebuilds beside the glibc ones, for
    # a musl host to pick; musl names its libc `libc.so` or `libc.musl-*`.
    case "$out" in
        *"not a dynamic executable"* | *"statically linked"*) ;;
        *"libc.so => not found"* | *"libc.musl-"*) ;;
        *"not found"*)
            echo "smoke_linux_assets: error: ${file#"$work"/} would not load here:" >&2
            printf '%s\n' "$out" | grep 'not found' >&2
            failed=1
            ;;
    esac
    loaded=$((loaded + 1))
done < <(find "$work" -type f | LC_ALL=C sort)
echo "ldd: $loaded ELF files"
[[ $loaded -gt 0 ]] || { echo "smoke_linux_assets: error: no ELF file to load" >&2; exit 1; }

bin="$(echo "$work"/datalib/datalib-*)"
[[ -d "$bin" ]] || { echo "smoke_linux_assets: error: no datalib-*/ directory in $1" >&2; exit 1; }
for name in datalib-dag datalib-step datalib-http datalib-applet datalib-doltlite; do
    "$bin/$name" --version || { echo "smoke_linux_assets: error: $name did not start" >&2; failed=1; }
done
"$work/runtime/node/bin/node" --version \
    || { echo "smoke_linux_assets: error: the runtime's node did not start" >&2; failed=1; }

exit "$failed"
