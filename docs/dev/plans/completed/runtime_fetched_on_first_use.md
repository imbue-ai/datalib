# The Node runtime, fetched on first use instead of shipped in every tarball

**Status: built (2026-09-18), kept as the record of what was decided.**
How it works now is [`../../runtime_fetch.md`](../../runtime_fetch.md).
Three things landed differently from the design below: the fetched
tree lives in `~/.cache/datalib/runtime/`, not the data root, so two
roots share one copy and the `latchkey` launcher can find it with no
data root in hand; it is named by the asset's sha256 rather than the
datalib version, so presence is the whole check and no stamp file is
needed; and the runtime is built in its own `runtime` job ahead of the
binaries rather than in the same job, because the musl legs must name
an asset their gnu sibling publishes, with its hash. The sizes below
are read off the v0.33.0 and v0.34.1 GitHub releases; the resolver and
model-fetch claims were checked against the tree at `a4262b94`.

## What we have, and what it costs

`qmd` (semantic search) and `latchkey` (credentials) are Node programs.
Since #521 the release tarball carries a `runtime/` directory beside
the binaries: a pinned Node, and the two packages' full `node_modules`
trees as Bazel built them from their lockfiles. The binaries look there
first (`datalib_runtime::node_runtime::runtime_root`), so a user needs
no Node, npm or npx. That replaced `npx -y @tobilu/qmd@2.8.3` at first
use, which pinned only the top-level version, let ~170 transitive
packages float, and ran their install scripts on the user's machine —
the audit's P0 §3.

The safety is right. The packaging is not:

| tarball | v0.33.0 | v0.34.1 |
|---|---|---|
| x86_64 linux-gnu | 59 MB | **507 MB** |
| aarch64 linux-gnu | 58 MB | 169 MB |
| aarch64 macOS | 54 MB | 154 MB |
| musl, both arches | ~60 MB | ~64 MB — no runtime at all |

Inside the x86_64 tarball (1130 MB unpacked), 604 MB is
node-llama-cpp's optional GPU backends — `@node-llama-cpp/linux-x64-cuda-ext`
(352 MB), `linux-x64-cuda` (175 MB), `linux-x64-vulkan` (76 MB) — plus
the `linux-arm64` and `linux-armv7l` CPU packages, which cannot run on
x86 at all. The pnpm tree is copied whole; `stage_runtime.sh` prunes
`typescript` and `playwright` by name and knows nothing else. The docker
image is built from the same tarball, so it carries the same 600 MB.

Two consequences beyond disk. The x86_64 leg's `gh release upload` took
73 minutes for v0.34.1 (the same asset took seconds at 59 MB), and every
Minds boot that installs a full tarball would pull it all. And the musl
tarballs — what Minds actually installs — carry **no** runtime, because
the Bazel Node is a glibc build and a "fully static" tarball must not
carry a binary that needs a libc. With the `npx` fallback off by default
(`DATALIB_ALLOW_NPX`), `qmd` on a v0.34.1 mind has nowhere to resolve
from and refuses to start. Minds is already "fetch after the fact"; it
just has nothing safe to fetch.

## What "safe" means here

The audit did not object to fetching at first use. It objected to what
was fetched and how. A first-use fetch is as safe as the bundle if:

1. **The bytes are pinned before they are fetched.** The binary knows
   the sha256 of what it expects; anything else is refused, not retried
   with looser rules.
2. **Nothing executes during the install.** No `postinstall`, no
   `prebuild-install`, no node-gyp. What arrives is files, unpacked.
3. **A miss fails loudly.** No runtime and no network is an error that
   says what to fetch and from where — never a fall-through to `npx`.
4. **The trust root is ours.** The bytes come from a datalib release
   that our CI built hermetically, not from the npm registry at fetch
   time.

The tree already does all four for qmd's models. `datalib_qmd_models`
holds a pinned table (repo, revision, file, sha256); `ensure_models`
fetches what is missing or wrong at first use, hashes it, refuses a
mismatch, re-fetches a corrupt file, and both the step and the applet
call it before touching qmd. Nobody calls the models "unpinned run-time
fetches", because they are pinned. The runtime can be the same thing.

## The design

### One more asset per triple

