# The Node runtime: bundled or fetched on first use

**Reference, current as of 2026-09-18.** The code is
`datalib/backend/runtime/src/node_runtime.rs` (where a runtime is looked
for), `runtime_manifest.rs` (the manifest), `datalib/backend/fetch/`
(the fetch), `scripts/stage_runtime.sh` (what a runtime holds) and the
`runtime` job in `.github/workflows/release.yml` (how it is published).
The plan this came from is
[`plans/completed/runtime_fetched_on_first_use.md`](plans/completed/runtime_fetched_on_first_use.md).

`qmd` (semantic search) and `latchkey` (credentials) are Node programs.
datalib runs them from a **runtime**: a pinned Node plus the two
packages' full `node_modules` trees, built by Bazel from their lockfiles
so nothing is resolved from the npm registry at install time and no
package's install script ever runs on a user's machine. Every spawn of
either tool goes through `datalib_runtime::node_runtime::tool_command`,
which finds the runtime in one of three places, in this order:

| where | who |
|---|---|
| `$DATALIB_RUNTIME_DIR` | a dev pointing at a tree `scripts/stage_runtime.sh` staged |
| `runtime/` beside the binaries, or one level up | the docker image (`/opt/datalib/runtime`), the .app (`Contents/Resources/runtime`) |
| `~/.cache/datalib/runtime/<sha12>/`, fetched on first use | a release tarball install |

The fourth option, `DATALIB_ALLOW_NPX=1`, runs the tool through
`npx -y` from the live registry. It is a dev escape hatch that warns on
every use and is never a default.

## Why the tarballs fetch

The runtime is about 100 MB compressed and the same for every user of a
platform. Shipping it inside each binaries tarball took v0.34.1's
x86_64 tarball from 59 MB to 507 MB (the pnpm tree carried
node-llama-cpp's CUDA and Vulkan backends and the foreign-arch packages
along with everything else), made the release's upload take over an
hour, and still left the musl tarballs — what Minds installs — with no
runtime at all, because the Node in it is a glibc build. A first-use
fetch is as safe as a bundle when the bytes are pinned before they are
fetched, nothing executes during the install, a miss fails loudly, and
the bytes come from our own release. qmd's GGUF models already work
that way (`datalib_qmd_models`); the runtime now does too.

## The assets

The `runtime` job of `release.yml` runs `scripts/stage_runtime.sh` on
each host platform and publishes what it stages:

```
runtime-aarch64-apple-darwin.tar.gz        node/ + qmd/<v>/ + latchkey/<v>/
runtime-x86_64-unknown-linux-gnu.tar.gz    likewise, CPU binding only
runtime-x86_64-unknown-linux-gnu-cuda.tar.gz  the two CUDA packages, overlaid on the CPU tree
runtime-aarch64-unknown-linux-gnu.tar.gz
```

each with a `.sha256` sidecar, under the same stable names as every
other asset. The archive holds the tree's contents at its top, so it
unpacks straight into a directory. `stage_runtime.sh` keeps only this
platform's CPU binding of node-llama-cpp (`linux-x64`, `linux-arm64`,
`mac-arm64-metal` — the Metal binding is the only one there is for
Apple Silicon; qmd runs it with GPU offloading off when told to stay on
CPU), moves the CUDA pair to the `-cuda` tree with `--cuda`, drops
Vulkan and the foreign-arch packages, and then proves the binding it
kept still loads by calling `getLlama({build: "never"})` through the
staged Node.

The musl tarballs have no runtime of their own: their manifest names
the gnu asset of the same arch, which the glibc host they almost always
run on is fine with. A musl host (Alpine) is refused with the reason
named, which is the same outcome as before with a better message.

## The manifest

The `build` job writes `runtime.manifest` beside the binaries, reading
each asset's sha256 back from the sidecar the `runtime` job published
and its size from the release. One asset per line:

```
cpu  runtime-x86_64-unknown-linux-gnu.tar.gz       <sha256> <bytes> https://github.com/…/releases/download/v0.35.0/runtime-x86_64-unknown-linux-gnu.tar.gz
cuda runtime-x86_64-unknown-linux-gnu-cuda.tar.gz  <sha256> <bytes> https://…
```

The binaries tarball is what `scripts/install.sh` verifies against its
own `.sha256`, so the manifest is as pinned as the binaries, and the URL
is the release's own, versioned, never `latest`. A checkout build has no
manifest and behaves exactly as before: a miss names the candidates and
the fixes.

## The fetch

`datalib-step`, `datalib-applet` and `datalib-http` call
`datalib_fetch::enable_runtime_fetch()` at start, which hands
`datalib_runtime` a `RuntimeFetcher`. On the first `tool_command` that
finds nothing staged, the resolver reads the manifest and calls it —
once per process; a failed fetch is reported, not retried by the next
`latchkey curl` in the same run. The fetcher:

1. takes a blocking `flock` on `~/.cache/datalib/runtime/.lock`, so
   parallel steps of one sync fetch once and the rest wait;
2. downloads the CPU asset to a `.partial` file, hashing as it streams,
   and refuses a mismatch with nothing kept;
3. unpacks it into `<sha12>.part-<pid>/` and renames that to `<sha12>/`
   — the first twelve hex digits of the asset's sha256 — so a directory
   that exists was verified and unpacked whole, and a new release lands
   beside the old one rather than over it;
4. with a `cuda` entry in the manifest, and `QMD_LLAMA_GPU=cuda` set or
   `libcuda.so.1` on the loader path, overlays the CUDA asset the same
   way and drops a `.cuda-<sha12>` marker in the tree;
5. prunes everything else in the cache directory: earlier releases'
   trees, and unpack directories a crash left behind.

The cache directory is `$XDG_CACHE_HOME/datalib/runtime`, else
`~/.cache/datalib/runtime`, beside qmd's model cache, and carries a
`CACHEDIR.TAG`. A cache directory rather than the data root, which the
plan first proposed, because two data roots on one machine then share
one copy and the `latchkey` launcher can find it without a data root in
hand (it looks there after the two sibling candidates).

`datalib-step pull-runtime` runs the same path ahead of time and prints
the tree it resolved and `qmd --version` through it. `install.sh`
suggests it; an offline first sync fails the `qmd` and `latchkey` steps
with the fetch error named and succeeds at everything else, the same as
the models.

## Who still bundles

- **The docker image** unpacks the CPU asset into `/opt/datalib/runtime`
  at build time, after checking its sidecar; the manifest beside the
  binaries is never consulted because the sibling candidate wins.
- **The .app** stages its own with `datalib/tauri/stage-runtime.sh` and
  is signed as a unit.

## Adding a platform, or a backend

A new host platform needs a `runtime` matrix entry in `release.yml`, a
`case` arm in `stage_runtime.sh` naming its node-llama-cpp binding, and
its triple in the binaries matrix. A new GPU backend (Vulkan, say) is a
second overlay asset: a `cuda`-shaped `AssetKind`, one more line in the
manifest, and a probe in `datalib_fetch::runtime` saying when it is
wanted.

## Measured

Staged on an M-series mac at the pins of 2026-09-18 (Node 108 MB, the
qmd tree 179 MB of which node-llama-cpp's llama.cpp source bundle is 33
MB, latchkey 11 MB): 299 MB unpacked, 102 MB as `.tar.gz`. The Linux
numbers land in the release the first time it runs; expect the x86_64
CPU asset to be near the mac one and the CUDA overlay near 500 MB.
