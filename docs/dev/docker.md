# The Docker image: what is in it and how it is built

The user-facing walkthrough — pull the image, serve the baked-in demo,
ingest a file of your own, put credentials in the container — is
[`docs/user/docker.md`](../user/docker.md). This page is about the
image itself: what it contains, how it is built and published, and the
security model the walkthrough's bind-mount rules rest on.

## What is in the image

`ghcr.io/imbue-ai/datalib:<tag>` is Ubuntu 26.04 plus:

- the release tarball, unpacked whole at `/opt/datalib`, with these
  linked into `/usr/local/bin`: `datalib-pipeline`, `datalib-step`,
  `datalib-http` with the web UI embedded, `datalib-applet`,
  `datalib-migrate-config`, `datalib-doltlite` — also as plain
  `doltlite` — `latchkey-curl-router`, `curl-impersonate` and the
  `latchkey` launcher, with `lib/libdoltlite.so` left in that tree; plus
  the release's `runtime-<triple>.tar.gz`
  asset unpacked beside them as `runtime/` — the Node runtime plus the
  lockfile-pinned `latchkey` package tree the binaries
  resolve beside themselves, checked against its published sha256 at
  build time. There is no Node, npm or npx on the image's `PATH`;
  nothing is fetched from a registry at build or run time, and the first-use fetch
  a tarball install does (`runtime_fetch.md`) never fires here.
  `DATALIB_NO_FETCH=1` is set, so nothing Datalib would otherwise fetch
  for itself (the runtime, the embedding model, a package) is fetched at run
  time: a missing piece is a failed step naming it
  ([`runtime_fetch.md`](runtime_fetch.md) § "When anything may be
  fetched");
- the embedding model (about 330 MB), the one model search loads,
  put in `/root/.cache/qmd/models` by `datalib-step pull-models` at
  build time from its pinned HuggingFace revision and sha256-verified,
  so search by meaning works with nothing fetched. The query-expansion
  and reranker models are pinned too but nothing loads them, so nothing
  fetches or ships them;
- the demo data library at `/opt/datalib/demo`, ingested and rendered
  at image build time from the TNG fixtures under
  `/opt/datalib/demo-sources` (see below);
- `tini` as PID 1 and [`entrypoint.sh`](../../datalib/docker/entrypoint.sh),
  which provisions latchkey's encryption key;
- a user, `datalib` (uid and gid 1000), that everything runs as
  (§ "Permissions and signals").

Two variants are published: the image above, and **`<version>-cuda`**,
the same image with NVIDIA's cuBLAS and Datalib's CUDA module added, for
a machine with an NVIDIA GPU (§ "The CUDA variant"). The devcontainer
CI runs in is a separate image
(`.devcontainer/Dockerfile`, published by `devcontainer.yml`) and does
not build on it.

Published for `linux/amd64` and `linux/arm64` from
[`datalib/docker/Dockerfile`](../../datalib/docker/Dockerfile) by the
`docker-image` and `docker-publish` jobs in
[`.github/workflows/release.yml`](../../.github/workflows/release.yml)
on every `v*` tag: each architecture builds on a runner of its own, with
nothing emulated, and pushes by digest, then one job gives the two a tag.
The image is not built from source: it consumes the
Linux gnu tarballs the same release just produced, so what a user pulls
is byte-for-byte what `curl | sh` installs on a glibc host at or above
the floor ([`release_steps.md`](release_steps.md) §"The Linux floor";
the image's own Ubuntu is newer, which costs nothing). A failed mac
job does not stop the image, so `:latest` can be ahead of the release
([`release_steps.md`](release_steps.md) §"What one failed job costs").
The user's page names the image by digest, not by a tag, and each release
carries that digest as its `docker-image.digest` asset
([`security_posture.md`](security_posture.md)).

## The CUDA variant

`ghcr.io/imbue-ai/datalib:<version>-cuda` (and `latest-cuda`), for
`linux/amd64` only, is what a person with an NVIDIA GPU runs, with
`docker run --gpus all`, and what embedding on Modal runs
([`plans/embed_on_modal.md`](plans/embed_on_modal.md)). On top of the
plain image it holds:

- **`/opt/datalib/lib/libggml-cuda.so`**, the release's
  `ggml-cuda-x86_64-unknown-linux-gnu.tar.gz` asset unpacked, which
  `datalib-step` loads from `lib/` beside itself when there is a GPU
  ([`embedder/README.md`](../../datalib/backend/embedder/README.md)
  § "Measured by hand");
- **cuBLAS 12** (`libcublas.so.12`, `libcublasLt.so.12`) in
  `/usr/local/lib/datalib-cuda/cublas`, on `LD_LIBRARY_PATH`. NVIDIA's
  container runtime passes a container the driver (`libcuda.so.1`) and
  nothing else, so without cuBLAS inside, `--gpus all` would still embed
  on the CPU. It is about 580 MB compressed, which is why the plain
  image goes without.

Both are NVIDIA's or link NVIDIA's code, whose use is governed by the
CUDA Toolkit EULA, not Datalib's MIT license:
[`CUDA-NOTICE.md`](../../third-party/llama_cpp/CUDA-NOTICE.md), which the
image carries in each of those directories' `licenses/`, beside NVIDIA's
own `LICENSE` for the component there (`licenses/nvidia/cudart/`,
`licenses/nvidia/cublas/`), copied from the archive Bazel fetched it in
(`@llama_cpp//:nvidia_licenses`).

**They sit below the release, so that a pull of the next version can
reuse them.** The Dockerfile's `CUDA` build argument picks a stage
between the OS layers and the tarball: `none`, or `12`, which adds the
two archives with `ADD --link`, which builds a layer apart from the
layers under it. A layer is reused only if its digest is the same, and
its digest is the hash of every byte of the layer, directory times
included. So `scripts/release/stage_docker_context.sh` roots each
archive at `/` (`cuda/rootfs-cuda-libs.tar` holds
`usr/local/lib/datalib-cuda/…`, `cuda/rootfs-ggml-cuda.tar` holds
`opt/datalib/…`), every entry dated 1970 as `stage_cuda.sh` dates its
own. Added to a directory instead (`ADD … /opt/datalib/`), BuildKit
makes the destination and its parents itself and stamps them with the
build's clock, so each build gets new digests for the same bytes:
measured with BuildKit v0.34.2, two builds from a cold cache a few
minutes apart gave different layers, over the same Ubuntu and over
different ones, and the rooted archives gave the same two layers both
times (583 MB and 105 MB compressed). The comparison is of what a
registry holds after a push, which is what a pull compares; a `--load`
into the local Docker store says nothing about it.
`.github/workflows/cuda.yml` builds the variant up to its demo
(`--target app`) twice from a cold cache, the second time over Ubuntu
24.04, pushes both to a registry on the runner, and fails if the two
CUDA layers' digests differ (`scripts/release/cuda_layer_digests.py`).
It runs on every change that could move them.
A machine with no GPU runs the variant on the
CPU, as the plain image does: `docker-publish-cuda` checks the
walkthrough against it on a runner that has none.

Published by the `cuda`, `docker-image-cuda` and `docker-publish-cuda`
jobs, apart from the plain image's, so the plain image neither waits for
the CUDA build nor fails with it. Each release carries its digest as
`docker-image-cuda.digest`.

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

The image carries the whole search index: every step in the demo
config runs at image time, `unified_grid_and_search_index`'s keyword pass and each
source's `embed` included, so search by words and by meaning
both answer the moment the container starts.

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

`datalib-pipeline` exists to mirror data out of services you are logged into,
and the credentials that takes are live session cookies and API tokens
that confer the full power of your account. Any process running as you
that can spawn `datalib-pipeline` or read your latchkey store can act as you
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
person has written mounts `/root/.latchkey`, and the image puts the
embedding model in `/root/.cache/qmd/models`, the shared cache
(`datalib_embedding_models::shared_models_dir`) every data root reads
under that home.

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
delivers SIGTERM cleanly to `datalib-pipeline`, which forwards it to its
running steps, and to `datalib-http`, which stops its applets on the
way out.
