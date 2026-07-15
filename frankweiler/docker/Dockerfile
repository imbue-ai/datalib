# Frankweiler — production runtime image.
#
# Ships the three release binaries (`frankweiler-sync`, `frankweiler-http`,
# `frankweiler-latchkey-curl-shim`) plus Node + `latchkey` so users can
# register services and run syncs from a single container.
#
# Not built from source: this image consumes the per-triple Linux tarballs
# already produced by `.github/workflows/release.yml`. The build context
# must contain:
#
#     dist/amd64/frankweiler-x86_64-unknown-linux-gnu.tar.gz
#     dist/arm64/frankweiler-aarch64-unknown-linux-gnu.tar.gz
#
# `scripts/build_docker.sh` stages them locally; the docker-publish job in
# `release.yml` stages them from the just-created GitHub Release.
#
# Bind mounts at runtime (see docs/docker.md):
#   /root/.latchkey                latchkey credentials store
#                                  (RW for `auth set` / `services register`;
#                                  may be RO for `frankweiler-sync`).
#   /data                          data root: config.yaml, the doltlite
#                                  DB, raw_api/ trees, rendered QMDs,
#                                  the qmd index.
#   /root/.cache/qmd/models        (optional) embedding-model cache; avoids
#                                  re-downloading ~300 MB on every fresh
#                                  `frankweiler-sync` run.
#
# Devcontainer: `.devcontainer/Dockerfile` inherits from a published
# version of THIS image via `FROM ghcr.io/imbue-ai/datalib:<tag>`,
# so everything below (apt + Node + latchkey + doltlite CLI + qmd
# models + bind-mount dirs) is carried into the devcontainer for free
# — no Dockerfile mirroring, no double download. The devcontainer
# just adds the Bazel build toolchain (clang/cmake/llvm/lld/bun/
# bazelisk + the `ubuntu` non-root user) on top. When this Dockerfile
# changes in a way the devcontainer should pick up, bump the
# `PROD_IMAGE_TAG` ARG in `.devcontainer/Dockerfile` to the next
# published tag.

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

# Pinned latchkey. The 7-day burn-in policy in README.md does NOT apply
# here: latchkey is a first-party Imbue dependency, so just track latest.
#
# No `latchkey --version` smoke here: that command initializes the
# keyring lazily, and inside a minimal Ubuntu image there's no Secret
# Service running, so latchkey exits non-zero with "No encryption key
# available." The entrypoint script (frankweiler-entrypoint.sh) handles
# this at runtime by setting `LATCHKEY_ENCRYPTION_KEY` before exec.
ARG LATCHKEY_VERSION=2.16.0
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
# `libdoltlite0 (= 0.11.8)` runtime as a hard dep, which is unavoidable
# even though our binary doesn't dynamic-link against it (the .so just
# sits there serving the CLI). Adds ~31 MB to the image — small price
# for a real debug shell against the prolly store.
#
# public-release assets are reachable anonymously, so curl is fine here
# (no gh CLI dep just for this step).
ARG DOLTLITE_CLI_VERSION=0.11.8
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

