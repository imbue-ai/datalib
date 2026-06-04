# `frankweiler/docker/` — production runtime image

Source for the multi-arch image published to
`ghcr.io/imbue-ai/mixed_up_files:<tag>` on every `v*` tag push.

- **User docs** (how to bind-mount, register services, run a sync):
  [`docs/docker.md`](../../docs/docker.md).
- **CI publish path:** the `docker-publish` job in
  [`.github/workflows/release.yml`](../../.github/workflows/release.yml)
  downloads the per-triple Linux tarballs from the just-created GitHub
  Release and feeds them into the build context this directory expects.

This README is for working on the image itself.

## Files

| File             | Purpose                                                                                          |
|------------------|--------------------------------------------------------------------------------------------------|
| `Dockerfile`     | Multi-arch Ubuntu 24.04 image. Reads `ARG TARGETARCH` (set by `buildx`) to pick the right tarball. |
| `entrypoint.sh`  | Bootstraps `LATCHKEY_ENCRYPTION_KEY` from a per-bind-mount key file. PID 1 wrapper under tini.   |

## Building locally

Use [`scripts/build_docker.sh`](../../scripts/build_docker.sh) — it stages
the build context (Dockerfile + entrypoint + per-arch tarballs at
`dist/{amd64,arm64}/`) and invokes `docker buildx` with the right
platform flags.

```sh
# 1. Build for both arches against the LATEST published GitHub Release.
#    Default version comes from frankweiler/backend/Cargo.toml workspace
#    version. Builds into the buildx cache only (no push, no load) — fast
#    "does it still build?" smoke.
scripts/build_docker.sh

# 2. Same, but load the host-native arch into your local docker daemon so
#    you can `docker run` it. --load is single-arch only (buildx limit).
scripts/build_docker.sh --load

# 3. Build against a specific tagged release.
scripts/build_docker.sh 0.4.0 --load

# 4. Build against tarballs you produced locally (e.g. via
#    `bazelisk build //frankweiler/backend:dist -c opt` inside
#    .devcontainer/). The dir must contain BOTH:
#       frankweiler-x86_64-unknown-linux-gnu.tar.gz
#       frankweiler-aarch64-unknown-linux-gnu.tar.gz
scripts/build_docker.sh --tarball-dir /path/to/tarballs --load

# 5. Push to your own registry.
REPO=your-fork/mixed_up_files \
IMAGE_NAME=ghcr.io/your-fork/mixed_up_files \
scripts/build_docker.sh --push
```

Don't run `docker build` directly here — the Dockerfile expects
`dist/<arch>/...tar.gz` in the build context, and `scripts/build_docker.sh`
is what puts them there.

## Running locally

Once the image is loaded (`--load`), the bind-mount contract from
[`docs/docker.md`](../../docs/docker.md) applies unchanged. Quick
shorthand for iterating:

```sh
IMG=ghcr.io/imbue-ai/mixed_up_files:latest
LATCHKEY_DIR="$HOME/.frankweiler-docker/latchkey"
DATA_ROOT="$HOME/mixed_up_files"
mkdir -p "$LATCHKEY_DIR" "$DATA_ROOT"

# Register + auth (run once per service).
docker run --rm -it -v "$LATCHKEY_DIR:/root/.latchkey" "$IMG" \
    latchkey services register claude-ai --base-api-url=https://claude.ai/
docker run --rm -it -v "$LATCHKEY_DIR:/root/.latchkey" "$IMG" \
    latchkey auth set claude-ai -H "Cookie: sessionKey=$(pbpaste)"
docker run --rm -v "$LATCHKEY_DIR:/root/.latchkey" "$IMG" \
    latchkey auth list

# Sync (latchkey RO, data root RW). No qmd model cache mount needed —
# the image ships all three GGUFs pre-baked.
docker run --rm \
    -v "$LATCHKEY_DIR:/root/.latchkey:ro" \
    -v "$DATA_ROOT:/data" \
    "$IMG" frankweiler-sync

# Serve the HTTP backend.
docker run --rm -p 8731:8731 \
    -v "$LATCHKEY_DIR:/root/.latchkey:ro" \
    -v "$DATA_ROOT:/data" \
    "$IMG" frankweiler-http
```

## Smoke-testing changes

The CI publish path runs a latchkey roundtrip
(`services register` → `auth set` → fresh-container `auth list`)
inside the freshly built image before pushing to ghcr.io. If you've
edited the Dockerfile, the entrypoint, or anything in `frankweiler/`
that lands in the binaries, mirror that smoke locally before pushing:

```sh
scripts/build_docker.sh --load
IMG=ghcr.io/imbue-ai/mixed_up_files:latest
tmp=$(mktemp -d)

docker run --rm -v "$tmp:/root/.latchkey" "$IMG" \
    latchkey services register claude-ai --base-api-url=https://claude.ai/
docker run --rm -v "$tmp:/root/.latchkey" "$IMG" \
    latchkey auth set claude-ai -H "Cookie: sessionKey=smoke-test-not-real"
# A FRESH container reading the same bind mount must see the credential.
docker run --rm -v "$tmp:/root/.latchkey" "$IMG" latchkey auth list \
    | grep claude-ai

docker run --rm "$IMG" frankweiler-sync --version
rm -rf "$tmp"
```

If `latchkey auth list` doesn't show `claude-ai`, the entrypoint's
`LATCHKEY_ENCRYPTION_KEY` bootstrap is broken — see `entrypoint.sh`
and the "Latchkey encryption key" section in `docs/docker.md`.

## Image size budget

~2.7 GB total, dominated by the qmd-model layer:

| Layer                              | Approx size |
|------------------------------------|------------:|
| `ubuntu:24.04`                     |      ~75 MB |
| Node 22 + base runtime deps        |     ~200 MB |
| `latchkey` (npm global)            |      ~30 MB |
| **qmd GGUFs** (embed + rerank + expand) |  **~2.25 GB** |
| frankweiler binaries (3 of them)   |      ~40 MB |
| Everything else                    |     <100 MB |

The qmd layer is intentionally placed *before* the binary COPY so
version bumps to the release tarballs don't invalidate it. Bumping qmd's
default model URIs (rare) does; see the model-prefetch step in the
`Dockerfile` for the HF URLs.

## When binaries fail to start with a missing-shared-library error

The release binaries are produced by `bazel build
//frankweiler/backend:dist -c opt` and dynamic-link against glibc, libm,
libgcc_s, and (for `frankweiler-sync` / `frankweiler-http`) historically
also libsqlite3. The sqlite dep was removed by switching sqlx's feature
from `sqlite-unbundled` to `sqlite` so libsqlite3-sys's `bundled` mode
plus the Bazel `crate.annotation` on it would static-link our doltlite
build (see `frankweiler/backend/Cargo.toml`'s sqlx dep comment).

If a future binary regresses to dynamic libsqlite3 (or picks up a new
dyn dep), `docker run … frankweiler-sync --version` fails with
`error while loading shared libraries: …`. Smoke test (above) catches
this before the image ever ships.
