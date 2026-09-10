# The Docker image: what is in it and how it is built

The user-facing walkthrough — pull the image, serve the baked-in demo,
ingest a file of your own, put credentials in the container — is
[`docs/user/docker.md`](../user/docker.md). This page is about the
image itself: what it contains, how it is built and published, and the
security model the walkthrough's bind-mount rules rest on.

## What is in the image

`ghcr.io/imbue-ai/datalib:<tag>` is Ubuntu 24.04 plus:

- every binary from the release tarball (`datalib-dag`, `datalib-step`,
  `datalib-http` with the web UI embedded, `datalib-applet`,
  `datalib-migrate-config`, `datalib-doltlite` — also as plain
  `doltlite` — and the two `latchkey-curl-*` binaries), installed under
  `/usr/local/bin`;
- Node 22, the pinned `latchkey` CLI, and the pinned `qmd` with its
  three models pre-fetched into `/root/.cache/qmd/models`, so a first
  sync never stalls on a multi-gigabyte download;
- the demo data library at `/opt/datalib/demo`, ingested and rendered
  at image build time from the TNG fixtures under
  `/opt/datalib/demo-sources` (see below);
- `tini` as PID 1 and [`entrypoint.sh`](../../datalib/docker/entrypoint.sh),
  which provisions latchkey's encryption key.

The `:<version>-slim` variant is the same image without the qmd
models; the devcontainer builds on it. It is amd64 only and not tagged
`latest`.

Published for `linux/amd64` and `linux/arm64` from
[`datalib/docker/Dockerfile`](../../datalib/docker/Dockerfile) by the
`docker-publish` job in
[`.github/workflows/release.yml`](../../.github/workflows/release.yml)
on every `v*` tag. The image is not built from source: it consumes the
Linux tarballs the same release just produced, so what a user pulls is
byte-for-byte what `curl | sh` installs.

## The demo library

[`datalib/docker/demo/config.toml`](../../datalib/docker/demo/config.toml)
names seven file-backed sources — Claude export, a Gmail `.mbox`,
Google Takeout, SMS Backup & Restore, LinkedIn, vCard contacts, PDFs —
whose inputs are the same TNG fixtures the test suite uses.
[`stage_demo.sh`](../../datalib/docker/stage_demo.sh) copies them into
the build context, and a `RUN` step in the Dockerfile ingests, renders
and grid-indexes them. That run is also the one end-to-end smoke test
of the shipped pipeline: a tarball whose binaries cannot ingest the
fixtures fails the image build.

The semantic index is deliberately not built at image time. The
config's `qmd_index` step sits below a `BUILD-TIME CUT` marker that the
Dockerfile drops for its run and keeps in the shipped file, so the
first sync in a container builds it. Embedding under the arm64 leg's
QEMU emulation would add tens of minutes to every release for an index
that takes about a minute natively.

## Checking the walkthrough against an image

[`datalib/docker/doc_test.sh`](../../datalib/docker/doc_test.sh)
executes every shell block in `docs/user/docker.md` that carries a
`<!-- doc-test: run <name> -->` marker, in page order, against one
image, then checks what each should have produced: the demo serves and
answers a search, the index builds, a mounted `.mbox` ingests, the
stores read back. The page's setup block is swapped for a free port, a
temp data root, and the TNG mbox fixture. So the page cannot drift from
the image without the test saying so.

It is `manual` in Bazel (it needs the host's docker daemon and a
registry pull) and release.yml runs it against every image it pushes:

```sh
# a published tag
bazelisk test //datalib/docker:doc_test --test_env=DATALIB_DOCKER_IMAGE=ghcr.io/imbue-ai/datalib:0.31.0

# an image you just built and loaded (below), from a checkout, no bazel
DATALIB_DOCKER_IMAGE=ghcr.io/imbue-ai/datalib:latest datalib/docker/doc_test.sh
```

## Building locally

```sh
# Both arches against the latest tagged release, into the buildx cache
# only: a "does it still build?" smoke.
scripts/build_docker.sh

# Same, but load the host-native arch into the local daemon so you can
# `docker run` it.
scripts/build_docker.sh --load

# Against tarballs you built yourself (bazel build //datalib/backend:dist
# on a Linux host, or the musl cross configs), named like the release's.
scripts/build_docker.sh --tarball-dir /path/to/tarballs --load

# Push to your own registry.
REPO=your-fork/datalib IMAGE_NAME=ghcr.io/your-fork/datalib scripts/build_docker.sh --push
```

Don't run `docker build` on the directory directly: the Dockerfile
expects `dist/<arch>/*.tar.gz` and `demo/` in its context, and
`build_docker.sh` is what stages them.

## Security model

`datalib-dag` exists to mirror data out of services you are logged into,
and the credentials that takes are live session cookies and API tokens
that confer the full power of your account. Any process running as you
that can spawn `datalib-dag` or read your latchkey store can act as you
on those services with no further prompt.

The image is here so those binaries and those credentials never have
to land in your host shell. The container sees only what you
bind-mount, so the blast radius is exactly the folders you map in.
That only holds if you stick to the walkthrough's mount table:
mounting `$HOME`, or running with `--privileged`, defeats the point.

### Latchkey's encryption key is provisioned inside the bind mount

On a host install latchkey keeps its encryption key in the OS keyring.
The container has no keyring, so
[`entrypoint.sh`](../../datalib/docker/entrypoint.sh) does this on every
start:

- if `LATCHKEY_ENCRYPTION_KEY` is already in the environment, use it
  (for ephemeral, no-persistence runs);
- else read `/root/.latchkey/encryption_key` from the bind mount,
  generating it (`openssl rand -base64 32`, mode `0600`) on the first
  run against that folder;
- if the folder is read-only and has no key, warn and use an ephemeral
  key, so credentials written that run are unreadable next run.

The key lives beside the blobs it protects, which is roughly a desktop
Linux box with an auto-unlocked keyring: it protects against disclosure
of `credentials.json.enc` alone and not against someone who can read
both files. Keep the host folder mode 700 and treat it as the file of
bearer tokens it effectively is. Moving the folder moves the
credentials; losing it loses them.

A store populated on the host cannot be read in the container as-is,
because its key is in the host keyring. `latchkey auth re-encrypt`
bridges that: it rewrites chosen services under a key read from stdin,
into a destination directory, which is how the walkthrough gets a
browser-login credential (Slack, Gmail, GitHub, Fastmail) into the
container. The browser flows themselves cannot run inside the image,
which has no browser.

## Permissions and signals

The container runs as root so that `/root/.latchkey` and `/data` have
a predictable owner. On a Linux host, files written into bind mounts
are therefore owned by `uid 0`; Docker Desktop on macOS maps them to
your user. `--user "$(id -u):$(id -g)"` is possible but moves `HOME`,
and with it the pre-baked model cache and latchkey's store, so prefer a
`chown` afterwards.

PID 1 is [tini](https://github.com/krallin/tini), so `docker stop`
delivers SIGTERM cleanly to `datalib-dag`, which forwards it to its
running steps, and to `datalib-http`, which stops its applets on the
way out.