# Install qmd globally and pre-fetch its three default models
# (embedding + query-expansion + reranker, ~2.25 GB total). `qmd pull`
# is qmd's own model-prefetch command — it reads the canonical model
# URIs out of qmd's source, downloads each one into qmd's default cache
# (`~/.cache/qmd/models`), and writes the matching `.etag` sidecars
# that `qmd doctor` later uses to detect cache corruption. Using `qmd
# pull` rather than curl'ing HuggingFace URLs ourselves means we don't
# have to track qmd's model-URI / cache-naming conventions — qmd bumps
# a default model, we just rebuild and the right files land.
#
# Pinned to the qmd version that `frankweiler-sync` invokes via
# `npx -y @tobilu/qmd@<v>` at runtime — see DEFAULT_QMD_VERSION in
# frankweiler/backend/core/src/qmd/mod.rs. The
# //tools:qmd_version_pins_test asserts the values agree.
#
# qmd 2.5.3 transitively depends on `tree-sitter-javascript`, which
# tries to fetch a prebuilt linux binary via `prebuild-install`. There
# isn't one for our node 22 / glibc combo so npm falls back to
# `node-gyp build`, which needs `make` + a C compiler + `python3` to
# compile the `.node` addon from source. We install those build deps
# here and leave them in the image — purging them after install is
# fragile (NodeSource's nodejs deb has a hard `Depends: python3`, so
# aggressive cleanup pulls node out from under us) and the marginal
# disk-size savings don't matter against the model layer.
#
# Keep this block in sync with `.devcontainer/Dockerfile`.
ARG QMD_VERSION=2.5.3
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        python3 \
    && rm -rf /var/lib/apt/lists/* \
    && npm install -g "@tobilu/qmd@${QMD_VERSION}" \
    && qmd --version \
    && qmd pull \
    && ls -lh /root/.cache/qmd/models

# Pre-create the bind-mount targets so empty-dir mounts behave predictably
# and so an unmounted run still has somewhere to write.
RUN mkdir -p /root/.latchkey /data

# === SHARED-WITH-DEVCONTAINER STATE ENDS HERE ===
# Everything below is prod-only (the release binaries + entrypoint).
# `.devcontainer/Dockerfile` inherits the image at this point and adds
# the Bazel toolchain on top — see the top-of-file note.

# Stage the matching per-arch tarball. `TARGETARCH` is supplied
# automatically by `docker buildx` (amd64 | arm64). The build context must
# contain `dist/<arch>/frankweiler-*-linux-gnu.tar.gz` — see the top-of-
# file comment for how it gets staged.
#
# We use a glob in COPY so the caller doesn't have to thread the version
# string through as another ARG; there is exactly one tarball per arch
# dir, named with the workspace version that release.yml just built.
ARG TARGETARCH
COPY dist/${TARGETARCH}/ /tmp/release/
RUN set -eux; \
    tar -xzf /tmp/release/frankweiler-*-linux-gnu.tar.gz -C /tmp/release; \
    # The tarball's top dir is `frankweiler-<version>-<triple>/`.
    staged="$(find /tmp/release -maxdepth 1 -type d -name 'frankweiler-*-linux-gnu' | head -n1)"; \
    test -n "$staged"; \
    install -m 0755 "$staged/frankweiler-sync"                  /usr/local/bin/frankweiler-sync; \
    install -m 0755 "$staged/frankweiler-http"                  /usr/local/bin/frankweiler-http; \
    install -m 0755 "$staged/frankweiler-latchkey-curl-shim"    /usr/local/bin/frankweiler-latchkey-curl-shim; \
    rm -rf /tmp/release; \
    # Smoke: --version on the one binary that supports it. frankweiler-http
    # takes a positional DATA_ROOT and clap rejects --version before parsing
    # anything else, so we can't use it the same way here.
    frankweiler-sync --version; \
    test -x /usr/local/bin/frankweiler-http; \
    test -x /usr/local/bin/frankweiler-latchkey-curl-shim


# Entrypoint shim that bootstraps `LATCHKEY_ENCRYPTION_KEY` from a
# per-bind-mount key file before exec'ing the user's command. See the
# script header and docs/docker.md for the security model.
COPY entrypoint.sh /usr/local/bin/frankweiler-entrypoint.sh
RUN chmod 0755 /usr/local/bin/frankweiler-entrypoint.sh

# Defaults wired for the documented bind-mount layout:
#   FRANKWEILER_ROOT     resolves the data root without a positional arg
#   LATCHKEY_CURL        points latchkey at the Chrome-impersonating shim
#                        (frankweiler-sync also sets this internally via
#                        sibling resolution, but exporting it means a
#                        user running `latchkey curl ...` directly inside
#                        the container gets CF impersonation too)
ENV FRANKWEILER_ROOT=/data \
    LATCHKEY_CURL=/usr/local/bin/frankweiler-latchkey-curl-shim

WORKDIR /data

EXPOSE 8731

# Even short-lived commands need `frankweiler-entrypoint.sh` because
# every documented use case touches latchkey, and latchkey aborts with
# "No encryption key available." if `LATCHKEY_ENCRYPTION_KEY` isn't
# set. That includes:
#   * `latchkey services register` / `auth set` / `auth list` directly;
#   * `frankweiler-sync`, which shells out to `latchkey curl` per source;
#   * `frankweiler-http`, ditto for any authenticated upstream fetch.
# The entrypoint reads the per-bind-mount key file (or provisions one
# on first run — see the script header) so users don't have to remember
# `-e LATCHKEY_ENCRYPTION_KEY=...` on every `docker run`.
#
# tini in front of it reaps zombie subprocesses (qmd-indexer spawns
# `npx`/`node`) and forwards SIGTERM cleanly to the long-lived
# `frankweiler-http` case. Cheap enough to keep for short-lived runs too.
#
# No default CMD: this image is multi-entrypoint (`frankweiler-sync`,
# `frankweiler-http`, `latchkey`), and forcing one would just make
# `docker run <img> latchkey ...` more awkward.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/frankweiler-entrypoint.sh"]
