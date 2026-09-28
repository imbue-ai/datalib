# `datalib/docker/` — production runtime image

Source for the multi-arch image published to
`ghcr.io/imbue-ai/datalib:<tag>` on every `v*` tag push.

- **User walkthrough** (the demo, your own data, credentials):
  [`docs/user/docker.md`](/docs/user/docker.md). **What is in the
  image and how it is built:** [`docs/dev/docker.md`](/docs/dev/docker.md).
- **CI publish path:** the `docker-publish` job in
  [`.github/workflows/release.yml`](../../.github/workflows/release.yml)
  downloads the per-triple Linux tarballs from the just-created GitHub
  Release and feeds them into the build context this directory expects.

This README is for working on the image itself.

## Files

| File             | Purpose                                                                                          |
|------------------|--------------------------------------------------------------------------------------------------|
| `Dockerfile`     | Multi-arch Ubuntu 26.04 image. Reads `ARG TARGETARCH` (set by `buildx`) to pick the right tarball. |
| `entrypoint.sh`  | Bootstraps `LATCHKEY_ENCRYPTION_KEY` from a per-bind-mount key file. PID 1 wrapper under tini.   |
| `demo/config.toml` | The demo data library baked into the image at `/opt/datalib/demo`. |
| `stage_demo.sh`  | Copies the demo's fixture inputs + config into a build context (`build_docker.sh` and release.yml both call it). |
| `doc_test.sh`    | Runs `docs/user/docker.md`'s shell blocks against an image; `//datalib/docker:doc_test` in Bazel, and a release.yml step. |

## Building locally

Use [`scripts/build_docker.sh`](../../scripts/build_docker.sh) — it stages
the build context (Dockerfile + entrypoint + the demo's inputs + per-arch
tarballs at `dist/{amd64,arm64}/`) and invokes `docker buildx` with the
right platform flags.

```sh
# 1. Build for both arches against the LATEST published GitHub Release.
#    Default version comes from datalib/backend/Cargo.toml workspace
#    version. Builds into the buildx cache only (no push, no load) — fast
#    "does it still build?" smoke.
scripts/build_docker.sh

# 2. Same, but load the host-native arch into your local docker daemon so
#    you can `docker run` it. --load is single-arch only (buildx limit).
scripts/build_docker.sh --load

# 3. Build against a specific tagged release.
scripts/build_docker.sh 0.4.0 --load

# 4. Build against assets you produced locally (the binaries via
#    `bazelisk build //datalib/backend:dist -c opt` inside
#    .devcontainer/, the runtime via scripts/stage_runtime.sh). The dir
#    must contain, for BOTH x86_64 and aarch64:
#       datalib-<triple>-unknown-linux-gnu.tar.gz
#       runtime-<triple>-unknown-linux-gnu.tar.gz (+ .sha256)
scripts/build_docker.sh --tarball-dir /path/to/tarballs --load

# 5. Push to your own registry.
REPO=your-fork/datalib \
IMAGE_NAME=ghcr.io/your-fork/datalib \
scripts/build_docker.sh --push
```

Don't run `docker build` directly here — the Dockerfile expects
`dist/<arch>/...tar.gz` and `demo/` in the build context, and
`scripts/build_docker.sh` is what puts them there.

## Running locally

Once the image is loaded (`--load`), everything in
[`docs/user/docker.md`](/docs/user/docker.md) applies unchanged with
`IMG=ghcr.io/imbue-ai/datalib:latest` — the demo, a mounted export, the
credential store.

## Smoke-testing changes

The image build itself runs the shipped pipeline over the demo
fixtures, so a tarball whose `datalib-dag` / `datalib-step` cannot
ingest them fails the build. After a `--load`, run the walkthrough
against the result — it serves the demo, builds the semantic index,
ingests a mounted mbox, and reads the stores back:

```sh
scripts/build_docker.sh --load
DATALIB_DOCKER_IMAGE=ghcr.io/imbue-ai/datalib:latest datalib/docker/doc_test.sh
```

The entrypoint's `LATCHKEY_ENCRYPTION_KEY` bootstrap is the one thing
that test does not cover (it needs no credential). Check it by hand
when you touch `entrypoint.sh`: a credential set in one container must
be listed by a fresh one reading the same bind mount.

```sh
IMG=ghcr.io/imbue-ai/datalib:latest
tmp=$(mktemp -d)
docker run --rm -v "$tmp:/root/.latchkey" "$IMG" \
    latchkey services register claude-ai --base-api-url=https://claude.ai/
docker run --rm -v "$tmp:/root/.latchkey" "$IMG" \
    latchkey auth set claude-ai -H "Cookie: sessionKey=smoke-test-not-real"
docker run --rm -v "$tmp:/root/.latchkey" "$IMG" latchkey auth list | grep claude-ai
rm -rf "$tmp"
```

## Image size

~2.7 GB, dominated by qmd's three GGUF models (~2.25 GB: embed, rerank,
expand), which `datalib-step pull-models` fetches from their pins
(`datalib_runtime::qmd::PINNED_MODELS`). That step runs from the
unpacked tarball, so it comes after the binaries' layer and a new
tarball re-runs it. The `-slim` tag (`QMD_PREFETCH_MODELS=false`) is
the same image without the models.

## When binaries fail to start with a missing-shared-library error

The gnu release binaries link glibc dynamically and doltlite statically
(sqlx's `sqlite` feature; see the sqlx comment in
`datalib/backend/Cargo.toml`). If one picks up a new dynamic
dependency, the Dockerfile's `datalib-dag --version` smoke and the demo
ingest fail the image build with `error while loading shared
libraries: …`, before the image ever ships.
