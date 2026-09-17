# Datalib — production runtime image.
#
# Ships the release tarball whole: the binaries (`datalib-dag`,
# `datalib-step`, `datalib-http`, `datalib-migrate-config`,
# `datalib-applet`, `datalib-doltlite`, `latchkey-curl-dispatch`,
# `latchkey-curl-impersonate`), the bundled `runtime/` (Node plus the
# lockfile-pinned `qmd` and `latchkey` package trees the binaries resolve
# beside themselves) and the `latchkey` launcher — so users register
# services and run syncs from a single container with no Node, npm or
# npx in the image — and a pre-baked demo data library to try first.
#
# Not built from source: this image consumes the per-triple Linux tarballs
# already produced by `.github/workflows/release.yml`. The build context
# must contain:
#
#     dist/amd64/datalib-x86_64-unknown-linux-gnu.tar.gz
#     dist/arm64/datalib-aarch64-unknown-linux-gnu.tar.gz
#
# plus `demo/` (the fixtures + config that datalib/docker/stage_demo.sh
# copies in). `scripts/build_docker.sh` stages all of it locally; the
# docker-publish job in `release.yml` stages the tarballs from the
# just-created GitHub Release and the demo from the checkout.
#
# Bind mounts at runtime (see docs/user/docker.md):
#   /root/.latchkey                latchkey credentials store
#                                  (RW for `auth set` / `services register`;
#                                  may be RO for `datalib-dag`).
#   /data                          the user's data root: config.toml, one
#                                  directory per source, the indexes under
#                                  unified_index/, the server's state
#                                  under system/.
#   /import                        (convention, not required) whatever
#                                  local export a file-backed source
#                                  reads, mounted read-only.
#
# Also baked in: a demo data library at /opt/datalib/demo, ingested at
# build time from the TNG fixtures — see the demo step near the bottom.
#
# The devcontainer (`.devcontainer/Dockerfile`) does NOT build on this
# image: it is its own `ubuntu:24.04` image with the Bazel toolchain,
# published by `devcontainer.yml` independently of releases. The two
# share their Node major and latchkey pin through
# `//tools:version_pins_test`, and nothing else.

FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