`release.yml`'s stage step already builds the runtime tree with
`scripts/stage_runtime.sh`. Instead of folding it into
`datalib-<triple>.tar.gz`, tar it on its own and attach it to the same
release:

```
datalib-<triple>.tar.gz          binaries + latchkey wrapper, ~60 MB again
runtime-<triple>.tar.gz          node/ + qmd/<v>/ + latchkey/<v>/, CPU backends only
runtime-<triple>-cuda.tar.gz     linux-x64 only: the cuda + cuda-ext packages
```

Same stable, un-versioned names as the other assets, so
`releases/latest/download/…` and `releases/download/v<ver>/…` both
work. Each gets its `.sha256` sidecar like the rest.

`stage_runtime.sh` grows a platform filter: after staging, drop every
`@node-llama-cpp/*` package that is not this triple's CPU binding, and
put this triple's GPU bindings (if any) in a second tree for the
`-cuda` asset. Vulkan is dropped outright — it is the backend qmd's
own `catch` exists for, throwing at init on driverless machines — until
someone wants it. The foreign-arch packages go with the same filter.
The existing entry-point assertion stays, and one line is added to it:
`node qmd.js embed "hello"` against a staged tree, so the CPU binding
is proven to load after the prune rather than assumed to.

### The binary knows the hash

The runtime is built in the same job as the binaries, so its sha256 is
not known when the binaries are compiled. Two ways round that; the
first is simpler and is what to build:

- **A manifest beside the binaries.** The stage step writes
  `runtime.sha256` (one line per runtime asset: name, sha256, bytes)
  into the tarball next to the binaries. The resolver reads it from the
  executable's directory, the same place it already looks for
  `runtime/`. The manifest is inside the tarball whose own sha256 the
  installer already verifies, so it is as pinned as the binaries.
- Stamping the hash into the binary (`rustc_env_files`, like
  `DATALIB_VERSION`) would need the runtime built first and the
  binaries second, which reorders the release job for no gain.

### The resolver fetches on a miss

`runtime_root()` today: `DATALIB_RUNTIME_DIR` if set, else `runtime/`
beside the executable, else one level up (the .app), else `None` and
the caller reports `MissingRuntime`. One step goes in before the miss:

```
runtime/ beside the binaries          present → use it (the .app, docker)
<data_root>/system/runtime/<ver>/     present and manifest hash stamped → use it
                                      absent → fetch, verify, unpack, stamp, use it
DATALIB_ALLOW_NPX=1                   as today: loud, opt-in, never a default
```

The fetch is `datalib_qmd_models::download_verified` lifted into a
place both can use — it already does the retry-with-backoff, the
streaming sha256, the temp-file-then-rename, and the "hashed wrong,
refused" path. Unpack into `system/runtime/<ver>.part/`, rename on
success, write a stamp file holding the sha256 so a later start does
not re-hash 100 MB. The `<ver>` in the path is the datalib version: a
new release brings a new runtime, and the old one is pruned the way
`stage_runtime.sh` already prunes stale package trees.

The data root is the right home rather than a user cache dir: it is
per-install, already the one directory datalib owns, already excluded
from cache-aware backups where it should be (`CACHEDIR.TAG` on the
derived trees — `system/runtime/` gets the same tag, it is rebuildable
from the release), and on Minds it is the mind's own volume.

Who triggers it: the same two call sites that call `ensure_models` —
the `qmd_index` step before its first `qmd` shell-out, and the
`unified_index` applet at boot — plus `datalib-step login` and anything
else that runs `latchkey`. Each already tolerates a slow first call
(the models are 300 MB). The first-run screen can say "fetching the
search runtime (80 MB)" the way it says so for the models.

### The GPU asset is opt-in

`runtime-linux-x64-cuda.tar.gz` is fetched only when `QMD_LLAMA_GPU=cuda`
is set in the environment datalib runs qmd with, or when a probe finds
`libcuda.so.1` on the loader path. Unpacked *over* the CPU tree (the
packages are disjoint directories under `.aspect_rules_js/`), after
which qmd's `auto` picks CUDA on its own — that is what
`loadLlamaRuntime` in `third-party/qmd/src/llm.ts` does today with the
bundle. Nobody without an NVIDIA GPU downloads 500 MB to embed a
mailbox; the one person with the GPU keeps it.

