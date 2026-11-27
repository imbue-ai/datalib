#!/usr/bin/env bash
# Lay out a docker build context as datalib/docker/Dockerfile expects it:
# the Dockerfile and entrypoint, the demo library, this
# architecture's binaries tarball and runtime asset under `dist/<arch>/`,
# and for the CUDA variant the two archives stage_cuda.sh made, each
# checked against its sidecar, then re-rooted under `cuda/` at the path
# the image holds it: `rootfs-cuda-libs.tar` (usr/local/lib/datalib-cuda/)
# and `rootfs-ggml-cuda.tar` (opt/datalib/). Rooted so that every
# directory in the layer an `ADD --link` makes of one comes from the
# archive with its fixed time, and none from the build's clock: the
# layers' digests are then the same from one release to the next
# (docs/dev/docker.md § "The CUDA variant").
#
#   scripts/release/stage_docker_context.sh <ctx> <arch> <triple> <assets dir> <library dir> [<cuda dir>]
#
# <library dir> holds the TNG library's `library.tar` and
# `config_body.toml` (release.yml's `tng-library` job), which
# datalib/docker/stage_demo.sh stages as the demo; `-` stages no demo,
# for a build of the image without it (`--target app`).
#
# Run by release.yml's two image jobs and by .github/workflows/cuda.yml.
set -euo pipefail

if [[ $# -lt 5 || $# -gt 6 ]]; then
    echo "usage: $0 <ctx> <arch> <triple> <assets dir> <library dir|-> [<cuda dir>]" >&2
    exit 2
fi
ctx="$1"
arch="$2"
triple="$3"
assets="$4"
library="$5"
cuda="${6:-}"

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd -P)"

mkdir -p "$ctx/dist/$arch"
cp "$repo_root/datalib/docker/Dockerfile" "$repo_root/datalib/docker/entrypoint.sh" "$ctx/"
if [[ "$library" != - ]]; then
    "$repo_root/datalib/docker/stage_demo.sh" "$ctx" "$library"
fi
cp "$assets/datalib-$triple.tar.gz" \
   "$assets/runtime-$triple.tar.gz" \
   "$assets/runtime-$triple.tar.gz.sha256" \
   "$ctx/dist/$arch/"
# <archive> unpacked under <prefix>, every entry owned by root and dated
# 1970 as stage_cuda.sh dates its own, written as <out>.
reroot() { # archive, prefix, out
    local tree
    tree="$(mktemp -d "${TMPDIR:-/tmp}/reroot.XXXXXX")"
    mkdir -p "$tree/$2"
    tar --no-same-owner -xzf "$1" -C "$tree/$2"
    tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 \
        -C "$tree" -cf "$3" "${2%%/*}"
    rm -rf "$tree"
}
if [[ -n "$cuda" ]]; then
    mkdir -p "$ctx/cuda"
    for name in "ggml-cuda-$triple.tar.gz" "cuda-libs-$triple.tar.gz"; do
        (cd "$cuda" && sha256sum -c "$name.sha256" >&2)
    done
    reroot "$cuda/cuda-libs-$triple.tar.gz" usr/local/lib/datalib-cuda "$ctx/cuda/rootfs-cuda-libs.tar"
    reroot "$cuda/ggml-cuda-$triple.tar.gz" opt/datalib "$ctx/cuda/rootfs-ggml-cuda.tar"
fi
ls -laR "$ctx/dist" ${cuda:+"$ctx/cuda"} >&2
