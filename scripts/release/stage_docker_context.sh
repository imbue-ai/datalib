#!/usr/bin/env bash
# Lay out a docker build context as datalib/docker/Dockerfile expects it:
# the Dockerfile and entrypoint, the demo library's inputs, this
# architecture's binaries tarball and runtime asset under `dist/<arch>/`,
# and for the CUDA variant the two archives stage_cuda.sh made under
# `cuda/`, each checked against its sidecar first.
#
#   scripts/release/stage_docker_context.sh <ctx> <arch> <triple> <assets dir> [<cuda dir>]
#
# Run by release.yml's two image jobs and by .github/workflows/cuda.yml.
set -euo pipefail

if [[ $# -lt 4 || $# -gt 5 ]]; then
    echo "usage: $0 <ctx> <arch> <triple> <assets dir> [<cuda dir>]" >&2
    exit 2
fi
ctx="$1"
arch="$2"
triple="$3"
assets="$4"
cuda="${5:-}"

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd -P)"

mkdir -p "$ctx/dist/$arch"
cp "$repo_root/datalib/docker/Dockerfile" "$repo_root/datalib/docker/entrypoint.sh" "$ctx/"
"$repo_root/datalib/docker/stage_demo.sh" "$ctx"
cp "$assets/datalib-$triple.tar.gz" \
   "$assets/runtime-$triple.tar.gz" \
   "$assets/runtime-$triple.tar.gz.sha256" \
   "$ctx/dist/$arch/"
if [[ -n "$cuda" ]]; then
    mkdir -p "$ctx/cuda"
    for name in "ggml-cuda-$triple.tar.gz" "cuda-libs-$triple.tar.gz"; do
        (cd "$cuda" && sha256sum -c "$name.sha256" >&2)
        cp "$cuda/$name" "$ctx/cuda/"
    done
fi
ls -laR "$ctx/dist" ${cuda:+"$ctx/cuda"} >&2
