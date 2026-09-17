# The image `bazel test //...` runs in on CI, and the devcontainer a
# contributor opens the repo in. Built from `ubuntu:24.04` directly — it
# used to inherit the prod runtime image, which bought it a Node it only
# needs at build time and cost it a publish that could only run after
# a release's prod-image job, so the CI image went unpublished for four
# releases while that job failed on an unrelated doc check (#469).
#
# What the build needs from the host is short, because Bazel brings its
# own Node, Python, Rust, qmd, doltlite and models: a C/C++ toolchain
# and the libraries two build scripts link, `python3` for the fixture
# scripts, `rsync` for the e2e wrapper, the stock `sqlite3` CLI for the
# beeper extract, and Playwright's browsers with their OS libraries.
# Node, latchkey and bun are here for the person, not the build: the
# app falls back to `npx -y latchkey@<pin>` for auth, and qmd's own dev
# loop is `bun`.
#
# Published by `.github/workflows/devcontainer.yml` — on every `v*`
# tag, when this file changes, and on demand — as
# `ghcr.io/imbue-ai/datalib_devcontainer:sha-<commit>`, `:X.Y.Z` and
# `:latest`. It does not wait on any other release job.
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# cmake + perl + pkg-config + libssl-dev   protobuf-src's build script.
# lld               rustc on Linux defaults to the deprecated gold
#                   linker and warns; lld present means we can opt in.
# git               workspace_status.sh, crate fetches that vendor from git.
# rsync             `//datalib/ui:e2e_test`'s wrapper stages the spec
#                   tree with `rsync -aL`.
# sqlite3           the beeper extract shells out to the *stock* CLI
#                   (doltlite misreads stock type codes); libsqlite3-0
#                   alone does not carry it.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        curl \
        git \
        gnupg \
        libsqlite3-0 \
        libssl-dev \
        lld \
        openssl \
        perl \
        pkg-config \
        python3 \
        python3-pip \
        rsync \
        sqlite3 \
        unzip \
        zip \
    && rm -rf /var/lib/apt/lists/*

# Node, the same major the prod image ships, and latchkey at the same
# pin: `//tools:version_pins_test` holds both in step with
# `datalib/docker/Dockerfile`. Playwright's installer below needs `npx`;
# everything else that runs Node under `bazel test` uses Bazel's own.
ARG NODE_MAJOR=22
RUN curl -fsSL "https://deb.nodesource.com/setup_${NODE_MAJOR}.x" | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/* \
    && node --version && npm --version
ARG LATCHKEY_VERSION=3.11.0
RUN npm install -g "latchkey@${LATCHKEY_VERSION}" && npm cache clean --force

# bun — qmd's own dev workflow (`bun install`, `bun src/cli/qmd.ts`).
ARG BUN_VERSION=1.4.0
RUN curl -fsSL https://bun.sh/install | bash -s "bun-v${BUN_VERSION}" \
    && mv /root/.bun/bin/bun /usr/local/bin/bun \
    && ln -s /usr/local/bin/bun /usr/local/bin/bunx \
    && rm -rf /root/.bun \
    && bun --version

# Playwright's browsers + their OS libraries, for `//datalib/ui:e2e_test`.
# Both engines the suite drives: Chromium runs the bulk of it, WebKit the
# grid-bearing specs in the engine the Tauri app uses (WKWebView), where
# a collapsed AG Grid has shipped twice while every Chromium assertion
# passed. `run_e2e.sh` calls `playwright install` at test time, but
# `--with-deps` needs root and a package index, which a test action has
# neither of — so both engines are baked here and that call is a no-op.
#
# PLAYWRIGHT_VERSION MUST match the resolved `playwright` version in
# `datalib/ui/pnpm-lock.yaml` (`//tools:version_pins_test`); a mismatch
# means Bazel's Playwright looks for a browser revision that isn't here.
#
# The cache lands at /root/.cache/ms-playwright. GitHub forces
# HOME=/github/home for container steps, so `test.yml` points the suite
# at it with `--test_env=PLAYWRIGHT_BROWSERS_PATH`.
ARG PLAYWRIGHT_VERSION=1.62.1
RUN npx -y "playwright@${PLAYWRIGHT_VERSION}" install --with-deps chromium webkit \
    && rm -rf /var/lib/apt/lists/* /root/.npm

# uv — `//:lint` drives ruff / pyright through it. The static binary from
# astral's image: no curl-to-bash, one tag to bump.
COPY --from=ghcr.io/astral-sh/uv:0.11.20 /uv /uvx /bin/

# bazelisk reads `.bazelversion` and downloads the matching bazel. Its
# download cache is pinned to a path outside $HOME so the bazel binary
# baked below is found whatever HOME a CI runner forces.
ARG BAZELISK_VERSION=1.29.0
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
ENV BAZELISK_HOME=/opt/bazel/bazelisk

# Root, deliberately: no uid dance for the workspace bind mount on
# Docker Desktop (VirtioFS maps uids back to the host user), and CI
# container steps run as root regardless.
WORKDIR /workspaces/datalib

# The pre-fetched output base, for CI.
#
# A warm CI run spent most of its bazel time before the first action:
# every external repository — the Rust toolchain, rustfmt, Python, Node,
# ~300 crates — downloaded into an empty container and, mostly,
# extracted on 4 vCPUs (~77 s to "Analyzed"; the same analysis takes
# ~10 s with the repositories on disk). Fetching them here, into an
# output base at a fixed path, puts them in the image instead:
# `prepare-bazel` points CI's bazel at the same paths, the repository
# marker files validate, and nothing is fetched again.
#
# Both paths have to be pinned, not just the output base. Bazel 9 keeps
# fetched repositories in a contents cache under the output *user* root
# and symlinks `external/<repo>` into it, and that root defaults to
# `$HOME/.cache/bazel` — `/root` here, `/github/home` on a CI runner.
# With only the output base pinned, every entry looked absent from the
# runner's cache and was fetched again (measured: 157 downloads, 50 s,
# on the first invocation of a run). The downloaded archives are
# dropped afterwards — 0.7 GB the image would carry for nothing, since
# the extracted contents are what a run uses and a repository added
# after the build downloads its own. A bump to a
# toolchain or lockfile between image builds just refetches that delta,
# which is why lockfile changes do not rebuild the image and the tag
# build at each release is what keeps the delta small.
#
# The workspace is bind-mounted for the fetch, not COPYed, so the image
# does not carry the source and this layer's key is the command text
# (plus the mounted files it reads). `--batch` because BuildKit's RUN
# container is too small for a resident bazel server's JVM.
# `--lockfile_mode=error` so a stale MODULE.bazel.lock fails the image
# build here rather than every CI run that pulls it.
#
# Off by default: a local devcontainer keeps bazel's default output
# base under /root/.cache/bazel, which `devcontainer.json` puts on a
# named volume; only the CI publish passes `PREFETCH_OUTPUT_BASE=true`.
ARG PREFETCH_OUTPUT_BASE=false
RUN --mount=type=bind,source=.,target=/workspaces/datalib,rw \
    if [ "${PREFETCH_OUTPUT_BASE}" = "true" ]; then \
        bazelisk --batch --host_jvm_args=-Xmx2048m \
            --output_user_root=/opt/bazel/user-root \
            --output_base=/opt/bazel/output-base \
            fetch --lockfile_mode=error //... \
        && rm -rf /opt/bazel/output-base/execroot /opt/bazel/output-base/action_cache \
            /opt/bazel/user-root/cache/repos/v1/content_addressable; \
    else \
        bazelisk version; \
    fi

# Cache warmup for a LOCAL devcontainer: resolve and fetch the backend's
# external repositories into the default output base (the named volume
# in `devcontainer.json` inherits the image's contents on first
# creation), then compile it in opt mode into the disk cache, so the
# first interactive build skips straight to what was edited. Skip with
# `--build-arg WARM_BAZEL_CACHE=false` when iterating on this file.
ARG WARM_BAZEL_CACHE=true
RUN --mount=type=bind,source=.,target=/workspaces/datalib,rw \
    if [ "${WARM_BAZEL_CACHE}" = "true" ]; then \
        bazelisk --batch --host_jvm_args=-Xmx1024m fetch //datalib/backend/...:all \
        && bazelisk --batch --host_jvm_args=-Xmx4096m build //datalib/backend/...:all -c opt; \
    fi

ENTRYPOINT []
CMD ["bash"]
