# Devcontainer that inherits from the prod runtime image and adds the
# Bazel build toolchain on top. The prod image already carries
# `ubuntu:24.04` + Node 22 + latchkey + the doltlite CLI + qmd's three
# default GGUFs at /root/.cache/qmd/models + the bind-mount target
# directories, so inheriting from it gives the devcontainer all of that
# "for free" — no byte-for-byte Dockerfile mirroring, no double
# download. Contributors who've already pulled the prod image during
# `docker pull ghcr.io/imbue-ai/mixed_up_files:<ver>` (or anyone who
# ran `scripts/build_docker.sh --load`) reuse the cached layers; a
# fresh `devcontainer up` only needs to fetch the dev-only build
# toolchain layers added below.
#
# Pin: the FROM tag should track a published prod image. Bump by
# editing the ARG default below; the bump is intentionally manual
# (rather than `:latest`) so devcontainer builds are reproducible.
ARG PROD_IMAGE_TAG=0.5.2
FROM ghcr.io/imbue-ai/mixed_up_files:${PROD_IMAGE_TAG}

# Reset the prod image's ENTRYPOINT. Prod wraps everything in
# `tini -- frankweiler-entrypoint.sh`, which bootstraps
# LATCHKEY_ENCRYPTION_KEY from a per-bind-mount key file before
# exec'ing the user's command. Inside a VS Code devcontainer that
# wrapping just gets in the way — interactive shells, `bazel run`,
# `cargo test`, etc. don't want latchkey state magically appearing.
# Empty `ENTRYPOINT []` + `CMD []` defers to whatever
# `devcontainer.json` or the user runs explicitly.
ENTRYPOINT []
CMD []

ENV DEBIAN_FRONTEND=noninteractive

