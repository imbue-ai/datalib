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
# Devcontainer overlap: the entire prefix down through `mkdir -p
# /root/.latchkey /data` (FROM through line ~159) is duplicated
# byte-for-byte in `.devcontainer/Dockerfile`. Docker's content-addressed
# layer cache then dedupes those layers on a single daemon: a
# contributor who has built one image and rebuilds the other pays zero
# bytes of additional disk for apt + Node + latchkey + doltlite CLI +
# the ~2.25 GB qmd model layer. The duplication has to be exact —
# whitespace, ARG defaults, comment text all participate in the
# layer-hash input. When editing the prefix, copy the new lines into
# `.devcontainer/Dockerfile` verbatim or both images will rebuild from
# scratch.

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

# === SHARED-WITH-DEVCONTAINER PREFIX ENDS HERE ===
# Everything below is prod-only. Add new shared bits ABOVE this line and
# mirror them into `.devcontainer/Dockerfile`.

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
