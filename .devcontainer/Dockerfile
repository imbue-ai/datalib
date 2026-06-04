# Devcontainer that mirrors prod runtime + adds the Bazel build toolchain.
#
# Lines 19–143 below are duplicated **byte-for-byte** from
# `frankweiler/docker/Dockerfile`'s shared prefix (everything through
# the `mkdir -p /root/.latchkey /data` instruction). Docker's
# content-addressed layer cache only deduplicates when two images
# produce identical layer hashes, and the hash inputs are the *exact*
# instruction text (whitespace, ARG defaults, the command bytes after
# the `RUN`). When you edit the prefix here, copy the same change into
# `frankweiler/docker/Dockerfile` verbatim, or contributors will rebuild
# 2.25 GB of qmd models twice.
#
# Below the shared prefix (search for `END SHARED PREFIX`) lives the
# devcontainer-only toolchain: build-essential, clang/cmake/lld/llvm
# for compiling Rust + boring-sys2 + libsqlite3-sys, bazelisk, bun,
# sudo, the `ubuntu` non-root user, and a user-level .bazelrc that
# overrides the workspace .bazelrc's macOS-default `LIBCLANG_PATH`.

# === BEGIN SHARED PREFIX (verbatim from frankweiler/docker/Dockerfile) ===

FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# Runtime deps only:
#   ca-certificates  TLS roots for HTTPS calls the binaries make
#   curl             debugging + healthchecks; the `latchkey curl` codepath
#                    invokes our bundled `frankweiler-latchkey-curl-shim`
#                    rather than this, but having curl present is useful
#                    for `docker exec` poking
#   tini             PID 1 that reaps zombies and forwards signals; the
#                    Rust binaries don't fork, but `frankweiler-sync`
#                    spawns `npx`/`node` subprocesses for the qmd indexer
#                    and we want SIGTERM to flow through cleanly
#   gnupg            required by the NodeSource setup script below
#   openssl          used by frankweiler-entrypoint.sh to generate the
#                    latchkey encryption key on first run
#
# No libsqlite3-0: doltlite is statically linked into the release binary
# via //third-party/doltlite:sqlite3 + the libsqlite3-sys crate
# annotation in MODULE.bazel. If you see "libsqlite3.so: cannot open
# shared object file" at runtime, the sqlx feature in
# frankweiler/backend/Cargo.toml regressed from `sqlite` to
# `sqlite-unbundled` — see the comment block on the sqlx dep there.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        gnupg \
        openssl \
        tini \
    && rm -rf /var/lib/apt/lists/*

# Node 22 LTS via NodeSource. Two consumers in this image:
#   1. `latchkey` (installed below) is an npm package.
#   2. `frankweiler-sync` shells out to `npx -y @tobilu/qmd@<version>`
#      to (re)build the qmd search index at sync time. The qmd version
#      is chosen by the binary, not pinned here.
# Keep `NODE_MAJOR` in sync with .devcontainer/Dockerfile.
ARG NODE_MAJOR=22
RUN curl -fsSL "https://deb.nodesource.com/setup_${NODE_MAJOR}.x" | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/* \
    && node --version && npm --version && npx --version

# Pinned latchkey. Bumping: follow the 7-day burn-in policy in README.md
# (no version newer than 7 days at the time of the bump). Latest at pin
# time (2026-06-04) that satisfied the burn-in was 2.12.2 (2026-05-28).
#
# No `latchkey --version` smoke here: that command initializes the
# keyring lazily, and inside a minimal Ubuntu image there's no Secret
# Service running, so latchkey exits non-zero with "No encryption key
# available." The entrypoint script (frankweiler-entrypoint.sh) handles
# this at runtime by setting `LATCHKEY_ENCRYPTION_KEY` before exec.
ARG LATCHKEY_VERSION=2.12.2
RUN npm install -g "latchkey@${LATCHKEY_VERSION}"

# Doltlite CLI — the SQLite-shell analogue for poking at the doltlite
# DB by hand. Lets users run `docker exec -it … doltlite
# /data/backend_index.doltlite_db` and execute `SELECT * FROM
# dolt_log()` etc. against the live store. Doltlite isn't in Ubuntu's
# package repos, but `dolthub/doltlite` publishes signed per-arch
# `.deb` packages on every GitHub release.
#
# Pin to the same version we statically link into the binary
# (//third-party/doltlite + MODULE.bazel's `doltlite_amalgamation`), so
# the SQL surface a user sees in the shell matches what the running
# binary observes — drift here would be confusing without buying
# anything. The `libdoltlite-dev` headers we don't want (no compilation
# happens in this image); the `doltlite` CLI package pulls in its
# `libdoltlite0 (= 0.11.5)` runtime as a hard dep, which is unavoidable
# even though our binary doesn't dynamic-link against it (the .so just
# sits there serving the CLI). Adds ~31 MB to the image — small price
# for a real debug shell against the prolly store.
#
# public-release assets are reachable anonymously, so curl is fine here
# (no gh CLI dep just for this step).
ARG DOLTLITE_CLI_VERSION=0.11.5
RUN arch="$(dpkg --print-architecture)" \
    && base="https://github.com/dolthub/doltlite/releases/download/v${DOLTLITE_CLI_VERSION}" \
    && curl --proto '=https' --tlsv1.2 -fL --retry 5 --retry-delay 4 \
        -o /tmp/libdoltlite0.deb "${base}/libdoltlite0_${DOLTLITE_CLI_VERSION}_${arch}.deb" \
    && curl --proto '=https' --tlsv1.2 -fL --retry 5 --retry-delay 4 \
        -o /tmp/doltlite.deb "${base}/doltlite_${DOLTLITE_CLI_VERSION}_${arch}.deb" \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        /tmp/libdoltlite0.deb \
        /tmp/doltlite.deb \
    && rm -rf /tmp/libdoltlite0.deb /tmp/doltlite.deb /var/lib/apt/lists/* \
    && doltlite --version

# Pre-fetch qmd's three embedding/reranker/expansion models so a fresh
# `frankweiler-sync` run doesn't pay a ~2.25 GB HuggingFace download on
# first use. Layered *before* the release-binary copy so it's cached
# across version bumps — model URIs only change when qmd's defaults
# change (rare), but the binaries change every release.
#
# Model URIs and the `hf_<owner>_<basename>.gguf` cache-filename pattern
# come from `@tobilu/qmd`'s `dist/llm.js` (the `DEFAULT_EMBED_MODEL` /
# `DEFAULT_RERANK_MODEL` / `DEFAULT_GENERATE_MODEL` constants and
# node-llama-cpp's `resolveModelFile` cache layout). If qmd flips a
# default (or the user passes `QMD_EMBED_MODEL=...` to override), the
# baked-in copy is simply ignored and qmd downloads the alternate model
# on first use — no harm done, just no speed-up.
#
# FIXME: Instead of this, we should create some markdown, then qmd add and index it, then check that the files we expect to be created got created.
# Total layer size: ~2.25 GB. Yes, the image is correspondingly chunky;
# the alternative was every fresh `docker run frankweiler-sync` paying a
# multi-minute, network-bound, easily-interrupted download, which is a
# much worse first-run experience than a one-time `docker pull`.
RUN mkdir -p /root/.cache/qmd/models && \
    cd /root/.cache/qmd/models && \
    curl --proto '=https' --tlsv1.2 -fL --retry 5 --retry-delay 4 \
        -o hf_ggml-org_embeddinggemma-300M-Q8_0.gguf \
        "https://huggingface.co/ggml-org/embeddinggemma-300M-GGUF/resolve/main/embeddinggemma-300M-Q8_0.gguf" && \
    curl --proto '=https' --tlsv1.2 -fL --retry 5 --retry-delay 4 \
        -o hf_ggml-org_qwen3-reranker-0.6b-q8_0.gguf \
        "https://huggingface.co/ggml-org/Qwen3-Reranker-0.6B-Q8_0-GGUF/resolve/main/qwen3-reranker-0.6b-q8_0.gguf" && \
    curl --proto '=https' --tlsv1.2 -fL --retry 5 --retry-delay 4 \
        -o hf_tobil_qmd-query-expansion-1.7B-q4_k_m.gguf \
        "https://huggingface.co/tobil/qmd-query-expansion-1.7B-gguf/resolve/main/qmd-query-expansion-1.7B-q4_k_m.gguf" && \
    ls -lh /root/.cache/qmd/models

# Pre-create the bind-mount targets so empty-dir mounts behave predictably
# and so an unmounted run still has somewhere to write.
RUN mkdir -p /root/.latchkey /data

# === END SHARED PREFIX — devcontainer-only layers below ===

# Build toolchain. Mirrors what GHA's ubuntu-latest carries that the
# release.yml workflow assumes available.
#
# build-essential   gcc/g++/make. Ubuntu 24.04's default gcc is 13,
#                   which matches GHA's ubuntu-latest and is the
#                   compiler that surfaced the blake3 SIMD `-march`
#                   issues we hit (see f08151c).
# cmake + perl + pkg-config + libssl-dev
#                   boring-sys2 + openssl-sys build-script deps.
# clang + libclang-dev + llvm
#                   bindgen (libsqlite3-sys's build script) dlopens
#                   libclang to parse <sqlite3.h>. macOS gets clang via
#                   Xcode CLT; on Ubuntu we install it explicitly.
# lld               rustc on Linux still defaults to the deprecated
#                   gold linker and warns; lld present means we can
#                   opt in via RUSTFLAGS/--linkopt if we want to
#                   silence that.
# python3 + python3-pip
#                   `tests/fixtures/build_qmd_index.py` and similar
#                   genrules.
# sudo              passwordless sudo for the `ubuntu` user (below).
# unzip + zip       rules_rust workspace setup occasionally pulls
#                   these in; cheap to keep.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        clang \
        cmake \
        git \
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

# bun — not needed for the bazel build, but qmd's own dev workflow uses
# it (`bun install`, `bun src/cli/qmd.ts <cmd>`) so working inside
# `third-party/qmd/` doesn't need extra setup.
ARG BUN_VERSION=1.1.45
RUN curl -fsSL https://bun.sh/install | bash -s "bun-v${BUN_VERSION}" \
    && mv /root/.bun/bin/bun /usr/local/bin/bun \
    && ln -s /usr/local/bin/bun /usr/local/bin/bunx \
    && rm -rf /root/.bun \
    && bun --version

# bazelisk reads `.bazelversion` and downloads the matching bazel, so we
# only pin bazelisk here.
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

# Non-root user. Ubuntu 24.04's base image already ships an `ubuntu` user
# at uid 1000 — we reuse it (rather than minting `vscode`) so
# host-bind-mounted files don't end up owned by a foreign uid.
RUN usermod -aG sudo ubuntu \
    && echo 'ubuntu ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/ubuntu \
    && chmod 0440 /etc/sudoers.d/ubuntu

# The shared prefix baked the qmd models into /root/.cache/qmd/models
# (owned by root, dir mode inherited from `mkdir -p` = 0755). `.bazelrc`
# bind-mounts `$HOME/.cache/qmd/models` into the sandbox, which for the
# devcontainer user resolves to `/home/ubuntu/.cache/qmd/models`.
# Symlink that to the prod location so we get the model layer for free
# without copying 2.25 GB into the user's home (a copy would mint a
# whole new layer instead of sharing).
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

# Bazel's output base lives under /home/ubuntu/.cache/bazel; the disk
# cache (~/Library/Caches/bazel-disk-cache on macOS) maps to
# /home/ubuntu/.cache/bazel-disk inside this container — see `.bazelrc`'s
# `build --disk_cache=...`. Override at run time with `--disk_cache=...`
# if you want a different layout.
