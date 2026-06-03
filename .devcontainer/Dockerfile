# Ubuntu image that mirrors GHA's `ubuntu-latest` closely enough to
# reproduce the linux-amd64 release build locally. Two motivations:
#
#   1. Iterate on Linux-only build issues (per-file SIMD flags, missing
#      headers, glibc/musl divergences) without paying a ~7-minute
#      push-and-wait CI roundtrip per attempt.
#   2. Catch portability bugs in `//frankweiler/backend:dist` before
#      they reach the release workflow.
#
# Not a perfect match: GitHub-hosted ubuntu-latest images carry a much
# bigger preinstalled-tools surface (azure-cli, dotnet, etc). This
# image is the minimal subset our build actually touches.
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# ── base toolchain ────────────────────────────────────────────────
# build-essential brings gcc/g++/make. Ubuntu 24.04's default gcc is
# 13, which matches what GHA's ubuntu-latest currently ships and is
# the compiler that surfaced the blake3 SIMD `-march` issues we hit
# (see f08151c).
#
# cmake + perl + pkg-config + libssl-dev cover the boring-sys2 +
# openssl-sys build scripts in our crate graph. libclang-dev (plus
# the `clang` driver for completeness) is needed by bindgen, which
# libsqlite3-sys's build script invokes to parse <sqlite3.h>. macOS
# gets clang via Xcode CLT; Ubuntu needs it from apt. python3 is
# used by `tests/fixtures/build_qmd_index.py`. unzip is occasionally
# pulled in by rules_rust workspace setup. lld is included because
# rustc on Linux currently picks the deprecated gold linker by
# default and warns about it (see notes circa 0cc018e); having lld
# present means we can opt into it later via RUSTFLAGS/--linkopt if
# we ever want to silence that warning.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        curl \
        git \
        gnupg \
        libclang-dev \
        libssl-dev \
        lld \
        llvm \
        make \
        perl \
        pkg-config \
        python3 \
        python3-pip \
        sudo \
        unzip \
        zip \
    && rm -rf /var/lib/apt/lists/*

# ── Node.js (for `npx -y @tobilu/qmd@<version>`) ──────────────────
#
# Ubuntu 24.04's apt ships Node 18; NodeSource's 22.x repo gives us a
# current LTS that matches what most contributors run locally. qmd's
# npm install path (node-llama-cpp, sqlite-vec) is happier on 20+.
#
# We could substitute bun's `bunx` via `NPX_BIN=bunx`, but
# `qmd_indexer/src/lib.rs` defaults to `npx` and CI exercises that
# path, so installing real Node keeps the dev/CI surface aligned.
ARG NODE_MAJOR=22
RUN curl -fsSL https://deb.nodesource.com/setup_${NODE_MAJOR}.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/* \
    && node --version && npm --version && npx --version

# ── bun ───────────────────────────────────────────────────────────
#
# Not needed for the bazel build (see Node note above), but qmd's own
# dev workflow uses it (`bun install`, `bun src/cli/qmd.ts <cmd>`), so
# having it on PATH means working inside `third-party/qmd/` doesn't
# need extra setup. Installed system-wide so non-root users get it
# without further ceremony.
ARG BUN_VERSION=1.1.45
RUN curl -fsSL https://bun.sh/install | bash -s "bun-v${BUN_VERSION}" \
    && mv /root/.bun/bin/bun /usr/local/bin/bun \
    && ln -s /usr/local/bin/bun /usr/local/bin/bunx \
    && rm -rf /root/.bun \
    && bun --version

# ── bazelisk (and a `bazel` shim that defers to it) ───────────────
#
# bazelisk reads `.bazelversion` and downloads the matching bazel, so
# we don't pin a bazel version here — only bazelisk itself. The
# project currently uses bazel 9.1.0 (see workspace .bazelversion).
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

# ── non-root user with passwordless sudo ──────────────────────────
#
# Ubuntu 24.04's base image already ships a default `ubuntu` user at
# uid 1000. We reuse it (rather than minting a new `vscode` user) so
# host-mounted files don't end up owned by a foreign uid.
RUN usermod -aG sudo ubuntu \
    && echo 'ubuntu ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/ubuntu \
    && chmod 0440 /etc/sudoers.d/ubuntu

# Pre-create the qmd model cache dir at qmd's own default path
# (matches `.bazelrc`'s --sandbox_add_mount_pair and is what
# `default_models_dir()` returns). Bazel's macOS sandboxing refuses
# to mount a missing path, and we mirror that behavior here so the
# container behaves identically to the host on first build.
RUN mkdir -p /home/ubuntu/.cache/qmd/models /home/ubuntu/.cache/bazel-disk \
    && chown -R ubuntu:ubuntu /home/ubuntu/.cache

# User-level .bazelrc that overrides the workspace .bazelrc's macOS
# LIBCLANG_PATH default with the path Ubuntu's apt install puts
# libclang at. Without this, every bazel invocation inside the
# container would have to remember to pass
# `--action_env=LIBCLANG_PATH=...` on the command line — same
# friction we removed for the CI workflow via bazelargs.
#
# `llvm-config --libdir` is run at image-build time and baked in;
# rebuild the image after a major LLVM bump (e.g. 18 → 19).
RUN ldir="$(llvm-config --libdir)" \
    && printf 'build --action_env=LIBCLANG_PATH=%s\n' "$ldir" > /home/ubuntu/.bazelrc \
    && chown ubuntu:ubuntu /home/ubuntu/.bazelrc

USER ubuntu
WORKDIR /workspaces/mixed_up_files

# Bazel will write its output base under /home/ubuntu/.cache/bazel by
# default; the disk cache (~/Library/Caches/bazel-disk-cache on macOS)
# lives at /home/ubuntu/.cache/bazel-disk inside this container —
# see `.bazelrc` (`build --disk_cache=~/Library/Caches/...`). Override
# at run time with `--disk_cache=...` if you want a different layout.