# Runtime deps only:
#   ca-certificates  TLS roots for HTTPS calls the binaries make
#   curl             debugging + healthchecks, and the system curl the
#                    dispatch curl execs for hosts that need no
#                    impersonation (the others go to our bundled
#                    `latchkey-curl-impersonate`)
#   tini             PID 1 that reaps zombies and forwards signals; the
#                    Rust binaries don't fork, but the pipeline spawns
#                    the bundled `node` for the qmd indexer and latchkey
#                    and we want SIGTERM to flow through cleanly
#   openssl          used by datalib-entrypoint.sh to generate the
#                    latchkey encryption key on first run
#
# No libsqlite3-0: doltlite is statically linked into the release binary
# via //third-party/doltlite:sqlite3 + the libsqlite3-sys crate
# annotation in MODULE.bazel. If you see "libsqlite3.so: cannot open
# shared object file" at runtime, the sqlx feature in
# datalib/backend/Cargo.toml regressed from `sqlite` to
# `sqlite-unbundled` — see the comment block on the sqlx dep there.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        openssl \
        tini \
    && rm -rf /var/lib/apt/lists/*

# Pre-create the bind-mount targets so empty-dir mounts behave predictably
# and so an unmounted run still has somewhere to write.
RUN mkdir -p /root/.latchkey /data

# Everything below is the release tarball and the entrypoint. A
# contributor in the devcontainer builds the binaries from source with
# Bazel, and `bazelisk build //third-party/doltlite:doltlite` is the
# shell they should use, so
# that it matches the tree they are editing rather than the published
# tag they inherited.

# Stage the matching per-arch tarball. `TARGETARCH` is supplied
# automatically by `docker buildx` (amd64 | arm64). The build context must
# contain `dist/<arch>/datalib-*-linux-gnu.tar.gz` — see the top-of-
# file comment for how it gets staged.
#
# We use a glob in COPY so the caller doesn't have to thread the version
# string through as another ARG; there is exactly one tarball per arch
# dir, named with the workspace version that release.yml just built.
#
# The tarball's top dir becomes /opt/datalib as-is (binaries, `runtime/`,
# the `latchkey` launcher), with symlinks onto PATH: the binaries look
# for `runtime/` beside their canonical path, and the launcher resolves
# its own link before looking. Nothing here is npm-installed.
ARG TARGETARCH
COPY dist/${TARGETARCH}/ /tmp/release/
RUN set -eux; \
    tar -xzf /tmp/release/datalib-*-linux-gnu.tar.gz -C /tmp/release; \
    staged="$(find /tmp/release -maxdepth 1 -type d -name 'datalib-*-linux-gnu' -print -quit)"; \
    test -n "$staged"; \
    mv "$staged" /opt/datalib; \
    rm -rf /tmp/release; \
    for bin in datalib-dag datalib-migrate-config datalib-step datalib-applet \
               datalib-http latchkey-curl-dispatch latchkey-curl-impersonate \
               datalib-doltlite latchkey; do \
        test -x "/opt/datalib/$bin"; \
        ln -sfn "/opt/datalib/$bin" "/usr/local/bin/$bin"; \
    done; \
    # The doltlite shell, for poking at a store by hand:
    #   docker exec -it … doltlite -readonly \
    #       /data/unified_index/grid_index/db.doltlite_db "SELECT * FROM dolt_log();"
    # The tarball's copy links the same amalgamation the pipeline writes
    # with, so the shell and the stores in /data cannot disagree about
    # the format. The bare `doltlite` name stays as a symlink: it is
    # what docs/dev/doltlite.md's recipes type.
    ln -sfn /usr/local/bin/datalib-doltlite /usr/local/bin/doltlite; \
    test -x /opt/datalib/runtime/node/bin/node; \
    # Smoke: --version on the one binary that supports it. datalib-http
    # takes a positional DATA_ROOT and clap rejects --version before parsing
    # anything else, so we can't use it the same way here.
    datalib-dag --version; \
    doltlite --version; \
    /opt/datalib/runtime/node/bin/node --version

# Pre-fetch qmd's three pinned GGUF models (embedding + query-expansion
# + reranker, ~2.25 GB) into qmd's cache (`~/.cache/qmd/models`), each
# from its pinned HuggingFace revision and sha256-verified — the same
# table the `qmd_index` step checks on every sync, so a rebuilt image
# with the same pins lands the same bytes.
#
# `QMD_PREFETCH_MODELS=false` skips it, which is what makes the
# `:<ver>-slim` variant slim — the three GGUFs are 2.2 GB of the image.
# Prod keeps them (default `true`): a first sync would otherwise stall
# on a multi-GB download. The `else` branch still creates the empty
# directory so a container that runs the app writes models to a path
# the image owns.
ARG QMD_PREFETCH_MODELS=true
RUN set -eux; \
    if [ "${QMD_PREFETCH_MODELS}" = "true" ]; then \
        datalib-step pull-models && ls -lh /root/.cache/qmd/models; \
    else \
        echo "QMD_PREFETCH_MODELS=false - skipping the model prefetch"; \
        mkdir -p /root/.cache/qmd/models; \
    fi

# Entrypoint shim that bootstraps `LATCHKEY_ENCRYPTION_KEY` from a
# per-bind-mount key file before exec'ing the user's command. See the
# script header and docs/docker.md for the security model.
COPY entrypoint.sh /usr/local/bin/datalib-entrypoint.sh
RUN chmod 0755 /usr/local/bin/datalib-entrypoint.sh

# --- The demo data library ------------------------------------------
#
# Seven file-backed sources over the Star Trek: TNG fixtures, staged
# into the build context by datalib/docker/stage_demo.sh, ingested and
# rendered here so `datalib-http /opt/datalib/demo` has something to
# show the moment the container starts. docs/user/docker.md walks a
# first-time user through it, and datalib/docker/doc_test.sh checks
# that walkthrough against the built image.
#
# The config's semantic-index step sits below a BUILD-TIME CUT marker
# and is dropped for this run (embedding under the arm64 leg's QEMU
# emulation would cost every release tens of minutes); the shipped
# config keeps it, so the first sync in a container builds the index.
# `--now` is pinned to the fixture's own era so the bookkeeping reads
# in fixture time rather than in build time, like the test fixture.
#
# This layer is also the one smoke test that runs the shipped
# pipeline end to end: a tarball whose datalib-dag or datalib-step
# cannot ingest the fixtures fails the image build here.
COPY demo/ /opt/datalib/demo-sources/
RUN set -eux; \
    mkdir -p /opt/datalib/demo; \
    mv /opt/datalib/demo-sources/config.toml /opt/datalib/demo/config.full.toml; \
    sed '/^# === BUILD-TIME CUT ===/,$d' /opt/datalib/demo/config.full.toml \
        > /opt/datalib/demo/config.toml; \
    datalib-dag /opt/datalib/demo/config.toml --now 2369-04-15T00:00:00+00:00; \
    mv /opt/datalib/demo/config.full.toml /opt/datalib/demo/config.toml; \
    datalib-dag --check /opt/datalib/demo/config.toml; \
    datalib-doltlite -readonly /opt/datalib/demo/unified_index/grid_index/db.doltlite_db \
        "SELECT provider, count(*) FROM grid_rows GROUP BY 1;" \
        | tee /dev/stderr | grep -q .

# LATCHKEY_CURL points latchkey at the dispatch curl (which routes
# marked requests to the impersonator sibling; datalib-step also sets
# this internally via sibling resolution, but exporting it means a user
# running `latchkey curl ...` directly inside the container goes
# through the same curl — add `-H 'X-Imbue-Impersonate: 1'` to make
# such a request impersonate).
ENV LATCHKEY_CURL=/usr/local/bin/latchkey-curl-dispatch

WORKDIR /data

EXPOSE 8731

# `datalib-http` binds 127.0.0.1 by default, which inside a container is
# the container's own loopback — `-p` publishes the container's external
# interface and so can never reach it. Every documented `docker run` had
# to carry `-e DATALIB_BIND=0.0.0.0:8731` to undo that.
#
# This widens nothing outside the container: 0.0.0.0 here is the
# container's network namespace, and what the host exposes is still
# decided entirely by `-p` (`-p 127.0.0.1:8731:8731` stays loopback-only).
# Override for a different port with `-e DATALIB_BIND=0.0.0.0:<port>`.
ENV DATALIB_BIND=0.0.0.0:8731

# Even short-lived commands need `datalib-entrypoint.sh` because
# every documented use case touches latchkey, and latchkey aborts with
# "No encryption key available." if `LATCHKEY_ENCRYPTION_KEY` isn't
# set. That includes:
#   * `latchkey services register` / `auth set` / `auth list` directly;
#   * `datalib-dag` (whose download steps shell out to `latchkey curl`);
#   * `datalib-http`, ditto for any authenticated upstream fetch.
# The entrypoint reads the per-bind-mount key file (or provisions one
# on first run — see the script header) so users don't have to remember
# `-e LATCHKEY_ENCRYPTION_KEY=...` on every `docker run`.
#
# tini in front of it reaps zombie subprocesses (the qmd indexer and
# latchkey run under the bundled `node`) and forwards SIGTERM cleanly to the long-lived
# `datalib-http` case. Cheap enough to keep for short-lived runs too.
#
# No default CMD: this image is multi-entrypoint (`datalib-dag`,
# `datalib-http`, `latchkey`), and forcing one would just make
# `docker run <img> latchkey ...` more awkward.
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/datalib-entrypoint.sh"]
