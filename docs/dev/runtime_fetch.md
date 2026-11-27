# The Node runtime: bundled or fetched on first use

The code is
`datalib/backend/runtime/src/node_runtime.rs` (where a runtime is looked
for), `runtime_manifest.rs` (the manifest), `cuda_module.rs` (where the
CUDA module is looked for, § "The CUDA module"), `datalib/backend/fetch/`
(the fetch), `scripts/stage_runtime.sh` (what a runtime holds) and the
`runtime` and `publish-release` jobs in `.github/workflows/release.yml`
(how it is published).

`latchkey` (credentials) is a Node program. Datalib runs it from a
**runtime**: a pinned Node plus latchkey's full `node_modules` tree, and
Modal's JavaScript SDK beside it (`modal/<v>/`, which an `embed` step
imports to embed a source on the person's Modal account:
`docs/dev/plans/embed_on_modal.md`), each built by Bazel from its lockfile so nothing is resolved from the npm
registry at install time and no package's install script ever runs on
a user's machine. Every spawn of it goes through `datalib_runtime::node_runtime::tool_command`,
which finds the runtime in one of three places, in this order:

| where | who |
|---|---|
| `$DATALIB_RUNTIME_DIR` | a dev pointing at a tree `scripts/stage_runtime.sh` staged |
| `runtime/` beside the binaries, or one level up | the docker image (`/opt/datalib/runtime`), the .app (`Contents/Resources/runtime`) |
| `~/.cache/datalib/runtime/<sha12>/`, fetched on first use | a release tarball install |

The fourth option, `DATALIB_ALLOW_NPX=1`, runs latchkey through
`npx -y` from the live registry. It is a dev escape hatch that warns
once per process, so every step log shows it, and is never
a default.

## Why the tarballs fetch

The runtime is the same for every user of a platform. Shipping it inside each binaries tarball took v0.34.1's
x86_64 tarball from 59 MB to 507 MB (the pnpm tree carried
node-llama-cpp's CUDA and Vulkan backends and the foreign-arch packages
along with everything else), made the release's upload take over an
hour, and still left the musl tarballs — what Minds installs — with no
runtime at all, because the Node in it is a glibc build. A first-use
fetch is as safe as a bundle when the bytes are pinned before they are
fetched, nothing executes during the install, a miss fails loudly, and
the bytes come from our own release. The embedding models work the same
way (`datalib_embedding_models`).

## The assets

The `runtime` job of `release.yml` runs `scripts/stage_runtime.sh` (through
`scripts/release/stage_runtime_asset.sh`, the step as a script) on
each host platform, and `publish-release` publishes what it stages:

```
runtime-aarch64-apple-darwin.tar.gz        node/ + latchkey/<v>/ + modal/<v>/
runtime-x86_64-unknown-linux-gnu.tar.gz    likewise
runtime-aarch64-unknown-linux-gnu.tar.gz   likewise
```

each with a `.sha256` sidecar, under the same stable names as every
other asset. The archive holds the tree's contents at its top, so it
unpacks straight into a directory. `stage_runtime.sh` proves the staged
latchkey can import playwright through the staged Node, and the
release step then runs `node --version` from the tree.

The musl tarballs have no runtime of their own: their manifest names
the gnu asset of the same arch, which the glibc host they almost always
run on is fine with. A musl host (Alpine) is refused with the reason
named (`refuse_musl_host` in `datalib/backend/fetch/src/runtime.rs`).

## The manifest

(`git-hash`, the other file beside the binaries, is not the runtime's:
it is the commit they came from, for the log view's source links —
[`logging.md`](logging.md) § "Every line has an author".)

The `build` job writes `runtime.manifest` beside the binaries, reading
each asset's sha256 from its sidecar and its size from the file, in the
`runtime` job's artifact (and the `cuda` job's: § "The CUDA module"). One
asset per line:

```
cpu  runtime-x86_64-unknown-linux-gnu.tar.gz  <sha256> <bytes> https://github.com/…/releases/download/v0.35.0/runtime-x86_64-unknown-linux-gnu.tar.gz
```

The binaries tarball is what `scripts/install.sh` verifies against the
hash it pins, so the manifest is as pinned as the binaries, and the URL
is the release's own, versioned, never `latest`. A checkout build has no
manifest: a miss names the candidates and the fixes.

## The fetch

