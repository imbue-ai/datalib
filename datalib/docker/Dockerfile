# Datalib — production runtime image.
#
# Ships the release tarball whole — the binaries (`datalib-dag`,
# `datalib-step`, `datalib-http`, `datalib-migrate-config`,
# `datalib-applet`, `datalib-doltlite`, `latchkey-curl-router`,
# `curl-impersonate`) and the `latchkey` launcher — plus the
# release's runtime asset unpacked beside them as `runtime/` (Node plus
# the lockfile-pinned `qmd` and `latchkey` package trees the binaries
# resolve beside themselves), so users register services and run syncs
# from a single container with no Node, npm or npx in the image and
# nothing fetched at run time — and a pre-baked demo data library to
# try first.
#
# Not built from source: this image consumes the per-triple Linux assets
# already produced by `.github/workflows/release.yml`. The build context
# must contain, per arch:
#
#     dist/amd64/datalib-x86_64-unknown-linux-gnu.tar.gz
#     dist/amd64/runtime-x86_64-unknown-linux-gnu.tar.gz         (+ .sha256)
#     dist/arm64/datalib-aarch64-unknown-linux-gnu.tar.gz
#     dist/arm64/runtime-aarch64-unknown-linux-gnu.tar.gz        (+ .sha256)
#
# plus `demo/` (the fixtures + config that datalib/docker/stage_demo.sh
# copies in). `scripts/build_docker.sh` stages all of it locally; the
# docker-image job in `release.yml` stages its architecture's assets from
# that run's artifacts and the demo from the checkout.
#
# Everything runs as `datalib`, uid and gid 1000, whose home is /root
# (see the user step below). Bind mounts at runtime (see
# docs/user/docker.md), which uid 1000 must be able to write:
#   /root/.latchkey                latchkey credentials store
#                                  (RW for `auth set` / `services register`;
#                                  may be RO for `datalib-dag`).
#   /data                          the user's data root: config.toml, one
#                                  directory per source, the indexes under
#                                  unified_document_index/, the server's state
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

FROM ubuntu:26.04

ENV DEBIAN_FRONTEND=noninteractive

# Runtime deps only:
#   ca-certificates  TLS roots for HTTPS calls the binaries make
#   curl             debugging + healthchecks, and the system curl the
#                    router curl execs for hosts that need no
#                    impersonation (the others go to our bundled
#                    `curl-impersonate`)
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

# The app parses whatever a person's mail and files hold, so nothing in
# the container runs as root: everything runs as `datalib`, uid and gid
# 1000, the id the first user on most Linux desktops has, so a folder
# such a user mounts is already writable. Ubuntu's stock `ubuntu` user
# holds that id and goes.
#
# Its home stays /root, a path and nothing more: every `docker run` a
# person has written mounts `/root/.latchkey`, and every data root an
# earlier image wrote links its qmd models at `/root/.cache/qmd/models`
# (a link the indexer never repoints).
#
# The bind-mount targets are made here so empty-dir mounts behave
# predictably and an unmounted run still has somewhere to write.
RUN set -eux; \
    if id ubuntu >/dev/null 2>&1; then userdel --remove ubuntu; fi; \
    if getent group ubuntu >/dev/null; then groupdel ubuntu; fi; \
    groupadd --gid 1000 datalib; \
    useradd --uid 1000 --gid 1000 --home-dir /root --no-create-home \
        --shell /bin/bash datalib; \
    mkdir -p /root/.latchkey /root/.cache /data; \
    chown datalib:datalib /root /root/.latchkey /root/.cache /data; \
    chmod 0755 /root
ENV HOME=/root

# Everything below is the release tarball and the entrypoint. A
# contributor in the devcontainer builds the binaries from source with
# Bazel, and `bazelisk build //third-party/doltlite:doltlite` is the
# shell they should use, so
# that it matches the tree they are editing rather than the published
# tag they inherited.