Metal needs no asset: `@node-llama-cpp/mac-arm64-metal` is the only mac
binding there is (no CPU-only mac prebuilt exists; qmd runs the Metal
binding with `gpuLayers: 0` when told to stay on CPU), so it stays in
`runtime-aarch64-apple-darwin.tar.gz`.

### Who keeps bundling

- **The .app** keeps `Contents/Resources/runtime/`. It is codesigned
  and notarized as a unit (`datalib/tauri/stage-runtime.sh`), and a
  signed app that fetches unsigned executables into itself is a worse
  story than a 150 MB download. Nothing changes there; the resolver's
  first candidate still finds it.
- **The docker image** unpacks `runtime-<triple>.tar.gz` (CPU) at build
  time, from the same release, into `/opt/datalib/runtime/`. An image
  is a bundle by definition, and the doc-test leg that checks
  `docs/user/docker.md` against the pushed image needs no network at
  run time. The image drops ~600 MB.
- **The tarballs** fetch. This is the change.

### Minds, and musl

The musl tarball is what Minds installs, and it has no runtime because
there is no musl Node. Once the runtime is an asset, the mind can fetch
`runtime-<arch>-unknown-linux-gnu.tar.gz` — the glibc Node runs fine on
the mind's own glibc; "fully static" was a property of the *binaries*
tarball, which stays fully static. The resolver's triple for the
runtime asset is the host's, not the binaries': a musl datalib on a
glibc host fetches the gnu runtime. A musl host (Alpine) has no runtime
to fetch and gets `MissingRuntime` with the reason named, which is
already true today and now says why.

This is what makes the plan worth doing first for Minds rather than
last: v0.34.1 broke `qmd` there, and the fix is the fetch.

## What does not change

- The lockfile-pinned, Bazel-built trees. `stage_runtime.sh` still
  builds them; only what it does with the result moves.
- `DATALIB_RUNTIME_DIR` and the `npx` opt-in, their meanings and their
  warnings.
- The version pins (`LATCHKEY_VERSION`, `DEFAULT_QMD_VERSION`) and
  `//tools:version_pins_test`.
- `scripts/install.sh`: it installs the binaries tarball and nothing
  else; the first run does the rest.

## Order of work

1. `stage_runtime.sh`: the platform filter and the CPU/CUDA split;
   the `embed` smoke. Measure the resulting sizes and put them in this
   file. *(Standalone: shrinks the next release even if nothing else
   lands.)*
2. `release.yml`: the runtime as its own asset(s), the manifest in the
   binaries tarball, the docker job fetching the runtime asset.
3. `datalib_runtime`: the manifest reader and the `system/runtime/`
   candidate. The fetch itself lives one crate up (`datalib_runtime`
   has no dependencies, on purpose — it is a `tools=` input to the
   fixture's embed action, and `reqwest` there would re-embed the
   corpus on every edit). `datalib_qmd_models`' fetch moves to a small
   `datalib_fetch` crate both use; the resolver takes a
   `&dyn Fn(&Manifest) -> Result<PathBuf>` it calls on a miss, and the
   step and applet hand it the fetching one.
4. The two call sites, the first-run message, the `CACHEDIR.TAG`.
5. The Minds env.d unit: nothing to add — the mind's first `qmd`
   fetches — but `datalib-inspiration`'s template prose says
   "needs node.js" and should stop.
6. A test that fails against the old behavior: a tarball-shaped
   directory with binaries, a manifest and no `runtime/`, a local HTTP
   server serving the runtime asset, and a `qmd --version` through the
   resolver. Then the same with a wrong hash, asserting the refusal and
   that nothing was unpacked.

## Open questions

- **Offline first use.** A laptop that installs from the tarball with
  no network gets search-by-SQL but no embedding until it is online
  once — the same as the models today. Say so in the message; there is
  nothing better to do.
- **Where the sha256 for the docker build comes from.** The docker job
  runs after the matrix and fetches by name from the release; it should
  verify against the `.sha256` sidecar the stage step uploaded, the way
  `install.sh` does for the binaries.
- **Vulkan.** Dropped here. If someone on Linux with an AMD GPU wants
  it, it is a third asset and one more line in the probe.