The two binaries that run latchkey, `datalib-step` (a sync's downloads)
and `datalib-http` (browser sign-in), call
`datalib_fetch::enable_runtime_fetch()` at start, which hands
`datalib_runtime` a `RuntimeFetcher`. On the first `tool_command` that
finds nothing staged, the resolver reads the manifest and calls it —
once per process; a failed fetch is reported, not retried by the next
`latchkey curl` in the same run. The fetcher:

1. takes a blocking `flock` on `~/.cache/datalib/runtime/.lock`, so
   parallel steps of one sync fetch once and the rest wait;
2. downloads the asset to a `.partial` file, hashing as it streams,
   and refuses a mismatch with nothing kept;
3. unpacks it into `<sha12>.part-<pid>/` and renames that to `<sha12>/`
   — the first twelve hex digits of the asset's sha256 — so a directory
   that exists was verified and unpacked whole, and a new release lands
   beside the old one rather than over it;
4. prunes everything else in the cache directory: earlier releases'
   trees, and unpack directories a crash left behind.

The cache directory is `$XDG_CACHE_HOME/datalib/runtime`, else
`~/.cache/datalib/runtime`, beside the embedding model's cache, and carries a
`CACHEDIR.TAG`. A cache directory rather than the data root, because
two data roots on one machine then share one copy and the `latchkey` launcher can find it without a data root in
hand.

The launcher is a shell script and cannot fetch. After the two sibling
candidates it looks in the cache for the one directory its
`runtime.manifest` names. Until a sync or `pull-runtime` has fetched that
one, which includes the time just after an upgrade, when the cache still
holds the previous release's tree, it refuses and names the command to
run. Running the other tree would mean running a different latchkey from
the one this release pins.

`datalib-step pull-runtime` runs the same path ahead of time and prints
the tree it resolved, `node --version` through it, and the latchkey the
tree holds, read from its `package.json` rather than run: `latchkey
--version` looks up its encryption key first, so it fails where there is
none and, on a mac, makes one. `pull-runtime --into <dir>` puts the tree
at `<dir>` instead of the cache, whatever `HOME` is, replacing what was
there in one rename; `<dir>` as `runtime/` beside the binaries, or one
level up, is where they look first. An install made by one user for
another (root installing into a workspace, say) wants this: the cache is
per `HOME`, and the user who runs the sync may not share it. `install.sh`
suggests `pull-runtime`; an offline first sync fails the steps that run
latchkey with the fetch error named and succeeds at everything else, the same as
the models.

## When anything may be fetched

One rule covers every download Datalib makes for itself (the runtime
and the CUDA module here, the embedding model, the perseus embedder's model files, a package): it
is pinned by sha256 before it is fetched, it is fetched on first use
only because the install did not ship it, and a miss fails loudly. All
of them go through `datalib_fetch` (`download_verified`, and
`get_unpinned` for a package's manifest), and that is where the rule is
enforced. An install that ships what it runs says so by setting
`DATALIB_NO_FETCH=1`; every fetch is then refused with an error naming
the URL it wanted and the switch, so a missing piece shows up as a
failed step rather than as a download. The docker image sets it, and every
child process is handed it (`datalib_env`). `DATALIB_MODELS_NO_FETCH`
(once `DATALIB_QMD_MODELS_NO_FETCH`, still read) is the older, narrower
switch: the embedding models only, reported as missing
rather than refused, for a test root that stages the one model it
loads.

## The CUDA module

The x86_64 gnu tarball's manifest has a second line, for llama.cpp's CUDA
backend, which the embedder loads to run on an NVIDIA GPU
(`datalib/backend/embedder/README.md`):

```
cuda ggml-cuda-x86_64-unknown-linux-gnu.tar.gz <sha256> <bytes> https://github.com/…/releases/download/v0.55.0/ggml-cuda-x86_64-unknown-linux-gnu.tar.gz
```

It is the release's `cuda` job's asset (`scripts/release/stage_cuda.sh`):
`lib/libggml-cuda.so`, about 105 MB compressed, and its notices. No other
tarball names one: macOS has Metal built in, and a musl binary cannot
load a glibc library. So every `build` leg of `release.yml` waits for
the `cuda` job, whose asset's sha256 the x86_64 gnu leg writes down.

`datalib_runtime::cuda_module` decides where the module comes from, the
first time a process opens the embedder on the GPU. The embedder does
not link it: a binary hands its `find` to
`datalib_embedder::look_for_cuda_module` at start (`datalib-step`,
`datalib-applet`, `gpu_check`), and one that does not runs on the CPU.

1. `lib/libggml-cuda.so` beside the binary, when it is there: the
   `-cuda` image, or an install that put it there by hand.
2. Else, only where NVIDIA's driver library (`libcuda.so.1`) loads, the
   manifest's `cuda` asset, from `$XDG_CACHE_HOME/datalib/cuda/<sha12>/`
   (`~/.cache/datalib/cuda/`), fetched there first if it is missing.
   The fetch is the runtime's (`datalib_fetch::cached_asset`): a lock,
   a download refused unless it hashes as the manifest says, an unpack
   renamed into place whole, and every other tree in that directory
   removed.
3. Else the CPU, and the step's log line says why: no driver, no
   manifest, no `cuda` line, or the fetch failed.

A laptop or a server without an NVIDIA driver therefore fetches nothing.
A machine with the driver but without cuBLAS 12 fetches the module once
and then cannot load it; the log names the missing library, and the
embed runs on the CPU. `datalib-step` is the one binary that fetches it;
`datalib-applet` loads one already fetched.

`datalib-step pull-cuda` runs the same path ahead of time, whether or not
the machine has the driver, and prints where the module is and whether
the driver loads. `DATALIB_NO_FETCH` refuses this fetch as it does every
other.

## Who still bundles

- **The docker image** unpacks the asset into `/opt/datalib/runtime`
  at build time, after checking its sidecar; the manifest beside the
  binaries is never consulted because the sibling candidate wins.
- **The .app** stages its own with `datalib/tauri/stage-runtime.sh` and
  is signed as a unit. Its JS tree is staged `--no-symlinks`:
  rewritten from the pnpm store layout into npm's flat one by
  `scripts/hoist_node_modules.py`, because Tauri's resource bundler
  copies regular files only and the store layout reaches every package
  through a link — a tree that lost them has every byte and no entry
  script. (Dereferencing the links instead does not work: Node looks
  for a package's dependencies beside where it really lives.)
  `datalib/tauri/check-app.sh`
  runs the bundled `datalib-step pull-runtime` after every `tauri
  build`, so a bundle whose runtime cannot start fails the build
  instead of shipping.

## Adding a platform

A new host platform needs a `runtime` matrix entry in `release.yml` and
its triple in the binaries matrix.

## Measured

Staged on an M-series mac, Node is 108 MB unpacked and the latchkey
tree 11 MB. Each asset's compressed size is on its release's page.
