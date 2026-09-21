#!/usr/bin/env bash
# release.yml's steps on Linux, from a mac: runs //tools:stage_runtime_test
# and //tools:stage_tarball_test inside the devcontainer — the image
# .devcontainer/Dockerfile builds for this host's arch, the working tree
# bind-mounted where devcontainer.json mounts it, the same named cache
# volumes — so what node-llama-cpp does only on Linux (v0.35.0's fork
# probe) shows up here rather than on a tag. Extra arguments go to
# `bazelisk test` inside the container.
#
#   bazelisk run //tools:release_steps_docker [-- <bazel test args>]
#
# DATALIB_DEVCONTAINER_IMAGE names an image to use instead of building
# one — on an x86_64 host, ghcr.io/imbue-ai/datalib_devcontainer:latest
# is the one CI runs in. The published image is amd64 only, and an
# emulated node-llama-cpp is not a faithful test, so an arm64 host
# builds its own.
#
# The first run pays for the image build and a cold Linux build of the
# shipped binaries (the better part of an hour); the volumes make the
# next one a few minutes. `--symlink_prefix=/` keeps the container's
# bazel from repointing the host tree's bazel-* symlinks.
set -euo pipefail

repo="${BUILD_WORKSPACE_DIRECTORY:-}"
[[ -n "$repo" ]] || { echo "run this through bazelisk run //tools:release_steps_docker" >&2; exit 2; }
command -v docker >/dev/null 2>&1 || { echo "docker not found on PATH" >&2; exit 1; }

image="${DATALIB_DEVCONTAINER_IMAGE:-}"
if [[ -z "$image" ]]; then
    image="datalib-devcontainer:local"
    # No warm-up: it compiles the backend in opt inside the image, which
    # the tests (fastbuild) would not reuse; the volumes below hold what
    # the run itself builds.
    echo ">>> building $image from .devcontainer/Dockerfile for $(uname -m)" >&2
    docker build --file "$repo/.devcontainer/Dockerfile" --build-arg WARM_BAZEL_CACHE=false \
        --tag "$image" "$repo" >&2
fi

echo ">>> running the release-step tests in $image" >&2
exec docker run --rm \
    --volume "$repo:/workspaces/datalib" \
    --workdir /workspaces/datalib \
    --volume datalib-bazel-output:/root/.cache/bazel \
    --volume datalib-bazel-disk:/root/Library/Caches/bazel-disk-cache \
    --volume datalib-npm:/root/.npm \
    "$image" \
    bazelisk test --symlink_prefix=/ --lockfile_mode=error --test_output=errors \
        //tools:stage_runtime_test //tools:stage_tarball_test "$@"
