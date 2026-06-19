# Devcontainer that inherits from the prod runtime image and adds the
# Bazel build toolchain on top. The prod image already carries
# `ubuntu:24.04` + Node 22 + latchkey + the doltlite CLI + qmd's three
# default GGUFs at /root/.cache/qmd/models + the bind-mount target
# directories, so inheriting from it gives the devcontainer all of that
# "for free" — no Dockerfile mirroring, no double download. Contributors
# who've already pulled the prod image during `docker pull
# ghcr.io/imbue-ai/mixed_up_files:<ver>` (or anyone who ran
# `scripts/build_docker.sh --load`) reuse the cached layers; a fresh
# `devcontainer up` only needs to fetch the dev-only build toolchain
# layers added below.
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
ENTRYPOINT []
CMD []

ENV DEBIAN_FRONTEND=noninteractive

# Build toolchain that prod doesn't ship. Items already present in
# prod (build-essential, python3, sudo-not-needed, etc) are NOT
# repeated here — apt would happily skip them, but listing only the
# deltas makes the dev-vs-prod surface visible.
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
# rsync             `//frankweiler/ui:e2e_test`'s wrapper (run_e2e.sh)
#                   stages the Playwright spec tree via `rsync -aL`.
#                   macOS ships rsync; Ubuntu's base image here does not.
# sqlite3           the beeper provider's extract shells out to the
#                   *stock* `sqlite3` CLI (it can't link rusqlite — see
#                   beeper/src/extract/index_db.rs — and doltlite
#                   misreads stock SQLite type codes). macOS ships
#                   /usr/bin/sqlite3; on Ubuntu the runtime lib
#                   (libsqlite3-0) does NOT carry the CLI, so the
#                   `//tests/fixtures:ingested_tng` genrule failed with
#                   "spawn sqlite3: No such file or directory" until
#                   this was added.
# unzip + zip       rules_rust workspace setup occasionally pulls
#                   these in; cheap to keep.
RUN apt-get update && apt-get install -y --no-install-recommends \
        clang \
        cmake \
        git \
        libclang-dev \
        libsqlite3-0 \
        libssl-dev \
        lld \
        llvm \
        perl \
        pkg-config \
        python3-pip \
        rsync \
        sqlite3 \
        unzip \
        zip \
    && rm -rf /var/lib/apt/lists/* \
    # Symlink the unversioned SONAME that the (currently) buggy
    # exec-config frankweiler_sync_bin's DT_NEEDED expects. The
    # `--host_linkopt=-Wl,--as-needed` we added to .bazelrc fixes the
    # source of the bug, but until BuildBuddy's cache is reseeded with
    # post-fix actions, the warmup will still pull a stale cached
    # exec-config tool with the phantom dep. The symlink lets it run;
    # once the cache reseeds, the symlink becomes a dead pointer
    # (harmless).
    && for d in /usr/lib/*-linux-gnu; do \
            [ -f "${d}/libsqlite3.so.0" ] || continue; \
            ln -sf libsqlite3.so.0 "${d}/libsqlite3.so"; \
       done

# bun — not needed for the bazel build, but qmd's own dev workflow
# uses it (`bun install`, `bun src/cli/qmd.ts <cmd>`) so working
# inside `third-party/qmd/` doesn't need extra setup.
ARG BUN_VERSION=1.1.45
RUN curl -fsSL https://bun.sh/install | bash -s "bun-v${BUN_VERSION}" \
    && mv /root/.bun/bin/bun /usr/local/bin/bun \
    && ln -s /usr/local/bin/bun /usr/local/bin/bunx \
    && rm -rf /root/.bun \
    && bun --version

# Playwright Chromium + its OS libraries, for `//frankweiler/ui:e2e_test`.
# The test resolves the browser from `~/.cache/ms-playwright` via
# `env_inherit = ["HOME"]` (HOME=/root here) but never runs
# `playwright install` itself, so the browser must be baked in. `npx
# playwright@<ver> install --with-deps chromium` both apt-installs the
# shared libs Chromium needs and downloads the browser build that this
# exact Playwright version expects.
#
# PLAYWRIGHT_VERSION MUST match the resolved `playwright` /
# `@playwright/test` version in `frankweiler/ui/pnpm-lock.yaml` — a
# mismatch means the bazel-linked Playwright looks for a browser
# revision that isn't on disk. Bump both together.
#
# NOTE: until a new image is published (on the next `v*` tag), CI still
# excludes e2e_test from the gate — see the FIXME in
# `.github/workflows/test.yml`. This bake is what lets that exclusion be
# lifted.
ARG PLAYWRIGHT_VERSION=1.59.1
RUN npx -y "playwright@${PLAYWRIGHT_VERSION}" install --with-deps chromium \
    && rm -rf /var/lib/apt/lists/*

# uv — `precommit_test` drives ruff / pyright through `uv run`, so the
# image needs `uv` on PATH for the in-bazel pre-commit check to work
# in CI. Pull the static binary from astral's official image rather
# than running their install script: no curl-to-bash, no networked
# install step, and bumping the version is a single tag edit.
COPY --from=ghcr.io/astral-sh/uv:0.11.20 /uv /uvx /bin/

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

# Run the devcontainer as `root` (inherited from the prod image's USER).
# That removes a whole class of friction:
#
#   * No symlink-shenanigans to make qmd's pre-baked GGUFs at
#     /root/.cache/qmd/models reachable from a different `$HOME` —
#     `.bazelrc`'s `--sandbox_add_mount_pair=$(HOME)/.cache/qmd/models`
#     just resolves to /root/.cache/qmd/models directly.
#   * No `chmod 0755 /root` to grant a non-root user traversal access
#     to the models directory.
#   * No `chown`-on-named-volume dance in postCreateCommand.
#
# Tradeoff: files the contributor creates via the workspace bind mount
# are owned by uid 0. On Docker-Desktop-on-macOS that's fine — VirtioFS
# maps container uids back to the host user. On a native Linux host
# (rare for devcontainer users today), the tree would need `sudo` to
# clean up.

# User-level .bazelrc that overrides the workspace .bazelrc's macOS-
# default `LIBCLANG_PATH` with the path Ubuntu's apt install puts
# libclang at. Without this, every bazel invocation inside the
# container would have to remember to pass
# `--action_env=LIBCLANG_PATH=...` on the command line — same friction
# we removed for the CI workflow via bazelargs.
#
# `llvm-config --libdir` is run at image-build time and baked in;
# rebuild the image after a major LLVM bump (e.g. 18 → 19).
#
# Both `--action_env` and `--host_action_env` are written so exec-config
# ("[for tool]") bindgen build scripts — e.g. boring-sys2 when an
# exec-config tool in `//...` pulls in the HTTP stack — find libclang
# too. `--action_env` alone only covers target-config actions; the
# missing host variant panicked bindgen with "Unable to find libclang"
# on a cold BuildBuddy cache. See the matching note in the workspace
# .bazelrc.
RUN ldir="$(llvm-config --libdir)" \
    && printf 'build --action_env=LIBCLANG_PATH=%s\nbuild --host_action_env=LIBCLANG_PATH=%s\n' "$ldir" "$ldir" > /root/.bazelrc

WORKDIR /workspaces/mixed_up_files

# Cache warmup: pre-populate bazel's repository cache by running
# `bazelisk fetch` against the workspace source. `fetch` resolves the
# bzlmod graph + every external repo + crate_universe dependency tree
# (the genuinely cold-and-slow parts of a first build) and writes the
# results into the repository cache and external repo dirs. It does
# NOT compile anything — which means we skip ~20 min of boring-sys2 /
# tokio / sqlx Rust compilation that would otherwise need to fit
# inside BuildKit's RUN container.
#
# Subsequent contributor `bazelisk build` invocations inside the
# devcontainer reuse the resolved repo state and skip straight to
# action execution — the long pole on cold builds is the external
# dep fetches (BoringSSL git clones, crate downloads) not the
# compile, so this captures most of the warm-cache win.
#
# The workspace source is bind-mounted ephemerally via BuildKit
# (`--mount=type=bind`) — it isn't COPYed into a layer. That has two
# good consequences:
#   * Image size doesn't grow by the size of the workspace.
#   * The layer's cache key is driven by the RUN command text alone,
#     not by source contents — so day-to-day source edits between
#     image builds DON'T invalidate this warmup step. The warmup
#     re-runs only when the Dockerfile, the parent prod image, or
#     this RUN command itself changes.
#
# Cache visibility at runtime: `devcontainer.json` mounts named volumes
# at `/root/.cache/bazel` and `/root/Library/Caches/bazel-disk-cache`.
# Docker named volumes inherit the image's contents at the mount point
# on FIRST creation (only — subsequent mounts overlay), so a fresh
# devcontainer gets the baked caches automatically; rebuilds reuse
# whatever the volume accumulated during prior sessions.
#
# Skip with `--build-arg WARM_BAZEL_CACHE=false` for fast iteration on
# the Dockerfile itself (or when network is unavailable at build time).
# Target is the Rust backend specifically — `//...` would drag in the
# Vue e2e Playwright machinery which needs a browser cache the image
# doesn't ship.
ARG WARM_BAZEL_CACHE=true
# `--batch` runs bazel single-shot without a persistent server daemon.
# The normal server-mode startup involves a JVM that allocates ~1.5 GB
# heap and listens on a unix socket; under BuildKit's RUN container
# (which inherits Docker Desktop's modest memory budget), the JVM gets
# OOM-killed before it can finish listening, and the bazel client
# times out with "couldn't connect to server after 120 seconds".
# Batch mode bypasses all of that — each invocation is a one-shot
# fork.
#
# `--host_jvm_args=-Xmx<N>m` caps the JVM heap so an under-resourced
# environment doesn't trigger the same OOM. 1 GB is plenty for a
# fetch (no compilation) and stays well under BuildKit's slice;
# the build step downstream gets more headroom since it has to hold
# the action graph.

# Step 1: fetch — resolves bzlmod graph + crate_universe + every
# external repo (BoringSSL git clone, doltlite amalgamation download,
# rules_rust toolchains, etc.), writes them into
# /root/.cache/bazel/_bazel_root/<hash>/external/.
RUN --mount=type=bind,source=.,target=/workspaces/mixed_up_files,rw \
    if [ "${WARM_BAZEL_CACHE}" = "true" ]; then \
        bazelisk --batch --host_jvm_args=-Xmx1024m fetch //frankweiler/backend/...:all; \
    fi

# Step 2: build — compiles the Rust backend in opt mode and lands
# every action's output in the disk cache. With step 1's external
# deps already warm, this step is just compilation; the contributor's
# first interactive `bazel build` hits the disk cache and skips
# straight to whatever they've actually edited. Heap bumped to 4 GB
# because the action graph + analysis caches get hefty.
RUN --mount=type=bind,source=.,target=/workspaces/mixed_up_files,rw \
    if [ "${WARM_BAZEL_CACHE}" = "true" ]; then \
        bazelisk --batch --host_jvm_args=-Xmx4096m build //frankweiler/backend/...:all -c opt; \
    fi

# Bazel's output base lives under /root/.cache/bazel; the disk cache
# (~/Library/Caches/bazel-disk-cache on macOS) maps to
# /root/Library/Caches/bazel-disk-cache inside this container — see
# `.bazelrc`'s `build --disk_cache=...`. Override at run time with
# `--disk_cache=...` if you want a different layout.