# Stage the matching per-arch tarball and runtime. `TARGETARCH` is
# supplied automatically by `docker buildx` (amd64 | arm64). The build
# context must contain `dist/<arch>/datalib-*-linux-gnu.tar.gz` and
# `dist/<arch>/runtime-*-linux-gnu.tar.gz` with its `.sha256` — see the
# top-of-file comment for how they get staged.
#
# We use a glob in COPY so the caller doesn't have to thread the version
# string through as another ARG; there is exactly one tarball per arch
# dir, named with the workspace version that release.yml just built.
#
# The tarball's top dir becomes /opt/datalib as-is (binaries, the
# `latchkey` launcher), with symlinks onto PATH, and the runtime asset
# — checked against the sha256 the release published beside it —
# unpacks into /opt/datalib/runtime/: the binaries look for `runtime/`
# beside their canonical path before consulting `runtime.manifest`,
# and the launcher resolves its own link before looking. Nothing here
# is npm-installed, and nothing is fetched when the container runs.
ARG TARGETARCH
COPY dist/${TARGETARCH}/ /tmp/release/
RUN set -eux; \
    tar --no-same-owner -xzf /tmp/release/datalib-*-linux-gnu.tar.gz -C /tmp/release; \
    staged="$(find /tmp/release -maxdepth 1 -type d -name 'datalib-*-linux-gnu' -print -quit)"; \
    test -n "$staged"; \
    mv "$staged" /opt/datalib; \
    (cd /tmp/release && sha256sum -c runtime-*-linux-gnu.tar.gz.sha256); \
    mkdir /opt/datalib/runtime; \
    tar --no-same-owner -xzf /tmp/release/runtime-*-linux-gnu.tar.gz -C /opt/datalib/runtime; \
    rm -rf /tmp/release; \
    for bin in datalib-dag datalib-migrate-config datalib-step datalib-applet \
               datalib-http latchkey-curl-router curl-impersonate \
               datalib-doltlite latchkey; do \
        test -x "/opt/datalib/$bin"; \
        ln -sfn "/opt/datalib/$bin" "/usr/local/bin/$bin"; \
    done; \
    # The doltlite shell, for poking at a store by hand:
    #   docker exec -it … doltlite -readonly \
    #       /data/<group>/ingest/entities.doltlite_db "SELECT * FROM dolt_log();"
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
    /opt/datalib/runtime/node/bin/node --version; \
    # The demo library is the one place under /opt the app writes.
    install -d -o datalib -g datalib /opt/datalib/demo

# The binaries and the runtime above stay root's and read-only to the
# app; from here on, everything runs as the app's user.
USER datalib

# qmd's embedding model (about 330 MB), the one model search loads,
# from its pinned HuggingFace revision and sha256-verified into qmd's
# cache (`/root/.cache/qmd/models`) — the same table the `embed` step and
# the search applet check, so a rebuilt image with the same pins lands
# the same bytes, and search by meaning works with nothing fetched.
RUN datalib-step pull-models && ls -lh /root/.cache/qmd/models

# Nothing is fetched at run time: what the image ships is all it runs.
# Every first-use fetch datalib makes (qmd's model, the Node runtime, a
# package) is refused under this switch, naming what was missing.
# `-e DATALIB_NO_FETCH=0` lets it fetch (docs/dev/docker.md).
ENV DATALIB_NO_FETCH=1

# Entrypoint shim that bootstraps `LATCHKEY_ENCRYPTION_KEY` from a
# per-bind-mount key file before exec'ing the user's command. See the
# script header and docs/docker.md for the security model.
COPY --chmod=0755 entrypoint.sh /usr/local/bin/datalib-entrypoint.sh

# --- The demo data library ------------------------------------------
#
# Seven file-backed sources over the Star Trek: TNG fixtures, staged
# into the build context by datalib/docker/stage_demo.sh, ingested and
# rendered here so `datalib-http /opt/datalib/demo` has something to
# show the moment the container starts. docs/user/docker.md walks a
# first-time user through it, and datalib/docker/doc_test.sh checks
# that walkthrough against the built image.
#
# Every step runs, the search index's keyword index and embeddings
# included, so search by words and by meaning both work the moment the
# demo starts and every step on the Manage screen has run.
# No `--now`: the run record is stamped at build time, so the
# Dashboard's "Synced …" reads as a real past time.
#
# This layer is also the one smoke test that runs the shipped
# pipeline end to end: a tarball whose datalib-dag or datalib-step
# cannot ingest the fixtures fails the image build here.
COPY --chown=datalib:datalib demo/ /opt/datalib/demo-sources/
RUN set -eux; \
    mv /opt/datalib/demo-sources/config.toml /opt/datalib/demo/config.toml; \
    datalib-dag /opt/datalib/demo/config.toml; \
    datalib-dag --check /opt/datalib/demo/config.toml; \
    test -s /opt/datalib/demo/unified_document_index/qmd_aggregator/qmd/index.sqlite; \
    datalib-doltlite -readonly /opt/datalib/demo/unified_document_index/grid_index/db.sqlite \
        "SELECT provider, count(*) FROM grid_rows GROUP BY 1;" \
        | tee /dev/stderr | grep -q .

# LATCHKEY_CURL points latchkey at the router curl (which hands
# marked requests to the impersonator sibling; datalib-step also sets
# this internally via sibling resolution, but exporting it means a user
# running `latchkey curl ...` directly inside the container goes
# through the same curl — add `-H 'X-Imbue-Impersonate: 1'` to make
# such a request impersonate).
ENV LATCHKEY_CURL=/usr/local/bin/latchkey-curl-router

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
