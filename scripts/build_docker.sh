#!/usr/bin/env bash
# Build (and optionally push) the multi-arch frankweiler runtime image.
#
# Usage:
#   scripts/build_docker.sh [VERSION] [--push] [--load] [--tarball-dir DIR]
#
# VERSION
#     Release tag to bake into the image, without the leading `v`. Defaults
#     to the workspace version from frankweiler/backend/Cargo.toml. The
#     script downloads the matching per-arch tarballs from the GitHub
#     Release `v$VERSION` of $REPO (default imbue-ai/datalib).
#
# --push
#     Push the built manifest list to $IMAGE_NAME:$VERSION and :latest.
#     Default is to build locally without pushing.
#
# --load
#     Load the (single-arch, host-native) image into the local docker
#     daemon so you can `docker run` it without pushing. Mutually exclusive
#     with --push: buildx can't both publish a manifest list and load
#     individual images in one invocation.
#
# --tarball-dir DIR
#     Skip the GitHub download step and use locally-built tarballs from
#     DIR (expected layout: $DIR/frankweiler-x86_64-unknown-linux-gnu.tar.gz
#     + $DIR/frankweiler-aarch64-unknown-linux-gnu.tar.gz). Useful when
#     iterating against a tarball you just produced via `bazel build
#     //frankweiler/backend:dist`.
#
# Environment:
#   REPO          owner/name on GitHub (default imbue-ai/datalib)
#   IMAGE_NAME    registry image ref (default ghcr.io/imbue-ai/datalib)
#
# Requires: docker (with buildx), gh, tar. `gh auth login` must have been
# run — the repo is private, so anonymous releases/download/<tag>/<file>
# URLs return 404. The `--tarball-dir` mode skips both gh and auth.

set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

REPO="${REPO:-imbue-ai/datalib}"
IMAGE_NAME="${IMAGE_NAME:-ghcr.io/imbue-ai/datalib}"

VERSION=""
PUSH=0
LOAD=0
TARBALL_DIR=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --push) PUSH=1; shift ;;
        --load) LOAD=1; shift ;;
        --tarball-dir) TARBALL_DIR="$2"; shift 2 ;;
        -h|--help) sed -n '2,32p' "$0"; exit 0 ;;
        -*) echo "unknown flag: $1" >&2; exit 2 ;;
        *) VERSION="$1"; shift ;;
    esac
done

if [[ ${PUSH} -eq 1 && ${LOAD} -eq 1 ]]; then
    echo "error: --push and --load are mutually exclusive (buildx limitation)" >&2
    exit 2
fi

if [[ -z "${VERSION}" ]]; then
    VERSION="$(grep -E '^version = "[^"]+"$' frankweiler/backend/Cargo.toml \
               | head -n1 | sed -E 's/^version = "([^"]+)"$/\1/')"
    echo "build_docker: VERSION not given, defaulting to Cargo.toml workspace version ${VERSION}"
fi

ctx="$(mktemp -d -t frankweiler-docker-XXXXXX)"
trap 'rm -rf "${ctx}"' EXIT INT TERM

# Stage the build context: Dockerfile + per-arch tarballs at the layout
# the Dockerfile expects (`dist/<arch>/...tar.gz`).
cp frankweiler/docker/Dockerfile "${ctx}/Dockerfile"
cp frankweiler/docker/entrypoint.sh "${ctx}/entrypoint.sh"
mkdir -p "${ctx}/dist/amd64" "${ctx}/dist/arm64"

fetch_tarball() {
    local triple="$1" arch_dir="$2"
    local name="frankweiler-${triple}.tar.gz"
    local dest_dir="${ctx}/dist/${arch_dir}"
    if [[ -n "${TARBALL_DIR}" ]]; then
        if [[ ! -f "${TARBALL_DIR}/${name}" ]]; then
            echo "error: ${TARBALL_DIR}/${name} not found" >&2
            exit 1
        fi
        cp "${TARBALL_DIR}/${name}" "${dest_dir}/${name}"
    else
        # `gh release download` instead of curl: the repo is private, so
        # plain HTTPS GETs against releases/download/<tag>/<file> return
        # 404 to anonymous clients. `gh` handles auth via the host
        # config from `gh auth login`. See docs/user/first_time_user.md for
        # the same pattern.
        echo "build_docker: gh release download v${VERSION} ${name} (repo ${REPO})"
        gh release download "v${VERSION}" \
            --repo "${REPO}" \
            --pattern "${name}" \
            --clobber \
            --dir "${dest_dir}"
    fi
}

if [[ -z "${TARBALL_DIR}" ]]; then
    command -v gh >/dev/null \
        || { echo "error: \`gh\` not on PATH — install via \`brew install gh\` and run \`gh auth login\`" >&2; exit 1; }
fi
fetch_tarball x86_64-unknown-linux-gnu  amd64
fetch_tarball aarch64-unknown-linux-gnu arm64

# Pick a builder that supports multi-platform. `docker buildx create
# --use` is idempotent enough for our purposes (errors if the name
# already exists; we just check and skip).
if ! docker buildx inspect frankweiler-builder >/dev/null 2>&1; then
    echo "build_docker: creating buildx builder 'frankweiler-builder'"
    docker buildx create --name frankweiler-builder --use >/dev/null
else
    docker buildx use frankweiler-builder >/dev/null
fi

tags=(--tag "${IMAGE_NAME}:${VERSION}" --tag "${IMAGE_NAME}:latest")

if [[ ${LOAD} -eq 1 ]]; then
    # `--load` only supports one platform at a time; pick the host's.
    host_arch="$(uname -m)"
    case "${host_arch}" in
        x86_64|amd64) plat="linux/amd64" ;;
        arm64|aarch64) plat="linux/arm64" ;;
        *) echo "error: cannot --load for host arch ${host_arch}" >&2; exit 1 ;;
    esac
    echo "build_docker: building ${plat} and loading into local docker"
    docker buildx build \
        --platform "${plat}" \
        "${tags[@]}" \
        --load \
        "${ctx}"
elif [[ ${PUSH} -eq 1 ]]; then
    echo "build_docker: building linux/amd64+linux/arm64 and pushing to ${IMAGE_NAME}"
    docker buildx build \
        --platform linux/amd64,linux/arm64 \
        "${tags[@]}" \
        --push \
        "${ctx}"
else
    # No --push and no --load: build both arches into the buildx cache
    # only. Useful for "does it build?" smoke checks without polluting
    # the local daemon or the registry.
    echo "build_docker: building linux/amd64+linux/arm64 (no push, no load)"
    docker buildx build \
        --platform linux/amd64,linux/arm64 \
        "${tags[@]}" \
        "${ctx}"
fi

echo "build_docker: done."
