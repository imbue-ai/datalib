# The Docker image: what is in it and how it is built

The user-facing walkthrough — pull the image, serve the baked-in demo,
ingest a file of your own, put credentials in the container — is
[`docs/user/docker.md`](../user/docker.md). This page is about the
image itself: what it contains, how it is built and published, and the
security model the walkthrough's bind-mount rules rest on.

## What is in the image

`ghcr.io/imbue-ai/datalib:<tag>` is Ubuntu 26.04 plus:

- the release tarball, unpacked whole at `/opt/datalib`, with these
  linked into `/usr/local/bin`: `datalib-dag`, `datalib-step`,
  `datalib-http` with the web UI embedded, `datalib-applet`,
  `datalib-migrate-config`, `datalib-doltlite` — also as plain
  `doltlite` — `latchkey-curl-router`, `curl-impersonate` and the
  `latchkey` launcher, with `lib/libdoltlite.so` left in that tree; plus
  the release's `runtime-<triple>.tar.gz`
  asset unpacked beside them as `runtime/` — the Node runtime plus the
  lockfile-pinned `qmd` and `latchkey` package trees the binaries
  resolve beside themselves, checked against its published sha256 at
  build time. There is no Node, npm or npx on the image's `PATH`;
  nothing is fetched from a registry at build or run time, and the first-use fetch
  a tarball install does (`runtime_fetch.md`) never fires here.
  `DATALIB_NO_FETCH=1` is set, so nothing datalib would otherwise fetch
  for itself (the runtime, qmd's models, a package) is fetched at run
  time: a missing piece is a failed step naming it
  ([`runtime_fetch.md`](runtime_fetch.md) § "When anything may be
  fetched");
- qmd's three models pre-fetched into `/root/.cache/qmd/models` by
  `datalib-step pull-models` at build time, each from its pinned
  HuggingFace revision and sha256-verified, so a first sync never stalls
  on a multi-gigabyte download;
- the demo data library at `/opt/datalib/demo`, ingested and rendered
  at image build time from the TNG fixtures under
  `/opt/datalib/demo-sources` (see below);
- `tini` as PID 1 and [`entrypoint.sh`](../../datalib/docker/entrypoint.sh),
  which provisions latchkey's encryption key;
- a user, `datalib` (uid and gid 1000), that everything runs as
  (§ "Permissions and signals").

The `:<version>-slim` variant is the same image without the qmd
models. It is amd64 only and not tagged `latest`. It still fetches
nothing: its keyword search works, while its `embed` steps and its
search by meaning fail naming the model they lack. `-e
DATALIB_NO_FETCH=0` lets it fetch the models (pinned, about 2.2 GB) on
first use instead. The devcontainer CI
runs in is a separate image (`.devcontainer/Dockerfile`, published by
`devcontainer.yml`) and does not build on either.

Published for `linux/amd64` and `linux/arm64` from
[`datalib/docker/Dockerfile`](../../datalib/docker/Dockerfile) by the
`docker-image` and `docker-publish` jobs in
[`.github/workflows/release.yml`](../../.github/workflows/release.yml)
on every `v*` tag: each architecture builds on a runner of its own, with
nothing emulated, and pushes by digest, then one job gives the two a tag.
The image is not built from source: it consumes the
Linux gnu tarballs the same release just produced, so what a user pulls
is byte-for-byte what `curl | sh` installs on a glibc host. A failed mac
job does not stop the image, so `:latest` can be ahead of the release
([`release_steps.md`](release_steps.md) §"What one failed job costs").
The user's page names the image by digest, not by a tag, and each release
carries that digest as its `docker-image.digest` asset
([`security_posture.md`](security_posture.md)).

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

The image carries half of the search index. Each source's
`keyword_index` (qmd's BM25 index, a couple of seconds for the whole
demo) runs at image time, so a free-text search answers the moment the
container starts. Its `embed` steps and `qmd_aggregator` sit below a
`BUILD-TIME CUT` marker that the Dockerfile drops for its run and keeps
in the shipped file, so the first sync in a container adds the
embeddings. The slim build has no models to embed with, so in a slim
container those steps fail until the models are let in (above).

The demo config is in the shape `datalib-http` leaves as written, so a
container start does not rewrite it and leave a `config.toml.bak`:
`config_examples_test.rs` in `datalib-step` holds it, and every other
config the repo ships or documents, to that.

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

`scripts/build_docker.sh` stages the build context and runs `docker
buildx`; [`datalib/docker/README.md`](../../datalib/docker/README.md)
§ "Building locally" has its modes. Don't run `docker build` on the
directory directly.

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
- else read `/root/.latchkey/encryption_key` from the bind mount
  (`$LATCHKEY_DIRECTORY` when that is set),
  generating it (`openssl rand -base64 32`, mode `0600`) on the first
  run against that folder;
- if the key is there but this user cannot read it (an earlier image
  wrote it as root), stop and print the `chown` that fixes it;
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

Nothing in the container runs as root. The image's `USER` is
`datalib`, uid and gid 1000, so the parsers that read a stranger's mail
and PDFs hold no more than the folders it owns: the data root, the
credential store, the demo library and its own home. The binaries and
the runtime under `/opt/datalib` are root's and read-only to it.

Its home stays `/root`, which is only a path: every `docker run` a
person has written mounts `/root/.latchkey`, and every data root an
earlier image wrote links its qmd models at `/root/.cache/qmd/models`, a
link the indexer never repoints.

uid 1000 is the first user on most Linux desktops, so a folder such a
user mounts is already writable. Docker Desktop on macOS maps ownership
to the Mac user, and nothing needs doing there. Otherwise the mounted
folders must be writable by uid 1000, which is one command with no
`sudo`, since it runs in the image:

```sh
docker run --rm --user 0 --entrypoint chown -v "$DATA_ROOT:/data" "$IMG" -R 1000:1000 /data
```

The same command brings forward a data root or a credential store an
earlier image wrote as root. The entrypoint looks for that on every
start: it warns when the top three levels of `/data` hold anything it
cannot write (a read-only mount is left alone), and it refuses to start
when the latchkey key is there but unreadable, since going on would mean
a new key that cannot open what is stored. Each message prints that
command. Running the image with `--user "$(id -u):$(id -g)"` instead is
not supported: that user cannot write the image's home, where the file
fingerprint cache lives.

PID 1 is [tini](https://github.com/krallin/tini), so `docker stop`
delivers SIGTERM cleanly to `datalib-dag`, which forwards it to its
running steps, and to `datalib-http`, which stops its applets on the
way out.