# Build toolchain that prod doesn't ship. Mirrors what GHA's
# `ubuntu-latest` carries that `release.yml`'s matrix build assumes.
# Items already present in prod (build-essential, python3) are NOT
# repeated here — apt would happily skip them as already-installed,
# but listing only the deltas makes the dev-vs-prod surface visible.
#
# cmake + perl + pkg-config + libssl-dev
#                   boring-sys2 + openssl-sys build-script deps.
# clang + libclang-dev + llvm
#                   bindgen (libsqlite3-sys's build script) dlopens
#                   libclang to parse <sqlite3.h>. macOS gets clang
#                   via Xcode CLT; on Ubuntu we install it explicitly.
# lld               rustc on Linux still defaults to the deprecated
#                   gold linker and warns; lld present means we can
#                   opt in via RUSTFLAGS/--linkopt if we want to
#                   silence that.
# git               needed by some build scripts (workspace_status.sh,
#                   rules_rust crate fetches that vendor from git).
# python3-pip       `tests/fixtures/build_qmd_index.py` and friends.
# sudo              passwordless sudo for the `ubuntu` user (below).
# unzip + zip       rules_rust workspace setup occasionally pulls
#                   these in; cheap to keep.
RUN apt-get update && apt-get install -y --no-install-recommends \
        clang \
        cmake \
        git \
        libclang-dev \
        libssl-dev \
        lld \
        llvm \
        perl \
        pkg-config \
        python3-pip \
        sudo \
        unzip \
        zip \
    && rm -rf /var/lib/apt/lists/*

# bun — not needed for the bazel build, but qmd's own dev workflow
# uses it (`bun install`, `bun src/cli/qmd.ts <cmd>`) so working
# inside `third-party/qmd/` doesn't need extra setup.
ARG BUN_VERSION=1.1.45
RUN curl -fsSL https://bun.sh/install | bash -s "bun-v${BUN_VERSION}" \
    && mv /root/.bun/bin/bun /usr/local/bin/bun \
    && ln -s /usr/local/bin/bun /usr/local/bin/bunx \
    && rm -rf /root/.bun \
    && bun --version

# bazelisk reads `.bazelversion` and downloads the matching bazel, so
# we only pin bazelisk here.
ARG BAZELISK_VERSION=1.22.0
ARG TARGETARCH
RUN ARCH=$(case "${TARGETARCH:-$(dpkg --print-architecture)}" in \
            amd64) echo amd64 ;; \
            arm64) echo arm64 ;; \
            *) echo "unknown arch: ${TARGETARCH:-$(dpkg --print-architecture)}" >&2; exit 1 ;; \
        esac) \
    && curl -fsSL -o /usr/local/bin/bazelisk \
        "https://github.com/bazelbuild/bazelisk/releases/download/v${BAZELISK_VERSION}/bazelisk-linux-${ARCH}" \
    && chmod +x /usr/local/bin/bazelisk \
    && ln -sf /usr/local/bin/bazelisk /usr/local/bin/bazel

# Non-root user. Ubuntu 24.04's base image already ships an `ubuntu`
# user at uid 1000 (inherited via prod's `FROM ubuntu:24.04`) — we
# reuse it (rather than minting `vscode`) so host-bind-mounted files
# don't end up owned by a foreign uid.
RUN usermod -aG sudo ubuntu \
    && echo 'ubuntu ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/ubuntu \
    && chmod 0440 /etc/sudoers.d/ubuntu

# The prod image baked qmd models into /root/.cache/qmd/models (owned
# by root, dir mode inherited from `mkdir -p` = 0755). `.bazelrc`
# bind-mounts `$HOME/.cache/qmd/models` into the sandbox, which for
# the devcontainer user resolves to /home/ubuntu/.cache/qmd/models.
# Symlink that to the prod location so we get the model layer for
# free without copying 2.25 GB into the user's home (a copy would
# mint a whole new layer instead of sharing).
#
# `chmod 0755 /root` makes /root traversable for `ubuntu`. The models
# themselves are world-readable already (curl default umask).
RUN chmod 0755 /root \
    && mkdir -p /home/ubuntu/.cache/bazel-disk \
    && ln -sfn /root/.cache/qmd/models /home/ubuntu/.cache/qmd/models \
    && chown -R ubuntu:ubuntu /home/ubuntu/.cache

# User-level .bazelrc that overrides the workspace .bazelrc's macOS-
# default `LIBCLANG_PATH` with the path Ubuntu's apt install puts
# libclang at. Without this, every bazel invocation inside the
# container would have to remember to pass
# `--action_env=LIBCLANG_PATH=...` on the command line — same friction
# we removed for the CI workflow via bazelargs.
#
# `llvm-config --libdir` is run at image-build time and baked in;
# rebuild the image after a major LLVM bump (e.g. 18 → 19).
RUN ldir="$(llvm-config --libdir)" \
    && printf 'build --action_env=LIBCLANG_PATH=%s\n' "$ldir" > /home/ubuntu/.bazelrc \
    && chown ubuntu:ubuntu /home/ubuntu/.bazelrc

USER ubuntu
WORKDIR /workspaces/mixed_up_files

# Cache warmup: pre-populate bazel's disk cache + output base by running
# an `opt` build of the Rust backend against the workspace source.
# Subsequent contributor builds inside the devcontainer hit the warmed
# caches (boring-sys2, tokio, sqlx, libsqlite3-sys, doltlite, ...) and
# only pay incremental costs for whatever they've edited.
#
# The workspace source is bind-mounted ephemerally via BuildKit
# (`--mount=type=bind`) — it isn't COPYed into a layer. That has two
# good consequences:
#   * Image size doesn't grow by the size of the workspace.
#   * The layer's cache key is driven by the RUN command text alone,
#     not by source contents — so day-to-day source edits between
#     image builds DON'T invalidate this 20-minute warmup step. The
#     warmup re-runs only when the Dockerfile, the parent prod image,
#     or this RUN command itself changes.
#
# `chown 1000:1000` on the bind because the host workspace may be owned
# by whatever the user's uid is — inside the build we always run as the
# `ubuntu` user (uid 1000) and need to traverse + read.
#
# Cache visibility at runtime: `devcontainer.json` mounts named volumes
# at `/home/ubuntu/.cache/bazel` and
# `/home/ubuntu/Library/Caches/bazel-disk-cache`. Docker named volumes
# inherit the image's contents at the mount point on FIRST creation
# (only — subsequent mounts overlay), so a fresh devcontainer gets the
# baked caches automatically; rebuilds reuse whatever the volume
# accumulated during prior sessions.
#
# Skip with `--build-arg WARM_BAZEL_CACHE=false` for fast iteration on
# the Dockerfile itself (or when network is unavailable at build time).
# The build target is the Rust backend explicitly rather than `//...`
# so we don't drag the UI's e2e Playwright machinery into the cache —
# it needs a browser cache the image doesn't ship.
ARG WARM_BAZEL_CACHE=true
RUN --mount=type=bind,source=.,target=/workspaces/mixed_up_files,rw \
    if [ "${WARM_BAZEL_CACHE}" = "true" ]; then \
        bazelisk build //frankweiler/backend/...:all -c opt; \
    fi

# Bazel's output base lives under /home/ubuntu/.cache/bazel; the disk
# cache (~/Library/Caches/bazel-disk-cache on macOS) maps to
# /home/ubuntu/Library/Caches/bazel-disk-cache inside this container —
# see `.bazelrc`'s `build --disk_cache=...`. Override at run time with
# `--disk_cache=...` if you want a different layout.
